//! Reads Orca's terminals, agents and mail over the running app's local socket: one connection,
//! polled every second. Orca has no event stream on that socket, and its CLI starts a Node
//! process per call, so muster speaks the CLI's protocol itself: one JSON request per line with
//! the token from `orca-runtime.json`, one JSON response per line. This protocol is Orca's own,
//! not a documented API. Sending and focusing use the `orca` CLI, once per action.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::herdr::pane_id;
use crate::model::PaneId;
use crate::pane::Pane;
use crate::source::Update;
use crate::transcript::Event;

const APP_BINARY: &str = "/Applications/Orca.app/Contents/Resources/bin/orca";
const POLL: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(5);
/// Mail read per poll, newest first; older mail stays as first read.
const MAIL_LIMIT: u32 = 500;

/// `ORCA_BIN`, then `orca` on `PATH`, then the app bundle's.
pub fn binary() -> String {
    crate::herdr::find("ORCA_BIN", "orca", Some(APP_BINARY.into()))
}

/// The runtime's metadata: `$ORCA_RUNTIME_FILE`, else Orca's user data folder.
fn runtime_file() -> Option<PathBuf> {
    if let Some(file) = std::env::var_os("ORCA_RUNTIME_FILE").filter(|f| !f.is_empty()) {
        return Some(file.into());
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let data = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map_or(home.join(".config"), PathBuf::from)
    };
    Some(data.join("orca/orca-runtime.json"))
}

/// Whether an Orca runtime answers on its socket.
pub fn running() -> bool {
    Client::connect().is_ok_and(|mut client| client.call("status.get", Value::Null).is_ok())
}

/// The model key of a terminal handle (`term_…`).
pub fn id(handle: &str) -> PaneId {
    pane_id(handle)
}

/// Shows the terminal in Orca's window.
pub fn focus(bin: &str, handle: &str) -> Result<()> {
    run(bin, &["terminal", "switch", "--terminal", handle, "--json"])
}

/// The sender of mail muster sends from outside an Orca terminal. Orca keeps no mailbox for
/// it, so an agent cannot reply by mail; the subject says so.
const SENDER: &str = "muster";
const SENDER_SUBJECT: &str = "from muster: reply in your terminal, not by mail";

/// Sends Orca mail to the agent in the terminal. Orca tells the agent to check its mail; it
/// never types the text into the terminal. Inside an Orca terminal the mail comes from that
/// terminal. Elsewhere, as in Muster Notch, it comes from `muster`: Orca would otherwise guess
/// the sender from the working folder, and could pick another agent's terminal.
pub fn send(bin: &str, handle: &str, text: &str) -> Result<()> {
    let inside = std::env::var_os("ORCA_TERMINAL_HANDLE").is_some_and(|h| !h.is_empty());
    let mut args = vec!["orchestration", "send", "--to", handle];
    if inside {
        args.extend(["--subject", "muster"]);
    } else {
        args.extend(["--from", SENDER, "--subject", SENDER_SUBJECT]);
    }
    args.extend(["--body", text, "--json"]);
    run(bin, &args)
}

fn run(bin: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {bin}"))?;
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    if output.status.success() && value["ok"] != false {
        return Ok(());
    }
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string());
    bail!("orca {}: {message}", args[..2].join(" "))
}

/// One connection to the runtime's socket.
struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    token: String,
    next: u64,
}

impl Client {
    fn connect() -> Result<Self> {
        let file = runtime_file().context("no home folder")?;
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("Orca is not running ({} is missing)", file.display()))?;
        let meta: Value = serde_json::from_str(&text)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let endpoint = meta["transports"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["kind"] == "unix")
            .and_then(|t| t["endpoint"].as_str())
            .context("Orca's runtime has no local socket")?;
        let stream = UnixStream::connect(endpoint)
            .with_context(|| format!("Orca is not running (cannot connect to {endpoint})"))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            token: meta["authToken"].as_str().unwrap_or_default().to_string(),
            next: 0,
        })
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = format!("muster-{}", self.next);
        let mut line = json!({
            "id": id, "authToken": self.token, "method": method, "params": params,
        })
        .to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        loop {
            let mut reply = String::new();
            if self.reader.read_line(&mut reply)? == 0 {
                bail!("Orca closed the connection");
            }
            let Ok(frame) = serde_json::from_str::<Value>(&reply) else {
                bail!("Orca sent an invalid response");
            };
            // Keepalives and replies to abandoned requests.
            if frame["id"] != id.as_str() {
                continue;
            }
            if frame["ok"] == true {
                return Ok(frame["result"].clone());
            }
            let message = frame
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("Orca {method}: {message}");
        }
    }
}

