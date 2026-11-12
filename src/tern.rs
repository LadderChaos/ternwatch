//! Reads Tern's pane state through the official `tern` CLI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::pane::Pane;

const APP_BINARY: &str = "/Applications/Tern.app/Contents/MacOS/tern";

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
                    address: id.to_string(),
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

/// Moves Tern's focus to the pane, so the user can work with the agent directly.
pub fn focus(bin: &str, pane: &str) -> Result<()> {
    run_quiet(bin, &["focus", pane])
}

/// Types `text` into the pane and presses Enter, as `tern run` does.
pub fn send(bin: &str, pane: &str, text: &str) -> Result<()> {
    run_quiet(bin, &["run", pane, text])
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
}
