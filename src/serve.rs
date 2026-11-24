//! `muster serve`: the web app in `web/`, the live snapshots `--json --follow` prints, a send
//! action, and a switch between Tern, herdr and Orca, over HTTP. Built for a phone that reaches
//! this Mac through `tailscale serve` (HTTPS inside the tailnet) or the local network. Every API
//! request needs the token stored in muster's config folder; the app's own files do not, so it
//! can load and ask for it. Only the watched window's watcher runs.

use std::any::Any;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::backend::Backend;

pub const LISTEN: &str = "127.0.0.1:7878";
/// A stream with nothing new writes an empty line this often, so proxies keep it open and a
/// closed client is noticed.
const KEEPALIVE: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 64 << 10;
const MAX_TEXT: usize = 8 << 10;
/// Open connections, streams included; more are refused.
const MAX_CONNECTIONS: usize = 64;

/// Served without the token. The icons and font are the notch app's.
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "/",
        "text/html; charset=utf-8",
        include_bytes!("../web/index.html"),
    ),
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/app.js"),
    ),
    (
        "/style.css",
        "text/css; charset=utf-8",
        include_bytes!("../web/style.css"),
    ),
    (
        "/sw.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/sw.js"),
    ),
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../web/manifest.webmanifest"),
    ),
    (
        "/icon-256.png",
        "image/png",
        include_bytes!("../notch/MusterNotch/Assets.xcassets/AppIcon.appiconset/icon_256.png"),
    ),
    (
        "/icon-512.png",
        "image/png",
        include_bytes!("../notch/MusterNotch/Assets.xcassets/AppIcon.appiconset/icon_512.png"),
    ),
    (
        "/DepartureMono-Regular.otf",
        "font/otf",
        include_bytes!("../notch/MusterNotch/Resources/DepartureMono-Regular.otf"),
    ),
];

/// The latest snapshot, numbered so each stream sends only newer ones.
#[derive(Default)]
struct Latest {
    seq: u64,
    json: Arc<String>,
    /// Addresses of open agents: the only ones the app may send to.
    targets: HashSet<String>,
}

/// The watched window. Replacing `guard` stops the old watcher; `generation` tells a stopped
/// watcher's snapshot thread that its snapshots are stale.
struct Watcher {
    backend: Backend,
    generation: u64,
    guard: Option<Box<dyn Any + Send>>,
}

struct Shared {
    latest: Mutex<Latest>,
    changed: Condvar,
    watcher: Mutex<Watcher>,
    history: u64,
    token: String,
    connections: AtomicUsize,
}

impl Shared {
    fn new(backend: Backend, history: u64, token: String) -> Self {
        Shared {
            latest: Mutex::default(),
            changed: Condvar::new(),
            watcher: Mutex::new(Watcher {
                backend,
                generation: 0,
                guard: None,
            }),
            history,
            token,
            connections: AtomicUsize::new(0),
        }
    }
}

