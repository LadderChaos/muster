//! The transcripts of local herdr terminals that run Claude Code or Codex: which file each
//! terminal's agent writes (see `claude` and `codex`), read from where the last read stopped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::claude;
use crate::codex::Codex;
use crate::model::PaneId;
use crate::pane::Pane;
use crate::source::Update;
use crate::transcript::{self, Event, Tail};

/// New transcript events, by pane.
pub type Batch = Vec<(PaneId, Vec<Event>)>;
/// How often shown transcripts are read without a file event.
const POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// A transcript keeps its read position after its process exits, so a resumed session reads
/// only what is new.
pub struct Follower {
    claude: Option<PathBuf>,
    codex: Option<Codex>,
    history: u64,
    /// The last terminals from hc, with transcripts attached.
    panes: Vec<Pane>,
    tails: HashMap<PathBuf, Tail>,
}

impl Follower {
    pub fn new(history: u64) -> Self {
        Self {
            claude: claude::config_dir(),
            codex: Codex::new(),
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

    /// Looks up the transcripts again after an agent changed its session records (Claude Code's
    /// `/clear` and `/resume`, a new Codex transcript). `None` when no
    /// terminal's transcript changed.
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
        self.read_shown(|file| {
            paths
                .iter()
                .any(|path| path.file_name() == file.file_name())
        })
    }

    /// Reads every shown transcript, and looks again for Codex transcripts whose first line was
    /// incomplete. Codex keeps its transcript open while it appends, and macOS reports a file's
    /// changes only when it is closed, so the watcher does not see Codex's appends.
    pub fn poll(&mut self) -> (Option<Vec<Pane>>, Batch) {
        let retry = self.codex.as_mut().is_some_and(|codex| {
            let pending = codex.pending();
            if pending {
                codex.rescan();
            }
            pending
        });
        let (panes, mut batch) = match retry.then(|| self.refresh()).flatten() {
            Some((panes, batch)) => (Some(panes), batch),
            None => (None, Vec::new()),
        };
        batch.extend(self.read_shown(|_| true));
        (panes, batch)
    }

    fn read_shown(&mut self, wanted: impl Fn(&Path) -> bool) -> Batch {
        let mut batch = Vec::new();
        for (file, tail) in &mut self.tails {
            if !wanted(file) {
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

    fn attach(&mut self, panes: &mut [Pane]) {
        for pane in panes.iter_mut() {
            pane.session_file = pane
                .pid
                .and_then(|pid| claude::session_file(self.claude.as_deref()?, pid));
        }
        if let Some(codex) = self.codex.as_mut() {
            codex.attach(panes);
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

/// Watches the transcripts and the session records that map processes to them, and polls the
/// transcripts every second (see `Follower::poll`). Updates are sent with the follower locked,
/// so they stay in order with the terminal thread's. Dropping the watcher and the follower stops
/// both.
pub fn watch(follower: &Arc<Mutex<Follower>>, tx: Sender<Update>) -> Option<RecommendedWatcher> {
    let (claude, codex_sessions) = {
        let follower = follower.lock().unwrap();
        let codex = follower.codex.as_ref().map(|codex| codex.sessions.clone());
        (follower.claude.clone(), codex)
    };
    let claude_sessions = claude.as_ref().map(|config| config.join("sessions"));
    let under =
        |path: &Path, dir: &Option<PathBuf>| dir.as_ref().is_some_and(|dir| path.starts_with(dir));
    let (weak, poll_tx) = (Arc::downgrade(follower), tx.clone());
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(POLL);
            let Some(follower) = weak.upgrade() else {
                return;
            };
            let mut follower = follower.lock().unwrap();
            let (panes, batch) = follower.poll();
            if !send(&poll_tx, panes, batch) {
                return;
            }
        }
    });
    let follower = Arc::clone(follower);
    let (watched_claude, watched_codex) = (claude_sessions.clone(), codex_sessions.clone());
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let mut follower = follower.lock().unwrap();
        // A Codex transcript not read yet may be one a terminal waits for.
        let new_codex = follower.codex.as_mut().is_some_and(|codex| {
            let new = event
                .paths
                .iter()
                .any(|path| under(path, &watched_codex) && !codex.knows(path));
            if new {
                codex.rescan();
            }
            new
        });
        let records_changed = event.paths.iter().any(|path| under(path, &watched_claude));
        if (new_codex || records_changed)
            && let Some((panes, batch)) = follower.refresh()
        {
            send(&tx, Some(panes), batch);
        }
        let batch = follower.changed(&event.paths);
        send(&tx, None, batch);
    })
    .ok()?;
    // Folders may be missing until each agent first runs.
    if let Some(config) = &claude {
        let _ = watcher.watch(&config.join("projects"), RecursiveMode::Recursive);
    }
    if let Some(sessions) = &claude_sessions {
        let _ = watcher.watch(sessions, RecursiveMode::NonRecursive);
    }
    if let Some(sessions) = &codex_sessions {
        let _ = watcher.watch(sessions, RecursiveMode::Recursive);
    }
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            claude: Some(config.clone()),
            codex: None,
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
