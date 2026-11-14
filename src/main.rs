mod app;
mod backend;
mod herdr;
mod model;
mod pane;
mod source;
mod tern;
mod transcript;
mod ui;

use std::io::{IsTerminal, Write, stdin, stdout};
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use serde_json::{Value, json};

use crate::app::App;
use crate::backend::Backend;
use crate::model::{Kind, Model, Node};
use crate::source::Update;

const HELP: &str = "\
muster — live map of the coding agents in your Tern or herdr window and the messages between them

usage:
  muster [--backend B] [--no-animation] [--all-panes] [--history MB]
  muster --json [--follow] [--backend B] [--history MB]
  muster --frame COLSxROWS [--backend B] [--all-panes] [--history MB]
  muster --send ADDRESS TEXT [--backend B]
  muster --focus ADDRESS [--backend B]

With Tern, reads `tern inspect --json` for panes and omp's session transcripts for traffic:
cross-pane messages (`tern run` / `tern send`), omp IRC, subagent spawns and yields.
With herdr, reads `hc events` for agents, their reported state and which agent started which;
herdr reports no message text, so no messages are shown.
It never writes to a pane unless you press m and send a message, or use --send.

options:
  --backend B      tern, herdr or auto (default): the window muster runs in, else a running
                   Tern, else a running herdr core
  --json           print a snapshot (agents and messages) as JSON and exit
  --follow         with --json, stream snapshots as newline-delimited JSON
  --frame WxH      print one rendered frame as text and exit, e.g. --frame 160x48
  --send A TEXT    type TEXT into the pane at address A (a JSON agent's `address`) and press Enter
  --focus A        show the pane at address A in its window
  --no-animation   no moving packets or spinners; state stays readable
  --all-panes      show every terminal pane, not only agents
  --history MB     transcript history read per agent on start (default 4)
  -h, --help       this help
  -V, --version    version

environment:
  TERN_BIN       the tern CLI (default: tern on PATH, then the Tern app bundle)
  HC_BIN         herdr-core's hc (default: hc on PATH, then ~/.local/bin/hc)
  HERDR_APP_BIN  the herdr-app CLI, for --focus (default: herdr-app on PATH, then Herdr.app)
  NO_COLOR       draw without color
";

enum Action {
    Send(String, String),
    Focus(String),
}

struct Args {
    backend: String,
    json: bool,
    follow: bool,
    frame: Option<(u16, u16)>,
    action: Option<Action>,
    animate: bool,
    all_panes: bool,
    history: u64,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args {
        backend: "auto".into(),
        json: false,
        frame: None,
        follow: false,
        action: None,
        animate: true,
        all_panes: false,
        history: 4 << 20,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--backend" => {
                let Some(backend) = iter.next() else {
                    bail!("--backend needs tern, herdr or auto");
                };
                args.backend = backend;
            }
            "--send" => {
                let (Some(address), Some(text)) = (iter.next(), iter.next()) else {
                    bail!("--send needs an address and the text, for example --send 7 \"hi\"");
                };
                args.action = Some(Action::Send(address, text));
            }
            "--focus" => {
                let Some(address) = iter.next() else {
                    bail!("--focus needs an address, for example --focus 7");
                };
                args.action = Some(Action::Focus(address));
            }
            "--json" => args.json = true,
            "--follow" => args.follow = true,
            "--frame" => {
                let size = iter.next().and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                });
                let Some(size) = size.filter(|(w, h): &(u16, u16)| *w >= 60 && *h >= 16) else {
                    bail!("--frame needs COLSxROWS of at least 60x16, for example --frame 160x48");
                };
                args.frame = Some(size);
            }
            "--no-animation" => args.animate = false,
            "--all-panes" => args.all_panes = true,
            "--history" => {
                let Some(mb) = iter
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|v| *v > 0.0)
                else {
                    bail!("--history needs a size in MB, for example --history 8");
                };
                args.history = (mb * 1_048_576.0) as u64;
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("muster {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => bail!("unknown argument {other}; see muster --help"),
        }
    }
    if args.follow && !args.json {
        bail!("--follow requires --json");
    }
    Ok(Some(args))
}

