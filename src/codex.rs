//! Codex transcripts of local herdr terminals. Codex writes each session to
//! `<home>/sessions/YYYY/MM/DD/rollout-<local time>-<session>.jsonl`; its first line
//! (`session_meta`) names the session's folder, start time and client. Codex records no process:
//! its terminal UI is a client of a shared app-server that runs every session and every hook. So a
//! terminal's transcript is the one its `codex resume <session>` command line names, else the
//! newest interactive session started in its folder since its process started, but only when no
//! other Codex terminal runs in that folder.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::TimeZone;
use serde_json::Value;

use crate::pane::Pane;

/// A session's start can be logged slightly before its process's start, which `ps` rounds down.
const START_SLACK_MS: i64 = 2_000;

/// `$CODEX_HOME`, else `~/.codex`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex")))
}

/// Codex session IDs are UUIDs; anything else is not a session to look up.
fn session_id(text: &str) -> Option<&str> {
    let uuid = text.len() == 36
        && text.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    uuid.then_some(text)
}

/// The session a `codex resume <session>` command line names. `codex resume --last` and the
/// picker name none.
pub fn resumed_session(argv: &[String]) -> Option<String> {
    let program = Path::new(argv.first()?).file_name()?.to_str()?;
    if !program.starts_with("codex") {
        return None;
    }
    let resume = argv.iter().position(|arg| arg == "resume")?;
    argv[resume + 1..]
        .iter()
        .find_map(|arg| session_id(arg))
        .map(str::to_string)
}

