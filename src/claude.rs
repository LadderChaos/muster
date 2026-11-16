//! Claude Code transcripts of local herdr terminals. Claude Code records each running process in
//! `<config>/sessions/<pid>.json` (its session ID and working directory); the transcript is
//! `<config>/projects/<cwd, every other character than A-Z a-z 0-9 as '-'>/<session>.jsonl`.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> Option<PathBuf> {
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
pub fn session_file(config: &Path, pid: u32) -> Option<PathBuf> {
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
