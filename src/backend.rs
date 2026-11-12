//! Which multiplexer ternwatch reads: Tern (panes and omp transcripts, so agents and messages) or
//! herdr (hc terminals, so agents only).

use std::any::Any;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Result, bail};

use crate::model::PaneId;
use crate::source::{self, Update};
use crate::{herdr, tern};

#[derive(Clone, Debug)]
pub enum Backend {
    Tern { bin: String },
    Herdr { hc: String, app: String },
}

impl Backend {
    /// `tern`, `herdr`, or `auto`: the multiplexer this process runs in, else a running Tern,
    /// else a running herdr core, else Tern.
    pub fn resolve(choice: &str) -> Result<Self> {
        let tern = || Backend::Tern {
            bin: tern::binary(),
        };
        let herdr = || Backend::Herdr {
            hc: herdr::binary(),
            app: herdr::app_binary(),
        };
        let set = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
        Ok(match choice {
            "tern" => tern(),
            "herdr" => herdr(),
            "auto" if set("TERN_PANE") => tern(),
            "auto" if set("HC_TERMINAL_ID") => herdr(),
            "auto" if tern_running(&tern::binary()) => tern(),
            "auto" if herdr::running(&herdr::binary()) => herdr(),
            "auto" => tern(),
            other => bail!("--backend takes tern, herdr or auto, not {other}"),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Tern { .. } => "tern",
            Backend::Herdr { .. } => "herdr",
        }
    }

    /// The window's word for a pane, in user-facing text.
    pub fn pane_word(&self) -> &'static str {
        match self {
            Backend::Tern { .. } => "pane",
            Backend::Herdr { .. } => "terminal",
        }
    }

    /// The pane ternwatch itself runs in, to leave it off the map.
    pub fn self_pane(&self) -> Option<PaneId> {
        match self {
            Backend::Tern { .. } => std::env::var("TERN_PANE").ok()?.parse().ok(),
            Backend::Herdr { .. } => Some(herdr::pane_id(&std::env::var("HC_TERMINAL_ID").ok()?)),
        }
    }

    pub fn focus(&self, address: &str) -> Result<()> {
        match self {
            Backend::Tern { bin } => tern::focus(bin, address),
            Backend::Herdr { app, .. } => herdr::focus(app, address),
        }
    }

    pub fn send(&self, address: &str, text: &str) -> Result<()> {
        match self {
            Backend::Tern { bin } => tern::send(bin, address, text),
            Backend::Herdr { hc, .. } => herdr::send(hc, address, text),
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
            Backend::Herdr { hc, .. } => Box::new(herdr::spawn(hc.clone(), tx)),
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