fn main() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let backend = Backend::resolve(&args.backend)?;
    match &args.action {
        Some(Action::Send(address, text)) => return backend.send(address, text),
        Some(Action::Focus(address)) => return backend.focus(address),
        None => {}
    }
    if args.json {
        return if args.follow {
            follow(&backend, args.history)
        } else {
            snapshot(&backend, args.history)
        };
    }
    if let Some((width, height)) = args.frame {
        return frame(backend, args.history, args.all_panes, width, height);
    }
    if !stdout().is_terminal() || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        bail!("muster needs a terminal; use --json for a snapshot");
    }
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let (tx, rx) = mpsc::channel();
    let _watch = backend.spawn(args.history, tx);
    let mut app = App::new(backend, args.animate, color, args.all_panes);

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = (|| -> Result<()> {
        while !app.quit {
            for update in rx.try_iter() {
                apply(&mut app, update);
            }
            app.absorb_changes();
            app.tick();
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            let wait = if app.animate {
                Duration::from_millis(40)
            } else {
                Duration::from_millis(250)
            };
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                    Event::Mouse(mouse) => app.handle_mouse(mouse),
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn apply(app: &mut App, update: Update) {
    match update {
        Update::Panes(panes) => {
            app.error = None;
            app.model.apply_panes(panes);
        }
        Update::Events { pane, sub, events } => app.model.apply(pane, sub.as_deref(), events),
        Update::Baseline => {
            app.model.live = true;
            app.loaded = true;
        }
        Update::Error(error) => app.error = Some(error),
    }
}

/// One full read of panes and transcripts, as the live view has after loading history.
fn load(backend: &Backend, history: u64) -> Result<Model> {
    let mut model = Model::default();
    match backend {
        Backend::Herdr { hc, .. } => model.apply_panes(herdr::terminals(hc)?),
        Backend::Tern { bin } => load_tern(&mut model, bin, history)?,
    }
    model.live = true;
    Ok(model)
}

fn load_tern(model: &mut Model, bin: &str, history: u64) -> Result<()> {
    let mut panes = tern::inspect(bin)?;
    for pane in panes.iter_mut().filter(|p| p.session_file.is_none()) {
        pane.cli = tern::foreground(bin, pane.id).and_then(|argv| pane::agent_cli(&argv));
    }
    let mains: Vec<_> = panes
        .iter()
        .filter_map(|p| Some((p.id, p.session_file.clone()?)))
        .collect();
    model.apply_panes(panes);
    for (pane, file) in mains {
        let events = transcript::Tail::new(file.clone(), history)
            .poll()
            .iter()
            .flat_map(transcript::parse)
            .collect();
        model.apply(pane, None, events);
        for (name, sub) in transcript::subagent_files(&file) {
            if transcript::untouched_for(&sub, source::STALE_SUB) {
                continue;
            }
            let events = transcript::Tail::new(sub, history / 4)
                .poll()
                .iter()
                .flat_map(transcript::parse)
                .collect();
            model.apply(pane, Some(&name), events);
        }
    }
    Ok(())
}

/// One rendered frame of the current state, as plain text.
fn frame(backend: Backend, history: u64, all_panes: bool, width: u16, height: u16) -> Result<()> {
    let model = load(&backend, history)?;
    let mut app = App::new(backend, false, false, all_panes);
    app.model = model;
    app.loaded = true;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    terminal.draw(|frame| ui::draw(frame, &mut app))?;
    let buffer = terminal.backend().buffer();
    for y in 0..height {
        let line: String = (0..width)
            .filter_map(|x| buffer.cell((x, y)))
            .map(|cell| cell.symbol())
            .collect();
        println!("{}", line.trim_end());
    }
    Ok(())
}

/// One full read of panes and transcripts, printed as JSON.
fn snapshot(backend: &Backend, history: u64) -> Result<()> {
    let model = load(backend, history)?;
    let value = snapshot_value(&model, backend.name());
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Stream the initial history and subsequent watcher updates as complete snapshots.
fn follow(backend: &Backend, history: u64) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let watch = backend.spawn(history, tx);
    // A parent that holds a pipe on stdin can stop the watcher by closing it, even when the
    // parent dies: stop the backend's child commands, then exit.
    let _watch = if stdin_is_pipe() {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stdin().lock(), &mut std::io::sink());
            drop(watch);
            std::process::exit(0);
        });
        None
    } else {
        Some(watch)
    };
    let mut model = Model::default();
    let mut loaded = false;
    let mut error = None;
    let stdout = stdout();
    let mut out = stdout.lock();
    while let Ok(first) = rx.recv() {
        // Apply all updates already available before serializing the complete state.
        for update in std::iter::once(first).chain(rx.try_iter()) {
            match update {
                Update::Panes(panes) => {
                    error = None;
                    model.apply_panes(panes);
                }
                Update::Events { pane, sub, events } => {
                    model.apply(pane, sub.as_deref(), events);
                }
                Update::Baseline => {
                    model.live = true;
                    loaded = true;
                }
                Update::Error(message) => {
                    eprintln!("{message}");
                    error = Some(message);
                }
            }
        }
        // Animation changes are only consumed by the interactive interface.
        model.changes.clear();
        if loaded || error.is_some() {
            let mut snapshot = snapshot_value(&model, backend.name());
            if let Some(error) = &error {
                snapshot["error"] = json!(error);
            }
            serde_json::to_writer(&mut out, &snapshot)?;
            writeln!(out)?;
            out.flush()?;
        }
    }
    bail!("live watcher stopped unexpectedly")
}