/// One message from Orca's mailbox.
#[derive(Clone, Debug, PartialEq)]
pub struct Mail {
    pub id: String,
    pub from: PaneId,
    pub event: Event,
}

/// What one poll read: the terminals and the mail.
struct Read {
    panes: Vec<Pane>,
    mail: Vec<Mail>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn read(client: &mut Client, reads: &mut Reads) -> Result<Read> {
    let terminals = client.call("terminal.list", json!({}))?;
    let agents = client.call("agent.list", json!({}))?;
    let inbox = client.call("orchestration.inbox", json!({ "limit": MAIL_LIMIT }))?;
    let now = now_ms();
    Ok(Read {
        panes: panes(&terminals, &agents, now),
        mail: mail(&inbox, reads, now),
    })
}

/// Live terminals, each with the agent Orca reports in it (`paneKey` = `tabId:leafId`). A
/// terminal running in the background, in a workspace with no open tab, has a `pty:` tab key
/// that no agent's `paneKey` matches; its CLI shows, but not its state.
fn panes(terminals: &Value, agents: &Value, now: i64) -> Vec<Pane> {
    let by_key: HashMap<&str, &Value> = agents["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["kind"] == "terminal")
        .filter_map(|a| Some((a["paneKey"].as_str()?, a)))
        .collect();
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    terminals["terminals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["connected"] != false)
        .filter_map(|t| {
            let handle = t["handle"].as_str()?;
            let key = format!("{}:{}", t["tabId"].as_str()?, t["leafId"].as_str()?);
            let agent = by_key.get(key.as_str()).copied().unwrap_or(&Value::Null);
            let cli = text(&agent["agent"]).or_else(|| text(&t["agentIdentity"]));
            let cwd = text(&t["worktreePath"]).unwrap_or_default();
            let workspace = text(&agent["workspace"])
                .unwrap_or_else(|| cwd.rsplit('/').next().unwrap_or_default().to_string());
            let state = text(&agent["state"]);
            let working = state.as_deref() == Some("working");
            let last = t["lastOutputAt"].as_i64().unwrap_or(now);
            // Orca's terminal titles are the shell's; name the agent and its handle instead.
            let short: String = handle.trim_start_matches("term_").chars().take(4).collect();
            Some(Pane {
                id: id(handle),
                address: handle.to_string(),
                title: format!("{} {short}: {workspace}", cli.as_deref().unwrap_or("term")),
                cwd,
                session: workspace,
                working,
                quiet_ms: if working {
                    0
                } else {
                    (now - last).max(0) as u64
                },
                inspected_at: Some(Instant::now()),
                cli,
                status: state,
                mailbox: true,
                ..Pane::default()
            })
        })
        .collect()
}

/// When muster first saw each mail read. Orca records no time for a read.
#[derive(Default)]
struct Reads {
    /// The first poll, which reads history, is done.
    baselined: bool,
    at: HashMap<String, i64>,
}

/// Mail between terminals, oldest first. Mail is delivered only once Orca marks it `read`
/// (the recipient checked it): Orca also sets `delivered_at` on mail it only pointed the
/// recipient to, still unread. Mail already read on the first poll is dated by Orca's
/// `delivered_at`, else its send time; a later read is dated when first seen.
fn mail(inbox: &Value, reads: &mut Reads, now: i64) -> Vec<Mail> {
    let baseline = !reads.baselined;
    reads.baselined = true;
    let mut rows: Vec<&Value> = inbox["messages"].as_array().into_iter().flatten().collect();
    rows.sort_by_key(|m| m["sequence"].as_i64().unwrap_or(0));
    rows.into_iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let from = m["from_handle"].as_str()?;
            let to = m["to_handle"].as_str()?;
            let ts = time(&m["created_at"]).unwrap_or(now);
            let delivered = (m["read"].as_i64().unwrap_or(0) != 0).then(|| {
                *reads.at.entry(id.clone()).or_insert_with(|| {
                    if baseline {
                        time(&m["delivered_at"]).unwrap_or(ts)
                    } else {
                        now
                    }
                })
            });
            Some(Mail {
                from: self::id(from),
                event: Event::Mail {
                    ts,
                    id: id.clone(),
                    to: self::id(to),
                    handles: [from.to_string(), to.to_string()],
                    text: m["body"].as_str().unwrap_or_default().to_string(),
                    delivered,
                    thread: m["thread_id"].as_str().map(str::to_string),
                },
                id,
            })
        })
        .collect()
}

fn time(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    Some(
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()?
            .timestamp_millis(),
    )
}

/// Every terminal and message, for a one-off snapshot.
pub fn load() -> Result<(Vec<Pane>, Vec<Mail>)> {
    let mut client = Client::connect()?;
    let read = read(&mut client, &mut Reads::default())?;
    Ok((read.panes, read.mail))
}

/// The pane fields the views show; how long a terminal has been quiet is not one, so a busy
/// terminal's output alone sends nothing.
fn fingerprint(panes: &[Pane]) -> String {
    panes
        .iter()
        .map(|p| {
            format!(
                "{}\t{}\t{}\t{:?}\t{:?}\n",
                p.id, p.title, p.cwd, p.cli, p.status
            )
        })
        .collect()
}

pub struct Guard {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Polls Orca on a thread and sends terminals when they change, then new or newly read mail.
pub fn spawn(tx: Sender<Update>) -> Guard {
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || poll(&tx, &stop))
    };
    Guard {
        stop,
        worker: Some(worker),
    }
}