/// `choice` is `tern`, `herdr`, `orca` or `auto`; `auto` takes the window last chosen in the app,
/// else the one `--backend auto` finds.
pub fn run(choice: &str, history: u64, listen: &str) -> Result<()> {
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("--listen needs an address and port, not {listen}"))?;
    let saved = std::fs::read_to_string(config_dir()?.join(BACKEND_FILE)).ok();
    let choice = match saved.as_deref().map(str::trim) {
        Some(saved) if choice == "auto" && BACKENDS.contains(&saved) => saved,
        _ => choice,
    };
    let backend = Backend::resolve(choice)?;
    let token = token()?;
    let listener = TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
    let shared = Arc::new(Shared::new(backend.clone(), history, token));
    watch(&shared, backend.clone());

    println!("muster serve: {} on http://{addr}", backend.name());
    println!("token: {}", shared.token);
    println!("open:  http://{addr}/#token={}", shared.token);
    if !addr.ip().is_loopback() {
        println!(
            "warning: plain HTTP on {addr}; the token and messages cross the network unencrypted"
        );
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if shared.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let _ = handle(stream, &shared);
            shared.connections.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

const BACKENDS: [&str; 3] = ["tern", "herdr", "orca"];
/// The window last chosen in the app, kept across restarts.
const BACKEND_FILE: &str = "serve-backend";

/// Stops the current watcher, if any, and watches `backend`. Until its history is loaded, the
/// app gets an empty snapshot marked `loading`.
fn watch(shared: &Arc<Shared>, backend: Backend) {
    let mut watcher = shared.watcher.lock().unwrap();
    watcher.generation += 1;
    let generation = watcher.generation;
    // The old watcher stops before the next starts, so two never run at once.
    watcher.guard = None;
    let name = backend.name();
    publish(
        shared,
        &json!({"backend": name, "agents": [], "messages": [], "loading": true}),
    );
    let (tx, rx) = mpsc::channel();
    watcher.guard = Some(backend.spawn(shared.history, tx));
    watcher.backend = backend;
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let result = crate::snapshots(&rx, name, |snapshot| {
            // Held while publishing, so a switch cannot land between the check and the publish.
            let watcher = shared.watcher.lock().unwrap();
            if watcher.generation != generation {
                bail!("replaced");
            }
            publish(&shared, &snapshot);
            Ok(())
        });
        if shared.watcher.lock().unwrap().generation == generation
            && let Err(error) = result
        {
            eprintln!("muster serve: {error:#}");
            std::process::exit(1);
        }
    });
}

/// Switches to `{"backend": "tern" | "herdr" | "orca"}` and saves the choice.
fn switch(shared: &Arc<Shared>, body: &[u8]) -> (&'static str, Value) {
    let request = serde_json::from_slice::<Value>(body).unwrap_or_default();
    let Some(choice) = request["backend"].as_str().filter(|c| BACKENDS.contains(c)) else {
        return (
            "400 Bad Request",
            json!({"error": "backend must be tern, herdr or orca"}),
        );
    };
    let backend = match Backend::resolve(choice) {
        Ok(backend) => backend,
        Err(error) => {
            return (
                "500 Internal Server Error",
                json!({"error": format!("{error:#}")}),
            );
        }
    };
    if shared.watcher.lock().unwrap().backend.name() != choice {
        watch(shared, backend);
    }
    if let Err(error) =
        config_dir().and_then(|dir| Ok(std::fs::write(dir.join(BACKEND_FILE), choice)?))
    {
        eprintln!("muster serve: cannot save the window: {error:#}");
    }
    ("200 OK", json!({"ok": true, "backend": choice}))
}

fn publish(shared: &Shared, snapshot: &Value) {
    let targets = snapshot["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["open"] == true)
        .filter_map(|a| match a["address"].as_str() {
            Some("") | None => a["pane"].as_u64().map(|p| p.to_string()),
            Some(address) => Some(address.to_string()),
        })
        .collect();
    let mut latest = shared.latest.lock().unwrap();
    latest.seq += 1;
    latest.json = Arc::new(snapshot.to_string());
    latest.targets = targets;
    shared.changed.notify_all();
}

fn config_dir() -> Result<PathBuf> {
    Ok(
        match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => {
                PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config")
            }
        }
        .join("muster"),
    )
}

/// The token in `$XDG_CONFIG_HOME/muster/serve-token` (else `~/.config/…`), made on first use.
/// Delete the file to make a new one.
fn token() -> Result<String> {
    let dir = config_dir()?;
    let file = dir.join("serve-token");
    if let Ok(token) = std::fs::read_to_string(&file) {
        let token = token.trim();
        if token.len() >= 32 {
            return Ok(token.to_string());
        }
    }
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&file)
        .and_then(|mut f| f.write_all(token.as_bytes()))
        .with_context(|| format!("cannot write {}", file.display()))?;
    Ok(token)
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    length: usize,
}