fn is_codex(pane: &Pane) -> bool {
    pane.cli
        .as_deref()
        .is_some_and(|cli| cli.starts_with("codex"))
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// An interactive session, from its transcript's first line.
#[derive(Clone, Debug, PartialEq)]
struct Session {
    cwd: PathBuf,
    start_ms: i64,
}

/// `session_meta` of a session a person runs in the terminal UI; `None` for any other client
/// (Desktop, `exec`, editors) and for subagents. `Err` while the line is not complete.
fn session(first_line: &str) -> Result<Option<Session>, ()> {
    let entry: Value = serde_json::from_str(first_line).map_err(|_| ())?;
    if entry["type"] != "session_meta" {
        return Ok(None);
    }
    let meta = &entry["payload"];
    let interactive = matches!(
        meta["originator"].as_str(),
        Some("codex-tui" | "codex_cli_rs")
    ) && meta["source"].is_string()
        && matches!(meta["thread_source"].as_str(), None | Some("user"));
    let start_ms = meta["timestamp"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.timestamp_millis());
    Ok(match (interactive, meta["cwd"].as_str(), start_ms) {
        (true, Some(cwd), Some(start_ms)) => Some(Session {
            cwd: canonical(cwd),
            start_ms,
        }),
        _ => None,
    })
}

fn first_line(path: &Path) -> Option<String> {
    let mut line = String::new();
    BufReader::new(std::fs::File::open(path).ok()?)
        .read_line(&mut line)
        .ok()?;
    line.ends_with('\n').then_some(line)
}

/// When process `pid` started, in ms.
fn process_start(pid: u32) -> Option<i64> {
    let output = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let time = chrono::NaiveDateTime::parse_from_str(&text, "%a %b %d %H:%M:%S %Y").ok()?;
    Some(
        chrono::Local
            .from_local_datetime(&time)
            .earliest()?
            .timestamp_millis(),
    )
}

/// Codex's sessions folder, and what is known of its transcripts.
pub struct Codex {
    pub sessions: PathBuf,
    /// Transcripts by path; `None` when not an interactive session.
    seen: HashMap<PathBuf, Option<Session>>,
    /// Transcripts by resumed session ID.
    rollouts: HashMap<String, PathBuf>,
    /// Resumed sessions with no transcript found.
    missing: HashSet<String>,
    /// Transcripts whose first line was incomplete when read.
    pending: HashSet<PathBuf>,
    started: HashMap<u32, Option<i64>>,
    /// Day folders from this date (`YYYY-MM-DD`) on are read; `None` until the first read.
    read_from: Option<String>,
    stale: bool,
}

impl Codex {
    pub fn new() -> Option<Self> {
        Some(Self::at(home_dir()?.join("sessions")))
    }

    fn at(sessions: PathBuf) -> Self {
        Self {
            sessions,
            seen: HashMap::new(),
            rollouts: HashMap::new(),
            missing: HashSet::new(),
            pending: HashSet::new(),
            started: HashMap::new(),
            read_from: None,
            stale: true,
        }
    }

    /// True when `path` is a transcript already read, or not a transcript.
    pub fn knows(&self, path: &Path) -> bool {
        self.seen.contains_key(path)
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
    }

    /// True when a transcript's first line was incomplete when last read. Codex appends to an
    /// open file, so no file event says when the line is complete.
    pub fn pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Codex added or changed a transcript not read yet: look again.
    pub fn rescan(&mut self) {
        self.stale = true;
        self.missing.clear();
    }

    /// Attaches transcripts to the Codex terminals among `panes` that have none yet.
    pub fn attach(&mut self, panes: &mut [Pane]) {
        let codex: Vec<usize> = (0..panes.len())
            .filter(|&i| is_codex(&panes[i]) && panes[i].pid.is_some())
            .collect();
        let live: HashSet<u32> = codex.iter().filter_map(|&i| panes[i].pid).collect();
        self.started.retain(|pid, _| live.contains(pid));
        let folders: Vec<PathBuf> = codex.iter().map(|&i| canonical(&panes[i].cwd)).collect();
        for (n, &i) in codex.iter().enumerate() {
            if panes[i].session_file.is_some() {
                continue;
            }
            if let Some(id) = panes[i].resumed.clone() {
                panes[i].session_file = self.rollout(&id);
                continue;
            }
            let folder = &folders[n];
            if panes[i].cwd.is_empty() || folders.iter().filter(|f| *f == folder).count() > 1 {
                continue;
            }
            let pid = panes[i].pid.unwrap_or_default();
            let Some(start) = *self
                .started
                .entry(pid)
                .or_insert_with(|| process_start(pid))
            else {
                continue;
            };
            self.read(start);
            panes[i].session_file = self
                .seen
                .iter()
                .filter_map(|(path, session)| Some((path, session.as_ref()?)))
                .filter(|(_, s)| s.cwd == *folder && s.start_ms >= start - START_SLACK_MS)
                .max_by_key(|(_, s)| s.start_ms)
                .map(|(path, _)| path.clone());
        }
    }

    /// Reads the first line of transcripts in day folders from `start`'s date on.
    fn read(&mut self, start: i64) {
        let Some(day) = chrono::Local
            .timestamp_millis_opt(start - 86_400_000)
            .single()
        else {
            return;
        };
        let from = day.format("%Y-%m-%d").to_string();
        if !self.stale && self.read_from.as_ref().is_some_and(|read| *read <= from) {
            return;
        }
        let from = self
            .read_from
            .take()
            .map_or(from.clone(), |read| read.min(from));
        for dir in days(&self.sessions)
            .into_iter()
            .filter(|(date, _)| *date >= from)
        {
            for path in std::fs::read_dir(dir.1)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
            {
                if self.knows(&path) {
                    continue;
                }
                // An incomplete first line is read again later.
                match first_line(&path).map(|line| session(&line)) {
                    Some(Ok(session)) => {
                        self.pending.remove(&path);
                        self.seen.insert(path, session);
                    }
                    _ => {
                        self.pending.insert(path);
                    }
                }
            }
        }
        self.read_from = Some(from);
        self.stale = false;
    }

    fn rollout(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(path) = self.rollouts.get(id) {
            return Some(path.clone());
        }
        if self.missing.contains(id) {
            return None;
        }
        let suffix = format!("-{id}.jsonl");
        let found = days(&self.sessions).into_iter().rev().find_map(|(_, day)| {
            std::fs::read_dir(day)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
                })
        });
        match &found {
            Some(path) => self.rollouts.insert(id.to_string(), path.clone()),
            None => {
                self.missing.insert(id.to_string());
                None
            }
        };
        found
    }
}