fn stdin_is_pipe() -> bool {
    stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
        .and_then(|file| file.metadata())
        .is_ok_and(|meta| meta.file_type().is_fifo())
}

fn snapshot_value(model: &Model, backend: &str) -> Value {
    let now = App::now_ms();
    let node = |node: &Node| match node {
        Node::Pane(pane) => json!({"pane": pane}),
        Node::Sub(pane, name) => json!({"pane": pane, "subagent": name}),
    };
    let agents: Vec<Value> = model
        .agents
        .values()
        .filter(|a| a.is_agent() || model.messages.iter().any(|m| m.from.pane() == a.pane.id || m.to.pane() == a.pane.id))
        .map(|a| {
            let (_, label, description) = a.label();
            json!({
                "pane": a.pane.id, "address": a.pane.address, "label": label, "title": a.pane.title, "description": description,
                "cwd": a.pane.cwd, "session": a.pane.session, "tab": a.pane.tab, "open": a.present,
                "omp": a.has_transcript(), "cli": a.pane.cli, "working": a.pane.working, "status": a.pane.state(),
                "parent": a.pane.parent, "quiet_ms": a.pane.quiet_ms,
                "model": a.model, "tokens": a.tokens, "tool": a.tool().map(|(t, i)| json!({"name": t, "intent": i})),
                "todo": a.todo.map(|(d, t)| json!({"done": d, "total": t})),
                "subagents": a.subs.iter().map(|(n, s)| json!({"name": n, "running": s.running(now), "done": s.done})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut messages: Vec<_> = model.messages.iter().collect();
    messages.sort_by_key(|m| (m.time(), m.seq));
    let messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "id": m.seq, "repeat_of": m.repeat_of,
                "kind": match m.kind { Kind::Pane => "pane", Kind::Irc => "irc", Kind::Spawn => "spawn", Kind::Yield => "yield" },
                "from": node(&m.from), "to": node(&m.to), "text": m.text,
                "sent_ms": m.sent, "delivered_ms": m.delivered, "failed": m.failed, "outcome": m.outcome,
                "delivery_observable": model.agents.get(&m.to.pane()).is_some_and(|a| a.has_transcript()),
                "reply": m.reply_to.is_some(), "replied": m.replied,
            })
        })
        .collect();
    json!({"backend": backend, "agents": agents, "messages": messages})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_keep_repeat_records_and_publish_root_ids() {
        let mut model = Model::default();
        for (ts, body) in [(1_000, "one\ntwo"), (2_000, "one two")] {
            model.apply(
                2,
                None,
                vec![transcript::Event::User {
                    ts,
                    text: format!("From 1: {body}"),
                }],
            );
        }
        let snapshot = snapshot_value(&model, "tern");
        let messages = snapshot["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["id"].as_u64(), Some(model.messages[0].seq));
        assert_eq!(messages[1]["id"].as_u64(), Some(model.messages[1].seq));
        assert!(messages[0]["repeat_of"].is_null());
        assert_eq!(messages[1]["repeat_of"], messages[0]["id"]);
        assert_eq!(messages[0]["text"], "one\ntwo");
        assert_eq!(messages[1]["text"], "one two");

        model.apply(
            1,
            None,
            vec![transcript::Event::ToolCall {
                ts: 900,
                id: "late-send".into(),
                name: "bash".into(),
                args: json!({"command": "tern run 2 'one two'"}),
                intent: String::new(),
            }],
        );
        let updated = snapshot_value(&model, "tern");
        let updated_messages = updated["messages"].as_array().unwrap();
        assert_eq!(updated_messages.len(), 2);
        for (before, after) in messages.iter().zip(updated_messages) {
            assert_eq!(before["id"], after["id"]);
            assert_eq!(before["text"], after["text"]);
            assert!(after["repeat_of"].is_null());
        }
    }
}