fn read_head(reader: &mut impl BufRead) -> Result<Request> {
    let mut line = String::new();
    let mut size = 0;
    // Each read stops one byte past the head's remaining budget, so a line with no newline
    // cannot grow without bound before the size check.
    let mut next = |line: &mut String| -> Result<()> {
        line.clear();
        let budget = (MAX_HEAD - size) as u64 + 1;
        size += reader.by_ref().take(budget).read_line(line)?;
        if size > MAX_HEAD {
            bail!("request head too large");
        }
        Ok(())
    };
    next(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        bail!("bad request line");
    };
    let mut request = Request {
        method: method.to_string(),
        path: target.split(['?', '#']).next().unwrap_or("/").to_string(),
        authorization: None,
        length: 0,
    };
    loop {
        next(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            request.authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            request.length = value.parse().context("bad content-length")?;
        }
    }
    Ok(request)
}

fn handle(stream: TcpStream, shared: &Arc<Shared>) -> Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let request = read_head(&mut reader)?;
    let api = request.path.starts_with("/api/");
    if api && !authorized(&request, &shared.token) {
        return respond(
            &mut out,
            "401 Unauthorized",
            &json!({"error": "bad or missing token"}),
        );
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/stream") => stream_snapshots(&mut out, shared),
        ("POST", "/api/send" | "/api/backend") => {
            if request.length > MAX_BODY {
                return respond(
                    &mut out,
                    "413 Payload Too Large",
                    &json!({"error": "too large"}),
                );
            }
            let mut body = vec![0; request.length];
            reader.read_exact(&mut body)?;
            let (status, reply) = if request.path == "/api/send" {
                send(shared, &body)
            } else {
                switch(shared, &body)
            };
            respond(&mut out, status, &reply)
        }
        (_, _) if api => respond(
            &mut out,
            "404 Not Found",
            &json!({"error": "no such endpoint"}),
        ),
        ("GET" | "HEAD", path) => match ASSETS.iter().find(|(p, ..)| *p == path) {
            Some((_, kind, body)) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                out.write_all(head.as_bytes())?;
                if request.method == "GET" {
                    out.write_all(body)?;
                }
                Ok(())
            }
            None => respond(&mut out, "404 Not Found", &json!({"error": "not found"})),
        },
        _ => respond(
            &mut out,
            "405 Method Not Allowed",
            &json!({"error": "method not allowed"}),
        ),
    }
}

fn authorized(request: &Request, token: &str) -> bool {
    let Some(given) = request
        .authorization
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer "))
    else {
        return false;
    };
    // Compare in constant time, so the response time does not reveal a matching prefix.
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn respond(out: &mut TcpStream, status: &str, body: &Value) -> Result<()> {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    out.write_all(head.as_bytes())?;
    out.write_all(body.as_bytes())?;
    Ok(())
}

/// Sends `{"backend", "address", "text"}` to an open agent through the backend, as `--send`
/// does. `backend` is the window the app showed: after a switch, from this or another client, an
/// address may name another window's pane, so a send for another window is refused.
fn send(shared: &Shared, body: &[u8]) -> (&'static str, Value) {
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return ("400 Bad Request", json!({"error": "body is not JSON"}));
    };
    let (Some(window), Some(address), Some(text)) = (
        request["backend"].as_str(),
        request["address"].as_str(),
        request["text"].as_str(),
    ) else {
        return (
            "400 Bad Request",
            json!({"error": "needs backend, address and text"}),
        );
    };
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_TEXT {
        return (
            "400 Bad Request",
            json!({"error": "text must be 1 to 8192 bytes"}),
        );
    }
    // Same lock order as `watch` and `publish`: the target is checked against the snapshot of
    // the backend that sends, with no switch in between.
    let backend = {
        let watcher = shared.watcher.lock().unwrap();
        if watcher.backend.name() != window {
            return (
                "409 Conflict",
                json!({"error": format!("muster now watches {}, not {window}", watcher.backend.name())}),
            );
        }
        if !shared.latest.lock().unwrap().targets.contains(address) {
            return ("409 Conflict", json!({"error": "that agent is not open"}));
        }
        watcher.backend.clone()
    };
    match backend.send(address, text) {
        Ok(()) => ("200 OK", json!({"ok": true})),
        Err(error) => ("502 Bad Gateway", json!({"error": format!("{error:#}")})),
    }
}

