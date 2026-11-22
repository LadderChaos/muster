//! Which multiplexer muster reads: Tern (panes and omp transcripts), herdr (hc terminals, and
//! the transcripts of local Claude Code and Codex terminals) or Orca (its terminals, agents and
//! mail). Only the chosen one runs.

use std::any::Any;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Result, bail};

use crate::model::PaneId;
use crate::source::{self, Update};
use crate::{herdr, orca, tern};

#[derive(Clone, Debug)]
pub enum Backend {
    Tern { bin: String },
    Herdr { hc: String, app: String },
    Orca { bin: String },
}

impl Backend {
    /// `tern`, `herdr`, `orca`, or `auto`: the multiplexer this process runs in, else a running
    /// Tern, else a running herdr core, else a running Orca, else Tern.
    pub fn resolve(choice: &str) -> Result<Self> {
        let tern = || Backend::Tern {
            bin: tern::binary(),
        };
        let herdr = || Backend::Herdr {
            hc: herdr::binary(),
            app: herdr::app_binary(),
        };
        let orca = || Backend::Orca {
            bin: orca::binary(),
        };
        let set = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
        Ok(match choice {
            "tern" => tern(),
            "herdr" => herdr(),
            "orca" => orca(),
            "auto" if set("TERN_PANE") => tern(),
            "auto" if set("HC_TERMINAL_ID") => herdr(),
            "auto" if set("ORCA_TERMINAL_HANDLE") => orca(),
            "auto" if tern_running(&tern::binary()) => tern(),
            "auto" if herdr::running(&herdr::binary()) => herdr(),
            "auto" if orca::running() => orca(),
            "auto" => tern(),
            other => bail!("--backend takes tern, herdr, orca or auto, not {other}"),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Tern { .. } => "tern",
            Backend::Herdr { .. } => "herdr",
            Backend::Orca { .. } => "orca",
        }
    }

    /// The window's word for a pane, in user-facing text.
    pub fn pane_word(&self) -> &'static str {
        match self {
            Backend::Tern { .. } => "pane",
            Backend::Herdr { .. } | Backend::Orca { .. } => "terminal",
        }
    }

    /// The pane muster itself runs in, to leave it off the map.
    pub fn self_pane(&self) -> Option<PaneId> {
        match self {
            Backend::Tern { .. } => std::env::var("TERN_PANE").ok()?.parse().ok(),
            Backend::Herdr { .. } => Some(herdr::pane_id(&std::env::var("HC_TERMINAL_ID").ok()?)),
            Backend::Orca { .. } => Some(orca::id(&std::env::var("ORCA_TERMINAL_HANDLE").ok()?)),
        }
    }

    pub fn focus(&self, address: &str) -> Result<()> {
        match self {
            Backend::Tern { bin } => tern::focus(bin, address),
            Backend::Herdr { app, .. } => herdr::focus(app, address),
            Backend::Orca { bin } => orca::focus(bin, address),
        }
    }

    pub fn send(&self, address: &str, text: &str) -> Result<()> {
        match self {
            Backend::Tern { bin } => tern::send(bin, address, text),
            Backend::Herdr { hc, .. } => herdr::send(hc, address, text),
            Backend::Orca { bin } => orca::send(bin, address, text),
        }
    }

    /// Starts the live watcher; it stops when the returned guard drops.
    pub fn spawn(&self, history: u64, tx: Sender<Update>) -> Box<dyn Any + Send> {
        match self {
            Backend::Tern { bin } => Box::new(source::spawn(
                source::Options {
                    bin: bin.clone(),
                    history,
                },
                tx,
            )),
            Backend::Herdr { hc, .. } => Box::new(herdr::spawn(hc.clone(), history, tx)),
            Backend::Orca { .. } => Box::new(orca::spawn(tx)),
        }
    }
}

/// `tern ls` succeeds only while a Tern session daemon answers; it never starts one.
fn tern_running(bin: &str) -> bool {
    Command::new(bin)
        .arg("ls")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success())
}
