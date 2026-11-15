//! Claude Code transcripts of local herdr terminals. Claude Code records each running process in
//! `<config>/sessions/<pid>.json` (its session ID and working directory); the transcript is
//! `<config>/projects/<cwd, every other character than A-Z a-z 0-9 as '-'>/<session>.jsonl`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;

use crate::model::PaneId;
use crate::pane::Pane;
use crate::source::Update;
use crate::transcript::{self, Event, Tail};

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")))
}

fn project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The transcript of the Claude Code process `pid`. It may not exist yet: Claude Code writes it
/// with the first turn.
fn session_file(config: &Path, pid: u32) -> Option<PathBuf> {
    let bytes = std::fs::read(config.join("sessions").join(format!("{pid}.json"))).ok()?;
    let session: Value = serde_json::from_slice(&bytes).ok()?;
    let id = session["sessionId"]
        .as_str()
        .filter(|id| !id.is_empty() && !id.contains(['/', '\\']))?;
    let name = format!("{id}.jsonl");
    let projects = config.join("projects");
    let expected = projects
        .join(project_dir(session["cwd"].as_str()?))
        .join(&name);
    if expected.exists() {
        return Some(expected);
    }
    // Long working directories get a shortened folder name.
    std::fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join(&name))
        .find(|path| path.exists())
        .or(Some(expected))
}

/// New transcript events, by pane.
pub type Batch = Vec<(PaneId, Vec<Event>)>;

/// The Claude Code transcripts of the current terminals, each read from where the last read
/// stopped. A transcript keeps its read position after its process exits, so a `claude --resume`
/// of the same session, or `/resume` back to it, reads only what is new.
pub struct Follower {
    config: Option<PathBuf>,
    history: u64,
    /// The last terminals from hc, with transcripts attached.
    panes: Vec<Pane>,
    tails: HashMap<PathBuf, Tail>,
}

impl Follower {
    pub fn new(history: u64) -> Self {
        Self {
            config: config_dir(),
            history,
            panes: Vec::new(),
            tails: HashMap::new(),
        }
    }

    /// Attaches transcripts to new terminal state and reads the transcripts it adds.
    pub fn panes(&mut self, mut panes: Vec<Pane>) -> (Vec<Pane>, Batch) {
        self.attach(&mut panes);
        let batch = self.sync(&panes);
        self.panes = panes.clone();
        (panes, batch)
    }

    /// Looks up the transcripts again after Claude Code changed its session records, as
    /// `/clear` and `/resume` do. `None` when no terminal's transcript changed.
    pub fn refresh(&mut self) -> Option<(Vec<Pane>, Batch)> {
        let mut panes = self.panes.clone();
        self.attach(&mut panes);
        let same = panes
            .iter()
            .zip(&self.panes)
            .all(|(new, old)| new.session_file == old.session_file);
        if same {
            return None;
        }
        Some(self.panes(panes))
    }

    /// Reads the transcripts among `paths` that belong to a current terminal. Others keep their
    /// appends until a terminal shows them again.
    pub fn changed(&mut self, paths: &[PathBuf]) -> Batch {
        let mut batch = Vec::new();
        for (file, tail) in &mut self.tails {
            if !paths
                .iter()
                .any(|path| path.file_name() == file.file_name())
            {
                continue;
            }
            if let Some(pane) = self
                .panes
                .iter()
                .find(|p| p.session_file.as_ref() == Some(file))
            {
                read(&mut batch, pane.id, tail);
            }
        }
        batch
    }

    fn attach(&self, panes: &mut [Pane]) {
        let Some(config) = &self.config else { return };
        for pane in panes {
            pane.session_file = pane.pid.and_then(|pid| session_file(config, pid));
        }
    }

    /// Reads each shown transcript: its history the first time, then whatever was added while no
    /// terminal showed it.
    fn sync(&mut self, panes: &[Pane]) -> Batch {
        let mut batch = Vec::new();
        for pane in panes {
            let Some(file) = &pane.session_file else {
                continue;
            };
            let shown = self
                .panes
                .iter()
                .any(|p| p.session_file.as_ref() == Some(file));
            let history = self.history;
            let tail = self
                .tails
                .entry(file.clone())
                .or_insert_with(|| Tail::new(file.clone(), history));
            // A transcript already shown is read by the file watcher.
            if !shown {
                read(&mut batch, pane.id, tail);
            }
        }
        batch
    }
}

