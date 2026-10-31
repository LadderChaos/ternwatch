//! Reads Tern's pane state through the official `tern` CLI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const APP_BINARY: &str = "/Applications/Tern.app/Contents/MacOS/tern";

/// One Tern pane, merged from the `sessions` tree and the `panes` list of `tern inspect --json`.
#[derive(Clone, Debug, Default)]
pub struct Pane {
    pub id: u64,
    pub title: String,
    pub cwd: String,
    pub session: String,
    pub tab: u32,
    pub tab_name: Option<String>,
    pub working: bool,
    pub quiet_ms: u64,
    /// When `quiet_ms` was sampled; age keeps advancing without querying Tern.
    pub inspected_at: Option<Instant>,
    pub session_file: Option<PathBuf>,
    /// The agent CLI in the foreground when the pane has no omp transcript (`codex`, `claude`,
    /// `codex (acp)`, …), from `tern process`.
    pub cli: Option<String>,
}

impl Pane {
    pub fn quiet_ms(&self) -> u64 {
        self.quiet_ms
            .saturating_add(self.inspected_at.map_or(0, |at| {
                at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
            }))
    }
}

/// `TERN_BIN`, then `tern` on `PATH`, then the app bundle.
pub fn binary() -> String {
    if let Ok(bin) = std::env::var("TERN_BIN")
        && !bin.trim().is_empty()
    {
        return bin;
    }
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("tern").is_file()))
        .unwrap_or(false);
    if !on_path && Path::new(APP_BINARY).is_file() {
        return APP_BINARY.to_string();
    }
    "tern".to_string()
}

pub fn inspect(bin: &str) -> Result<Vec<Pane>> {
    let output = Command::new(bin)
        .args(["inspect", "--json"])
        .output()
        .with_context(|| format!("cannot run `{bin} inspect --json`"))?;
    if !output.status.success() {
        bail!(
            "`tern inspect --json` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root: Value =
        serde_json::from_slice(&output.stdout).context("tern inspect returned invalid JSON")?;
    Ok(parse_inspect(&root))
}

pub fn parse_inspect(root: &Value) -> Vec<Pane> {
    let mut panes: Vec<Pane> = Vec::new();
    for session in array(root, "sessions") {
        let session_name = str_of(session, "name");
        for tab in array(session, "tabs") {
            let tab_number = tab.get("number").and_then(Value::as_u64).unwrap_or(0) as u32;
            let tab_name = tab.get("name").and_then(Value::as_str).map(str::to_string);
            for block in array(tab, "blocks") {
                let Some(id) = block.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                // Browser and file blocks have no program; they are not terminals.
                if block.get("program").and_then(Value::as_str).is_none() {
                    continue;
                }
                panes.push(Pane {
                    id,
                    title: str_of(block, "title"),
                    cwd: str_of(block, "cwd"),
                    session: session_name.clone(),
                    tab: tab_number,
                    tab_name: tab_name.clone(),
                    ..Pane::default()
                });
            }
        }
    }
    for live in array(root, "panes") {
        let Some(id) = live.get("pane").and_then(Value::as_u64) else {
            continue;
        };
        let Some(pane) = panes.iter_mut().find(|pane| pane.id == id) else {
            continue;
        };
        pane.working = live
            .get("working")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        pane.quiet_ms = live.get("quiet_ms").and_then(Value::as_u64).unwrap_or(0);
        pane.inspected_at = Some(Instant::now());
        pane.session_file = live
            .get("omp_session_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        let title = str_of(live, "title");
        if !title.is_empty() {
            pane.title = title;
        }
    }
    panes
}

/// The pane's foreground command line, from `tern process <pane> --json`.
pub fn foreground(bin: &str, pane: u64) -> Option<Vec<String>> {
    let output = Command::new(bin)
        .args(["process", &pane.to_string(), "--json"])
        .output()
        .ok()?;
    let root: Value = serde_json::from_slice(&output.stdout).ok()?;
    let argv = root.pointer("/foreground/argv")?.as_array()?;
    Some(
        argv.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
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

/// Moves Tern's focus to the pane, so the user can work with the agent directly.
pub fn focus(bin: &str, pane: u64) -> Result<()> {
    run_quiet(bin, &["focus", &pane.to_string()])
}

/// Types `text` into the pane and presses Enter, as `tern run` does.
pub fn send(bin: &str, pane: u64, text: &str) -> Result<()> {
    run_quiet(bin, &["run", &pane.to_string(), text])
}

fn run_quiet(bin: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(bin).args(args).output()?;
    if !output.status.success() {
        bail!(
            "tern {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn array<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn str_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn merges_live_state_into_blocks_and_skips_browser_blocks() {
        let root = json!({
            "sessions": [{"name": "Chaos", "tabs": [{"number": 2, "name": "rwa", "blocks": [
                {"id": 7, "title": "old", "cwd": "/w", "program": "/bin/zsh", "exited": null},
                {"id": 8, "title": "https://x", "cwd": "/w"}
            ]}]}],
            "panes": [{"pane": 7, "title": "omp: new", "working": true, "quiet_ms": 12,
                       "omp_session_file": "/s/a.jsonl"}]
        });
        let panes = parse_inspect(&root);
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert_eq!((pane.id, pane.tab, pane.title.as_str()), (7, 2, "omp: new"));
        assert!(pane.working);
        assert_eq!(pane.session_file.as_deref(), Some(Path::new("/s/a.jsonl")));
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
