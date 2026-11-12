//! Reads herdr-core's terminals through `hc events`: agents, the state they report over OSC 7501,
//! and which agent started which. hc carries no message text, so this backend shows agents only.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::model::PaneId;
use crate::pane::{Pane, agent_cli};
use crate::source::Update;

const APP_BUNDLE: &str = "/Applications/Herdr.app/Contents/MacOS/Herdr";
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// `HC_BIN`, then `hc` on `PATH`, then herdr-core's link in `~/.local/bin`.
pub fn binary() -> String {
    find(
        "HC_BIN",
        "hc",
        std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/bin/hc")),
    )
}

/// `HERDR_APP_BIN`, then `herdr-app` on `PATH`, then the app bundle.
pub fn app_binary() -> String {
    find("HERDR_APP_BIN", "herdr-app", Some(APP_BUNDLE.into()))
}

fn find(variable: &str, name: &str, fallback: Option<std::path::PathBuf>) -> String {
    if let Ok(bin) = std::env::var(variable)
        && !bin.trim().is_empty()
    {
        return bin;
    }
    let on_path = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()));
    match fallback {
        Some(path) if !on_path && path.is_file() => path.to_string_lossy().into_owned(),
        _ => name.to_string(),
    }
}

/// Whether a herdr core answers.
pub fn running(bin: &str) -> bool {
    Command::new(bin)
        .args(["server", "status"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The model key of an hc address: a 53-bit FNV-1a hash, so it stays the same across restarts
/// and is exact as a JSON number.
pub fn pane_id(address: &str) -> PaneId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in address.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash & ((1 << 53) - 1)
}

/// Shows the terminal in the Herdr window.
pub fn focus(app: &str, address: &str) -> Result<()> {
    let args = json!({ "terminal": address }).to_string();
    run_quiet(app, &["do", "pane.focus", &args], "herdr-app pane.focus")
}

/// Pastes `text` into the terminal and presses Enter, as `hc send` does.
pub fn send(bin: &str, address: &str, text: &str) -> Result<()> {
    run_quiet(bin, &["send", address, text], "hc send")
}

fn run_quiet(bin: &str, args: &[&str], what: &str) -> Result<()> {
    let output = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {bin}"))?;
    if !output.status.success() {
        bail!("{what}: {}", error_text(&output.stderr));
    }
    Ok(())
}

/// hc writes errors as `{"error": {"message", "hint"}}`; anything else is passed on trimmed.
fn error_text(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
        return text.trim().to_string();
    };
    let field = |key: &str| {
        value
            .pointer(&format!("/error/{key}"))
            .and_then(Value::as_str)
    };
    match (field("message"), field("hint")) {
        (Some(message), Some(hint)) => format!("{message} ({hint})"),
        (Some(message), None) => message.to_string(),
        _ => text.trim().to_string(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The live terminals, from the first line of `hc events`.
pub fn terminals(bin: &str) -> Result<Vec<Pane>> {
    let mut child = Command::new(bin)
        .args(["events", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run `{bin} events`"))?;
    let mut first = Vec::new();
    let read =
        BufReader::new(child.stdout.take().expect("piped stdout")).read_until(b'\n', &mut first);
    let _ = child.kill();
    let output = child.wait_with_output()?;
    let mut terminals = Terminals::default();
    if read.is_err()
        || !terminals
            .apply(
                &serde_json::from_slice(&first).unwrap_or_default(),
                now_ms(),
            )
            .changed()
    {
        bail!("`{bin} events` failed: {}", error_text(&output.stderr));
    }
    Ok(terminals.panes(now_ms()))
}

/// What one line of `hc events` did to the terminals.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Changed,
    Unchanged,
    /// The core dropped events for this subscriber; a new subscription starts from a snapshot.
    Resubscribe,
}

impl Applied {
    fn changed(&self) -> bool {
        *self == Applied::Changed
    }
}

/// Terminal facts by address, as `hc events` reports them: a snapshot, then changes.
#[derive(Default)]
pub struct Terminals {
    by_address: BTreeMap<String, Value>,
    /// Events at or below the snapshot's sequence are already in it.
    seq: u64,
}

impl Terminals {
    pub fn apply(&mut self, line: &Value, now_ms: i64) -> Applied {
        let snapshot = Some(line)
            .filter(|line| line.get("terminals").is_some())
            .or_else(|| {
                line.get("params")
                    .filter(|params| params.get("terminals").is_some())
            });
        if let Some(snapshot) = snapshot {
            self.by_address = snapshot["terminals"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| Some((t.get("address")?.as_str()?.to_string(), t.clone())))
                .collect();
            self.seq = snapshot["seq"].as_u64().unwrap_or(0);
            return Applied::Changed;
        }
        match line.get("method").and_then(Value::as_str) {
            Some("event") => {}
            Some("missed") => return Applied::Resubscribe,
            // `heartbeat` and anything newer.
            _ => return Applied::Unchanged,
        }
        let params = &line["params"];
        if params["seq"].as_u64().is_some_and(|seq| seq <= self.seq) {
            return Applied::Unchanged;
        }
        let Some(address) = params["terminal"].as_str() else {
            return Applied::Unchanged;
        };
        let data = &params["data"];
        let kind = params["type"].as_str().unwrap_or_default();
        match kind {
            "created" => {
                self.by_address.insert(address.to_string(), data.clone());
                return Applied::Changed;
            }
            "removed" => {
                return match self.by_address.remove(address) {
                    Some(_) => Applied::Changed,
                    None => Applied::Unchanged,
                };
            }
            _ => {}
        }
        let Some(terminal) = self
            .by_address
            .get_mut(address)
            .and_then(Value::as_object_mut)
        else {
            return Applied::Unchanged;
        };
        // Each event type carries its own fields; `turn` also carries the agent's state word under
        // `state`, which is not the terminal's `live`/`finished` state.
        let copy = |terminal: &mut serde_json::Map<String, Value>, key: &str| match data.get(key) {
            Some(value) => {
                terminal.insert(key.to_string(), value.clone());
                Applied::Changed
            }
            None => Applied::Unchanged,
        };
        match kind {
            "foreground" => {
                terminal.insert("foreground".into(), data.clone());
                Applied::Changed
            }
            "status" => {
                terminal.insert("status_ms".into(), json!(now_ms));
                copy(terminal, "status")
            }
            "turn" | "title" | "labels" => copy(terminal, kind),
            "finished" | "lost" => {
                terminal.insert("state".into(), json!("finished"));
                Applied::Changed
            }
            "restarted" => {
                terminal.insert("state".into(), json!("live"));
                Applied::Changed
            }
            _ => Applied::Unchanged,
        }
    }

    /// Live terminals as panes; finished ones are gone from the window.
    pub fn panes(&self, now_ms: i64) -> Vec<Pane> {
        self.by_address
            .values()
            .filter(|t| t["state"] == "live")
            .filter_map(|t| pane(t, now_ms))
            .collect()
    }
}

fn pane(terminal: &Value, now_ms: i64) -> Option<Pane> {
    let address = terminal["address"].as_str()?;
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let status = &terminal["status"];
    let state = text(&status["state"]);
    let working = state.as_deref() == Some("working");
    let since = terminal["status_ms"]
        .as_i64()
        .or_else(|| terminal["created_ms"].as_i64())
        .unwrap_or(now_ms);
    let foreground = &terminal["foreground"];
    let argv: Vec<String> = foreground["argv"]
        .as_array()
        .or_else(|| terminal["argv"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|arg| arg.as_str().map(str::to_string))
        .collect();
    // herdr's `where` label: `<space> / <tab>`.
    let place = terminal
        .pointer("/labels/where")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (session, tab) = match place.rsplit_once(" / ") {
        Some((space, tab)) => (space.to_string(), tab.parse().unwrap_or(0)),
        None => (place.to_string(), 0),
    };
    // A remote terminal's parent is named as its own machine knows it.
    let machine = address.rsplit_once('/').map(|(machine, _)| machine);
    let parent = terminal["parent"].as_str().map(|parent| match machine {
        Some(machine) if !parent.contains('/') => pane_id(&format!("{machine}/{parent}")),
        _ => pane_id(parent),
    });
    Some(Pane {
        id: pane_id(address),
        address: address.to_string(),
        // omp titles its terminal `π > <title>` outside Tern.
        title: text(&terminal["title"])
            .map(|title| {
                title
                    .strip_prefix("π > ")
                    .map_or(title.clone(), str::to_string)
            })
            .or_else(|| text(&terminal["name"]))
            .unwrap_or_default(),
        cwd: text(&foreground["cwd"])
            .or_else(|| text(&terminal["cwd"]))
            .unwrap_or_default(),
        session,
        tab,
        tab_name: None,
        working,
        quiet_ms: if working {
            0
        } else {
            (now_ms - since).max(0) as u64
        },
        inspected_at: Some(Instant::now()),
        session_file: None,
        cli: text(&status["app"]).or_else(|| agent_cli(&argv)),
        status: state,
        parent,
    })
}

pub struct Guard {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Follows `hc events` on a thread, reconnecting with backoff, and sends every change as panes.
pub fn spawn(bin: String, tx: Sender<Update>) -> Guard {
    let stop = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(None));
    let worker = {
        let (stop, child) = (Arc::clone(&stop), Arc::clone(&child));
        thread::spawn(move || run(&bin, &tx, &stop, &child))
    };
    Guard {
        stop,
        child,
        worker: Some(worker),
    }
}

enum End {
    /// Nobody receives updates any more.
    Closed,
    Resubscribe,
    Failed(String),
}

fn run(bin: &str, tx: &Sender<Update>, stop: &AtomicBool, slot: &Mutex<Option<Child>>) {
    let mut baseline = false;
    let mut backoff = RETRY_MIN;
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        match follow(bin, tx, stop, slot, &mut baseline) {
            End::Closed => return,
            End::Resubscribe => continue,
            End::Failed(message) => {
                if stop.load(Ordering::SeqCst) || tx.send(Update::Error(message)).is_err() {
                    return;
                }
            }
        }
        if started.elapsed() >= RETRY_MAX {
            backoff = RETRY_MIN;
        }
        let until = Instant::now() + backoff;
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

/// One subscription, until it ends.
fn follow(
    bin: &str,
    tx: &Sender<Update>,
    stop: &AtomicBool,
    slot: &Mutex<Option<Child>>,
    baseline: &mut bool,
) -> End {
    let spawned = Command::new(bin)
        .args(["events", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return End::Failed(format!(
                "cannot start `{bin} events`: {error}; reconnecting"
            ));
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    *slot.lock().unwrap() = Some(child);
    // A guard dropped before the child was stored could not kill it.
    let end = if stop.load(Ordering::SeqCst) {
        End::Closed
    } else {
        read(stdout, tx, baseline)
    };
    if let Some(mut child) = slot.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let detail = errors
        .join()
        .map(|text| error_text(&text))
        .unwrap_or_default();
    match end {
        End::Failed(_) if detail.is_empty() => {
            End::Failed("hc events disconnected; reconnecting".into())
        }
        End::Failed(_) => End::Failed(format!("hc events disconnected: {detail}; reconnecting")),
        end => end,
    }
}

fn read(stdout: impl Read, tx: &Sender<Update>, baseline: &mut bool) -> End {
    let mut terminals = Terminals::default();
    for line in BufReader::new(stdout).split(b'\n') {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match terminals.apply(&value, now_ms()) {
            Applied::Unchanged => {}
            Applied::Resubscribe => return End::Resubscribe,
            Applied::Changed => {
                if tx.send(Update::Panes(terminals.panes(now_ms()))).is_err() {
                    return End::Closed;
                }
                if !*baseline {
                    *baseline = true;
                    if tx.send(Update::Baseline).is_err() {
                        return End::Closed;
                    }
                }
            }
        }
    }
    End::Failed(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64, terminal: &str, kind: &str, data: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "event",
               "params": {"seq": seq, "terminal": terminal, "type": kind, "data": data}})
    }

    fn snapshot() -> Value {
        json!({"channel": null, "seq": 40, "terminals": [
            {"address": "t16", "argv": ["/bin/zsh", "-l"], "cwd": "/w", "parent": null, "title": "",
             "state": "live", "status": null, "status_ms": 1_000,
             "foreground": {"argv": ["/bin/zsh", "-l"], "cwd": "/w/oops", "name": "zsh"},
             "labels": {"where": "oops-system / 1"}},
            {"address": "vps/t5", "argv": ["omp"], "cwd": "/home", "parent": null,
             "title": "π > FACT: Diagnose", "state": "live", "status": {"app": "omp", "state": "done"},
             "status_ms": 4_000, "labels": {"where": "tmp / 2"}},
            {"address": "vps/t6", "argv": ["codex"], "cwd": "/home", "parent": "t5", "title": "",
             "name": "review", "state": "live", "status": {"app": "codex", "state": "working"},
             "status_ms": 9_000, "labels": {}}
        ]})
    }

    #[test]
    fn maps_terminals_to_panes_with_state_parent_and_place() {
        let mut terminals = Terminals::default();
        assert_eq!(terminals.apply(&snapshot(), 10_000), Applied::Changed);
        let panes = terminals.panes(10_000);
        let by = |address: &str| panes.iter().find(|p| p.address == address).unwrap();

        let shell = by("t16");
        assert_eq!(
            (shell.session.as_str(), shell.tab, shell.cwd.as_str()),
            ("oops-system", 1, "/w/oops")
        );
        assert_eq!(
            (shell.cli.as_deref(), shell.status.as_deref()),
            (None, None)
        );
        assert_eq!(shell.quiet_ms, 9_000);

        let omp = by("vps/t5");
        assert_eq!(
            (omp.cli.as_deref(), omp.state(), omp.working),
            (Some("omp"), "done", false)
        );
        assert_eq!(omp.id, pane_id("vps/t5"));
        assert_eq!(omp.title, "FACT: Diagnose");

        // The child names its parent as its machine does.
        let review = by("vps/t6");
        assert_eq!(review.title, "review");
        assert_eq!(review.parent, Some(omp.id));
        assert!(review.working);
        assert_eq!(review.quiet_ms, 0);
    }

    #[test]
    fn applies_changes_in_order_and_skips_what_the_snapshot_holds() {
        let mut terminals = Terminals::default();
        terminals.apply(&snapshot(), 10_000);
        // Already in the snapshot.
        let stale = event(40, "t16", "title", json!({"title": "old"}));
        assert_eq!(terminals.apply(&stale, 10_000), Applied::Unchanged);

        let created = json!({"address": "t1p", "argv": ["sh"], "cwd": "/w", "state": "live",
                             "status": null, "status_ms": null, "created_ms": 10_000, "parent": "t16"});
        assert_eq!(
            terminals.apply(&event(41, "t1p", "created", created), 10_000),
            Applied::Changed
        );
        let status = json!({"records": {}, "reported": true,
                            "status": {"app": "probe", "kind": "question", "state": "blocked"}});
        assert_eq!(
            terminals.apply(&event(42, "t1p", "status", status), 12_000),
            Applied::Changed
        );
        // `turn` carries the agent's word under `state`; the terminal stays live.
        terminals.apply(
            &event(43, "t1p", "turn", json!({"state": "blocked", "turn": 1})),
            12_000,
        );
        terminals.apply(
            &event(44, "t1p", "title", json!({"title": "probe title"})),
            12_000,
        );
        let panes = terminals.panes(15_000);
        let probe = panes.iter().find(|p| p.address == "t1p").unwrap();
        assert_eq!(
            (probe.title.as_str(), probe.state(), probe.cli.as_deref()),
            ("probe title", "blocked", Some("probe"))
        );
        assert_eq!(probe.quiet_ms, 3_000);
        assert_eq!(probe.parent, Some(pane_id("t16")));

        let finished =
            json!({"exit_code": 0, "finished_ms": 16_000, "reason": "exited", "signal": null});
        terminals.apply(&event(45, "t1p", "finished", finished), 16_000);
        assert!(terminals.panes(16_000).iter().all(|p| p.address != "t1p"));
        assert_eq!(
            terminals.apply(&event(46, "t1p", "removed", json!({})), 16_000),
            Applied::Changed
        );
        assert_eq!(
            terminals.apply(&event(47, "t1p", "removed", json!({})), 16_000),
            Applied::Unchanged
        );
    }

    #[test]
    fn heartbeats_are_quiet_and_missed_events_resubscribe() {
        let mut terminals = Terminals::default();
        let heartbeat = json!({"jsonrpc": "2.0", "method": "heartbeat", "params": {}});
        assert_eq!(terminals.apply(&heartbeat, 0), Applied::Unchanged);
        let missed = json!({"jsonrpc": "2.0", "method": "missed", "params": {"seq": 90}});
        assert_eq!(terminals.apply(&missed, 0), Applied::Resubscribe);
        // A fresh snapshot replaces everything held before.
        terminals.apply(&snapshot(), 0);
        let fresh = json!({"jsonrpc": "2.0", "method": "missed",
                           "params": {"seq": 91, "terminals": [{"address": "t2", "state": "live"}]}});
        assert_eq!(terminals.apply(&fresh, 0), Applied::Changed);
        let addresses: Vec<_> = terminals.panes(0).into_iter().map(|p| p.address).collect();
        assert_eq!(addresses, ["t2"]);
    }

    #[test]
    fn pane_ids_are_stable_and_exact_in_json() {
        assert_eq!(pane_id("vps/t7"), pane_id("vps/t7"));
        assert_ne!(pane_id("t7"), pane_id("vps/t7"));
        assert!(pane_id("t7") < 1 << 53);
    }

    #[test]
    fn reads_hc_errors() {
        let json =
            br#"{"error": {"kind": "not_found", "message": "no terminal t9", "hint": "hc ls"}}"#;
        assert_eq!(error_text(json), "no terminal t9 (hc ls)");
        assert_eq!(error_text(b" plain failure\n"), "plain failure");
    }
}