fn read(batch: &mut Batch, pane: PaneId, tail: &mut Tail) {
    let events: Vec<Event> = tail.poll().iter().flat_map(transcript::parse).collect();
    if !events.is_empty() {
        batch.push((pane, events));
    }
}

/// Sends terminal state, if any, then the transcript events. False when nobody receives.
pub fn send(tx: &Sender<Update>, panes: Option<Vec<Pane>>, batch: Batch) -> bool {
    if let Some(panes) = panes
        && tx.send(Update::Panes(panes)).is_err()
    {
        return false;
    }
    batch.into_iter().all(|(pane, events)| {
        tx.send(Update::Events {
            pane,
            sub: None,
            events,
        })
        .is_ok()
    })
}

/// Watches Claude Code's transcripts and session records. Updates are sent with the follower
/// locked, so they stay in order with the terminal thread's. Dropping the watcher stops it.
pub fn watch(follower: &Arc<Mutex<Follower>>, tx: Sender<Update>) -> Option<RecommendedWatcher> {
    let config = follower.lock().unwrap().config.clone()?;
    let sessions = config.join("sessions");
    let follower = Arc::clone(follower);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let mut follower = follower.lock().unwrap();
        if event.paths.iter().any(|path| path.starts_with(&sessions))
            && let Some((panes, batch)) = follower.refresh()
        {
            send(&tx, Some(panes), batch);
        }
        let batch = follower.changed(&event.paths);
        send(&tx, None, batch);
    })
    .ok()?;
    // Either folder may be missing until Claude Code first runs.
    let _ = watcher.watch(&config.join("projects"), RecursiveMode::Recursive);
    let _ = watcher.watch(&config.join("sessions"), RecursiveMode::NonRecursive);
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_transcript_from_the_process_record() {
        let config = std::env::temp_dir().join(format!("muster-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        let project = config.join("projects").join("-Users-me-my-app-v2");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            config.join("sessions/42.json"),
            r#"{"pid":42,"sessionId":"abc-1","cwd":"/Users/me/my_app.v2"}"#,
        )
        .unwrap();
        // Not written yet: the expected place.
        assert_eq!(session_file(&config, 42), Some(project.join("abc-1.jsonl")));
        // Under a folder whose name does not follow the cwd.
        let short = config.join("projects").join("-Users-me-my-app-123abc");
        std::fs::create_dir_all(&short).unwrap();
        std::fs::write(short.join("abc-1.jsonl"), "").unwrap();
        assert_eq!(session_file(&config, 42), Some(short.join("abc-1.jsonl")));
        assert_eq!(session_file(&config, 7), None);
        std::fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn a_resumed_session_reads_only_what_was_added_while_its_process_was_gone() {
        let config = std::env::temp_dir().join(format!("muster-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        std::fs::create_dir_all(config.join("projects/-x")).unwrap();
        let record = |pid: u32| {
            std::fs::write(
                config.join(format!("sessions/{pid}.json")),
                r#"{"sessionId":"s","cwd":"/x"}"#,
            )
            .unwrap()
        };
        let transcript = config.join("projects/-x/s.jsonl");
        let turn = |uuid: &str| {
            format!(
                "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"timestamp\":\"2026-11-16T00:00:00Z\",\
                 \"message\":{{\"content\":\"prompt {uuid}\"}}}}\n"
            )
        };
        std::fs::write(&transcript, turn("a")).unwrap();
        let pane = |pid: u32| Pane {
            id: 9,
            pid: Some(pid),
            ..Pane::default()
        };
        let mut follower = Follower {
            config: Some(config.clone()),
            history: 1 << 20,
            panes: Vec::new(),
            tails: HashMap::new(),
        };
        let texts = |batch: Batch| -> Vec<String> {
            batch
                .into_iter()
                .flat_map(|(_, events)| events)
                .filter_map(|event| match event {
                    Event::User { text, .. } => Some(text),
                    _ => None,
                })
                .collect()
        };
        record(1);
        assert_eq!(texts(follower.panes(vec![pane(1)]).1), ["prompt a"]);
        // The process exits; its record goes, and the session gains a turn meanwhile.
        std::fs::remove_file(config.join("sessions/1.json")).unwrap();
        assert!(follower.panes(vec![pane(1)]).1.is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, turn("b").as_bytes()).unwrap();
        assert!(
            follower
                .changed(std::slice::from_ref(&transcript))
                .is_empty()
        );
        // `claude --resume` of the same session, under a new process.
        record(2);
        assert_eq!(texts(follower.panes(vec![pane(2)]).1), ["prompt b"]);
        std::fs::remove_dir_all(&config).unwrap();
    }
}
