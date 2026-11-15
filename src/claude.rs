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
/// stopped.
pub struct Follower {
    config: Option<PathBuf>,
    history: u64,
    /// The last terminals from hc, with transcripts attached.
    panes: Vec<Pane>,
    tails: HashMap<PaneId, Tail>,
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

    /// Reads the transcripts among `paths`.
    pub fn changed(&mut self, paths: &[PathBuf]) -> Batch {
        let mut batch = Vec::new();
        for (pane, tail) in &mut self.tails {
            if paths
                .iter()
                .any(|path| path.file_name() == tail.path.file_name())
            {
                read(&mut batch, *pane, tail);
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

    fn sync(&mut self, panes: &[Pane]) -> Batch {
        self.tails.retain(|id, tail| {
            panes
                .iter()
                .any(|pane| pane.id == *id && pane.session_file.as_ref() == Some(&tail.path))
        });
        let mut batch = Vec::new();
        for pane in panes {
            let Some(file) = &pane.session_file else {
                continue;
            };
            if !self.tails.contains_key(&pane.id) {
                let mut tail = Tail::new(file.clone(), self.history);
                read(&mut batch, pane.id, &mut tail);
                self.tails.insert(pane.id, tail);
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
}