/// `sessions/YYYY/MM/DD` folders as (`YYYY-MM-DD`, folder), oldest first.
fn days(sessions: &Path) -> Vec<(String, PathBuf)> {
    let children = |dir: &Path| -> Vec<(String, PathBuf)> {
        let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| Some((entry.file_name().to_str()?.to_string(), entry.path())))
            .collect();
        found.sort_unstable();
        found
    };
    let mut found = Vec::new();
    for (year, year_dir) in children(sessions) {
        for (month, month_dir) in children(&year_dir) {
            for (day, day_dir) in children(&month_dir) {
                found.push((format!("{year}-{month}-{day}"), day_dir));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn reads_the_resumed_session_from_the_command_line() {
        let id = "01a1de22-4dc4-77b1-8f26-3278b4caba2d";
        assert_eq!(
            resumed_session(&argv(&["/opt/bin/codex", "-m", "x", "resume", id, "go on"])),
            Some(id.to_string())
        );
        assert_eq!(resumed_session(&argv(&["codex", "resume", "--last"])), None);
        assert_eq!(resumed_session(&argv(&["codex"])), None);
        assert_eq!(resumed_session(&argv(&["claude", "--resume", id])), None);
    }

    #[test]
    fn only_sessions_a_person_runs_in_the_terminal_ui_count() {
        let meta = |payload: Value| json!({"type": "session_meta", "payload": payload}).to_string();
        let tui = json!({"originator": "codex-tui", "source": "vscode", "thread_source": "user",
                         "cwd": "/", "timestamp": "2026-11-15T17:22:10.257Z"});
        assert_eq!(
            session(&meta(tui.clone())),
            Ok(Some(Session {
                cwd: PathBuf::from("/"),
                start_ms: 1_794_763_330_257
            }))
        );
        let mut subagent = tui.clone();
        subagent["source"] = json!({"subagent": {"other": "guardian"}});
        let mut desktop = tui.clone();
        desktop["originator"] = json!("Codex Desktop");
        let mut exec = tui;
        exec["originator"] = json!("codex_exec");
        for other in [subagent, desktop, exec] {
            assert_eq!(session(&meta(other.clone())), Ok(None), "{other}");
        }
        assert_eq!(session("{\"type\":\"session_me"), Err(()));
    }

    #[test]
    fn matches_by_folder_only_when_one_codex_terminal_runs_there() {
        let root = std::env::temp_dir().join(format!("muster-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let today = chrono::Local::now();
        let day = root.join(today.format("sessions/%Y/%m/%d").to_string());
        let (alone, shared) = (root.join("alone"), root.join("shared"));
        for dir in [&day, &alone, &shared] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let now = today.timestamp_millis();
        let write = |name: &str, cwd: &Path, start_ms: i64| {
            let time = chrono::Utc
                .timestamp_millis_opt(start_ms)
                .unwrap()
                .to_rfc3339();
            let line = json!({"type": "session_meta", "payload": {"originator": "codex-tui",
                "source": "cli", "cwd": cwd, "timestamp": time}});
            let path = day.join(format!("rollout-{name}.jsonl"));
            std::fs::write(&path, format!("{line}\n")).unwrap();
            path
        };
        let mut codex = Codex::at(root.join("sessions"));
        let pane = |id: u64, pid: u32, cwd: &Path| Pane {
            id,
            pid: Some(pid),
            cli: Some("codex".into()),
            cwd: cwd.to_string_lossy().into(),
            ..Pane::default()
        };
        let mut panes = vec![
            pane(1, 101, &alone),
            pane(2, 102, &shared),
            pane(3, 103, &shared),
        ];
        let process = now - 60_000;
        codex.started.extend([
            (101, Some(process)),
            (102, Some(process)),
            (103, Some(process)),
        ]);
        // Before this process, then two sessions of it (a `/new`): the newest is its own.
        write("old", &alone, process - 600_000);
        write("first", &alone, process + 1_000);
        let newest = write("second", &alone, process + 30_000);
        write("other", &shared, process + 5_000);
        codex.attach(&mut panes);
        assert_eq!(panes[0].session_file, Some(newest));
        assert_eq!(
            (&panes[1].session_file, &panes[2].session_file),
            (&None, &None)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