/// Newline-delimited JSON snapshots in HTTP chunks: the latest one now, then each newer one.
fn stream_snapshots(out: &mut TcpStream, shared: &Shared) -> Result<()> {
    out.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )?;
    let mut sent = 0;
    loop {
        let next = {
            let latest = shared.latest.lock().unwrap();
            let (latest, _) = shared
                .changed
                .wait_timeout_while(latest, KEEPALIVE, |l| l.seq == sent)
                .unwrap();
            (latest.seq != sent).then(|| {
                sent = latest.seq;
                Arc::clone(&latest.json)
            })
        };
        match next {
            Some(json) => chunk(out, &[json.as_bytes(), b"\n"])?,
            None => chunk(out, &[b"\n"])?,
        }
    }
}

fn chunk(out: &mut TcpStream, parts: &[&[u8]]) -> Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    out.write_all(format!("{len:x}\r\n").as_bytes())?;
    for part in parts {
        out.write_all(part)?;
    }
    out.write_all(b"\r\n")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(authorization: Option<&str>) -> Request {
        Request {
            method: "GET".into(),
            path: "/api/stream".into(),
            authorization: authorization.map(str::to_string),
            length: 0,
        }
    }

    #[test]
    fn only_the_exact_bearer_token_is_authorized() {
        let token = "ab".repeat(24);
        assert!(authorized(
            &request(Some(&format!("Bearer {token}"))),
            &token
        ));
        assert!(!authorized(
            &request(Some(&format!("Bearer {}", &token[1..]))),
            &token
        ));
        assert!(!authorized(&request(Some(&token)), &token));
        assert!(!authorized(&request(None), &token));
    }

    #[test]
    fn head_keeps_path_without_query_and_reads_headers() {
        let raw =
            "POST /api/send?x=1 HTTP/1.1\r\nAUTHORIZATION: Bearer t\r\nContent-Length: 12\r\n\r\n";
        let head = read_head(&mut raw.as_bytes()).unwrap();
        assert_eq!(
            (
                head.method.as_str(),
                head.path.as_str(),
                head.authorization.as_deref(),
                head.length
            ),
            ("POST", "/api/send", Some("Bearer t"), 12)
        );
    }

    #[test]
    fn a_head_line_without_end_is_refused_at_the_limit() {
        // Endless bytes with no newline: an unbounded read would never return.
        let mut endless = BufReader::new(std::io::repeat(b'a'));
        let error = read_head(&mut endless).err().unwrap();
        assert_eq!(error.to_string(), "request head too large");
    }

    #[test]
    fn only_open_agents_are_send_targets() {
        let shared = Shared::new(
            Backend::Tern {
                bin: "/nonexistent".into(),
            },
            0,
            String::new(),
        );
        publish(
            &shared,
            &json!({"agents": [
                {"pane": 7, "address": "", "open": true},
                {"pane": 8, "address": "term_x", "open": true},
                {"pane": 9, "address": "term_closed", "open": false},
            ]}),
        );
        let targets = shared.latest.lock().unwrap().targets.clone();
        assert_eq!(targets.len(), 2);
        assert!(targets.contains("7") && targets.contains("term_x"));
        let send = |body: &[u8]| send(&shared, body).1["error"].as_str().map(str::to_string);
        assert_eq!(
            send(br#"{"backend": "tern", "address": "term_closed", "text": "hi"}"#).as_deref(),
            Some("that agent is not open")
        );
        // An open address, but from a composer that showed another window.
        assert_eq!(
            send(br#"{"backend": "orca", "address": "term_x", "text": "hi"}"#).as_deref(),
            Some("muster now watches tern, not orca")
        );
    }
}
