//! A terminal pane as both backends report it: a Tern pane or a herdr-core terminal.

use std::path::PathBuf;
use std::time::Instant;

use crate::model::PaneId;

#[derive(Clone, Debug, Default)]
pub struct Pane {
    /// Model key. Tern's pane number; for herdr, derived from the terminal address.
    pub id: PaneId,
    /// What the backend's commands take: the Tern pane number, or an hc address (`t7`, `vps/t7`).
    pub address: String,
    pub title: String,
    pub cwd: String,
    pub session: String,
    pub tab: u32,
    pub tab_name: Option<String>,
    pub working: bool,
    pub quiet_ms: u64,
    /// When `quiet_ms` was sampled; age keeps advancing without querying the backend.
    pub inspected_at: Option<Instant>,
    pub session_file: Option<PathBuf>,
    /// The agent CLI in the pane (`codex`, `claude`, `codex (acp)`, …) when it has no omp
    /// transcript: Tern's foreground process, or the app a herdr terminal reports.
    pub cli: Option<String>,
    /// herdr's OSC 7501 state word: `working`, `blocked`, `done`, `idle` or `error`.
    pub status: Option<String>,
    /// The herdr terminal that started this one with `hc run`.
    pub parent: Option<PaneId>,
}

impl Pane {
    pub fn quiet_ms(&self) -> u64 {
        self.quiet_ms
            .saturating_add(self.inspected_at.map_or(0, |at| {
                at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
            }))
    }

    /// The state word: `working`, herdr's reported word, or `idle`.
    pub fn state(&self) -> &str {
        if self.working {
            return "working";
        }
        self.status.as_deref().unwrap_or("idle")
    }
}

const AGENT_CLIS: [&str; 14] = [
    "omp",
    "pi",
    "claude",
    "codex",
    "gemini",
    "opencode",
    "aider",
    "hermes",
    "goose",
    "amp",
    "cursor-agent",
    "copilot",
    "qwen",
    "crush",
];

/// Which coding agent a command line runs: `claude`, `codex`, or `codex (acp)` for Tern's ACP
/// bridge (`tern acp --fork codex:<session>`). Node launchers are matched by their script path.
pub fn agent_cli(argv: &[String]) -> Option<String> {
    let base = |arg: &str| arg.rsplit('/').next().unwrap_or(arg).to_string();
    let program = base(argv.first()?);
    if program == "tern" && argv.get(1).map(String::as_str) == Some("acp") {
        let inner = argv
            .iter()
            .find_map(|arg| arg.split_once(':').map(|(cli, _)| cli.to_string()));
        let inner = inner.filter(|cli| AGENT_CLIS.contains(&cli.as_str()));
        return Some(
            inner
                .map(|cli| format!("{cli} (acp)"))
                .unwrap_or_else(|| "acp".into()),
        );
    }
    if AGENT_CLIS.contains(&program.as_str()) {
        return Some(program);
    }
    if matches!(program.as_str(), "node" | "bun" | "deno") {
        return argv[1..].iter().find_map(|arg| {
            AGENT_CLIS
                .iter()
                .find(|cli| arg.contains(&format!("/{cli}")))
                .map(|cli| cli.to_string())
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_age_advances_without_another_inspection() {
        let pane = Pane {
            quiet_ms: 12,
            inspected_at: Some(Instant::now() - std::time::Duration::from_secs(2)),
            ..Pane::default()
        };
        assert!(pane.quiet_ms() >= 2012);
        let saturated = Pane {
            quiet_ms: u64::MAX,
            ..pane
        };
        assert_eq!(saturated.quiet_ms(), u64::MAX);
    }

    #[test]
    fn names_agent_clis_including_acp_and_node_launchers() {
        let argv = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let acp = argv(&[
            "/Applications/Tern.app/Contents/MacOS/tern",
            "acp",
            "--fork",
            "codex:01a1",
            "--handoff",
            "0:1",
        ]);
        assert_eq!(agent_cli(&acp).as_deref(), Some("codex (acp)"));
        assert_eq!(
            agent_cli(&argv(&["claude", "--resume"])).as_deref(),
            Some("claude")
        );
        let node = argv(&[
            "node",
            "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js",
        ]);
        assert_eq!(agent_cli(&node).as_deref(), Some("claude"));
        assert_eq!(agent_cli(&argv(&["/bin/zsh", "-l"])), None);
        assert_eq!(agent_cli(&argv(&["tern", "ls"])), None);
    }
}
