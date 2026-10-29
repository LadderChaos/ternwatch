mod app;
mod model;
mod source;
mod tern;
mod transcript;
mod ui;

use std::io::{IsTerminal, stdout};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use serde_json::{Value, json};

use crate::app::App;
use crate::model::{Kind, Model, Node};
use crate::source::Update;

const HELP: &str = "\
ternwatch — live map of the coding agents in your Tern window and the messages between them

usage:
  ternwatch [--no-animation] [--all-panes] [--history MB]
  ternwatch --json [--history MB]
  ternwatch --frame COLSxROWS [--all-panes] [--history MB]

Reads `tern inspect --json` for panes and omp's session transcripts for traffic:
cross-pane messages (`tern run` / `tern send`), omp IRC, subagent spawns and yields.
It never writes to a pane unless you press m and send a message.

options:
  --json           print one snapshot (agents and messages) as JSON and exit
  --frame WxH      print one rendered frame as text and exit, e.g. --frame 160x48
  --no-animation   no moving packets or spinners; state stays readable
  --all-panes      show every terminal pane, not only omp agents
  --history MB     transcript history read per agent on start (default 4)
  -h, --help       this help
  -V, --version    version

environment:
  TERN_BIN   the tern CLI (default: tern on PATH, then the Tern app bundle)
  NO_COLOR   draw without color
";

struct Args {
    json: bool,
    frame: Option<(u16, u16)>,
    animate: bool,
    all_panes: bool,
    history: u64,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args {
        json: false,
        frame: None,
        animate: true,
        all_panes: false,
        history: 4 << 20,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => args.json = true,
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
                println!("ternwatch {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => bail!("unknown argument {other}; see ternwatch --help"),
        }
    }
    Ok(Some(args))
}

fn main() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let bin = tern::binary();
    if args.json {
        return snapshot(&bin, args.history);
    }
    if let Some((width, height)) = args.frame {
        return frame(bin, args.history, args.all_panes, width, height);
    }
    if !stdout().is_terminal() || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        bail!("ternwatch needs a terminal; use --json for a snapshot");
    }
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let (tx, rx) = mpsc::channel();
    source::spawn(
        source::Options {
            bin: bin.clone(),
            history: args.history,
        },
        tx,
    );
    let mut app = App::new(bin, args.animate, color, args.all_panes);

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
fn load(bin: &str, history: u64) -> Result<Model> {
    let mut model = Model::default();
    let mut panes = tern::inspect(bin)?;
    for pane in panes.iter_mut().filter(|p| p.session_file.is_none()) {
        pane.cli = tern::foreground(bin, pane.id).and_then(|argv| tern::agent_cli(&argv));
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
    model.live = true;
    Ok(model)
}

/// One rendered frame of the current state, as plain text.
fn frame(bin: String, history: u64, all_panes: bool, width: u16, height: u16) -> Result<()> {
    let model = load(&bin, history)?;
    let mut app = App::new(bin, false, false, all_panes);
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
fn snapshot(bin: &str, history: u64) -> Result<()> {
    let model = load(bin, history)?;
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
                "pane": a.pane.id, "label": label, "title": a.pane.title, "description": description,
                "cwd": a.pane.cwd, "session": a.pane.session, "tab": a.pane.tab, "open": a.present,
                "omp": a.has_transcript(), "cli": a.pane.cli, "working": a.pane.working, "quiet_ms": a.pane.quiet_ms,
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
                "kind": match m.kind { Kind::Pane => "pane", Kind::Irc => "irc", Kind::Spawn => "spawn", Kind::Yield => "yield" },
                "from": node(&m.from), "to": node(&m.to), "text": m.text,
                "sent_ms": m.sent, "delivered_ms": m.delivered, "failed": m.failed, "outcome": m.outcome,
                "delivery_observable": model.agents.get(&m.to.pane()).is_some_and(|a| a.has_transcript()),
                "reply": m.reply_to.is_some(), "replied": m.replied,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"agents": agents, "messages": messages}))?
    );
    Ok(())
}