fn poll(tx: &Sender<Update>, stop: &AtomicBool) {
    let mut client: Option<Client> = None;
    let mut reads = Reads::default();
    let mut shown = None;
    let mut sent: HashMap<String, Event> = HashMap::new();
    let mut baseline = false;
    let mut failed = false;
    while !stop.load(Ordering::SeqCst) {
        let result = match client.as_mut() {
            Some(client) => read(client, &mut reads),
            None => Client::connect().and_then(|c| read(client.insert(c), &mut reads)),
        };
        match result {
            Ok(read) => {
                let print = fingerprint(&read.panes);
                if failed || shown.as_ref() != Some(&print) {
                    shown = Some(print);
                    if tx.send(Update::Panes(read.panes)).is_err() {
                        return;
                    }
                }
                failed = false;
                for mail in read.mail {
                    if sent.get(&mail.id) == Some(&mail.event) {
                        continue;
                    }
                    sent.insert(mail.id, mail.event.clone());
                    let update = Update::Events {
                        pane: mail.from,
                        sub: None,
                        events: vec![mail.event],
                    };
                    if tx.send(update).is_err() {
                        return;
                    }
                }
                if !baseline {
                    baseline = true;
                    if tx.send(Update::Baseline).is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                client = None;
                if !failed
                    && tx
                        .send(Update::Error(format!("{error:#}; retrying")))
                        .is_err()
                {
                    return;
                }
                failed = true;
            }
        }
        let until = Instant::now() + POLL;
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_is_oldest_first_and_read_mail_is_delivered_once() {
        let inbox = json!({"messages": [
            {"id": "m2", "sequence": 2, "from_handle": "term_b", "to_handle": "term_a",
             "body": "pong", "read": 0, "thread_id": "m1", "created_at": "2026-11-22T01:28:02Z"},
            {"id": "m1", "sequence": 1, "from_handle": "term_a", "to_handle": "term_b",
             "body": "ping", "read": 1, "thread_id": null, "created_at": "2026-11-22T01:25:05Z"},
            {"id": "m0", "sequence": 0, "from_handle": null, "to_handle": "term_a", "body": "x"},
        ]});
        let mut reads = Reads::default();
        let first = mail(&inbox, &mut reads, 9_999_999_999_999);
        let ping_ts = time(&json!("2026-11-22T01:25:05Z")).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|m| (m.id.as_str(), m.from))
                .collect::<Vec<_>>(),
            vec![("m1", id("term_a")), ("m2", id("term_b"))]
        );
        // History read on the first poll is dated by its send time.
        assert!(
            matches!(&first[0].event, Event::Mail { delivered: Some(at), .. } if *at == ping_ts)
        );
        assert!(matches!(&first[1].event,
            Event::Mail { delivered: None, thread: Some(t), to, .. } if t == "m1" && *to == id("term_a")));
        // Mail read later is dated when that is first seen, and keeps that date.
        let mut inbox = inbox;
        inbox["messages"][0]["read"] = json!(1);
        let later = mail(&inbox, &mut reads, 5_000);
        assert!(matches!(
            &later[1].event,
            Event::Mail {
                delivered: Some(5_000),
                ..
            }
        ));
        let again = mail(&inbox, &mut reads, 6_000);
        assert_eq!(later, again);
    }

    #[test]
    fn pointed_but_unread_mail_is_not_delivered() {
        // Orca sets delivered_at when it points the recipient at mail, before any read.
        let inbox = json!({"messages": [
            {"id": "m1", "sequence": 1, "from_handle": "term_a", "to_handle": "term_b",
             "body": "ping", "read": 0, "created_at": "2026-11-22T01:25:05Z",
             "delivered_at": "2026-11-22T01:25:06Z"},
        ]});
        let mut reads = Reads::default();
        let got = mail(&inbox, &mut reads, 9_000);
        assert!(matches!(
            &got[0].event,
            Event::Mail {
                delivered: None,
                ..
            }
        ));
    }

    #[test]
    fn first_read_after_an_all_unread_baseline_is_dated_when_seen() {
        let mut inbox = json!({"messages": [
            {"id": "m1", "sequence": 1, "from_handle": "term_a", "to_handle": "term_b",
             "body": "ping", "read": 0, "created_at": "2026-11-22T01:25:05Z"},
        ]});
        let mut reads = Reads::default();
        mail(&inbox, &mut reads, 1_000);
        inbox["messages"][0]["read"] = json!(1);
        let got = mail(&inbox, &mut reads, 7_000);
        assert!(matches!(
            &got[0].event,
            Event::Mail {
                delivered: Some(7_000),
                ..
            }
        ));
    }

    #[test]
    fn mail_from_outside_orca_is_named_by_its_sender() {
        let inbox = json!({"messages": [
            {"id": "m1", "sequence": 1, "from_handle": "muster", "to_handle": "term_527ede88",
             "body": "hi", "read": 0, "created_at": "2026-11-22T01:25:05Z"},
        ]});
        let mut model = crate::model::Model::default();
        for m in mail(&inbox, &mut Reads::default(), 9_000) {
            model.apply(m.from, None, vec![m.event]);
        }
        let name = |handle| model.name(&crate::model::Node::Pane(id(handle)));
        assert_eq!(name("muster"), "muster");
        assert_eq!(name("term_527ede88"), "term 527e");
    }

    #[test]
    fn terminals_join_their_agents_by_pane_key() {
        let terminals = json!({"terminals": [
            {"handle": "term_7240df0f", "tabId": "t1", "leafId": "l1", "connected": true,
             "worktreePath": "/w/orca-ws", "agentIdentity": "claude", "lastOutputAt": 1_000},
            {"handle": "term_dead", "tabId": "t2", "leafId": "l2", "connected": false},
        ]});
        let agents = json!({"agents": [
            {"kind": "terminal", "agent": "claude", "state": "working", "paneKey": "t1:l1",
             "workspace": "orca-ws"},
        ]});
        let panes = panes(&terminals, &agents, 5_000);
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert_eq!(
            (pane.id, pane.address.as_str()),
            (id("term_7240df0f"), "term_7240df0f")
        );
        assert_eq!(pane.title, "claude 7240: orca-ws");
        assert_eq!(
            (pane.state(), pane.quiet_ms, pane.mailbox),
            ("working", 0, true)
        );
    }
}
