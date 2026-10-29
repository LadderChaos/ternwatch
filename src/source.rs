//! Background collection: polls `tern inspect` and tails every agent's transcripts.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::model::PaneId;
use crate::tern::{self, Pane};
use crate::transcript::{self, Event, Tail};

pub enum Update {
    Panes(Vec<Pane>),
    Events {
        pane: PaneId,
        sub: Option<String>,
        events: Vec<Event>,
    },
    /// The first full read of history is done; later events are live.
    Baseline,
    Error(String),
}

pub struct Options {
    pub bin: String,
    /// Bytes read back from the end of each main transcript on start.
    pub history: u64,
}

const INSPECT_EVERY: Duration = Duration::from_millis(500);
const SCAN_SUBS_EVERY: Duration = Duration::from_secs(2);
const TAIL_EVERY: Duration = Duration::from_millis(200);
/// How often to re-check which agent CLI runs in a pane without an omp transcript.
const PROCESS_EVERY: Duration = Duration::from_secs(5);
/// Subagent transcripts untouched this long are finished history; they are not read.
pub const STALE_SUB: Duration = Duration::from_secs(24 * 3600);

struct Source {
    pane: PaneId,
    sub: Option<String>,
    tail: Tail,
}

pub fn spawn(options: Options, tx: Sender<Update>) {
    thread::spawn(move || {
        let mut sources: HashMap<PathBuf, Source> = HashMap::new();
        let mut stale: HashSet<PathBuf> = HashSet::new();
        let mut clis: HashMap<PaneId, (Instant, Option<String>)> = HashMap::new();
        let mut last_inspect = Instant::now() - INSPECT_EVERY;
        let mut last_scan = Instant::now() - SCAN_SUBS_EVERY;
        let mut baseline_sent = false;
        loop {
            let now = Instant::now();
            if now - last_inspect >= INSPECT_EVERY {
                last_inspect = now;
                match tern::inspect(&options.bin) {
                    Ok(mut panes) => {
                        for pane in panes.iter_mut().filter(|p| p.session_file.is_none()) {
                            let (checked, cli) =
                                clis.entry(pane.id).or_insert((now - PROCESS_EVERY, None));
                            if now - *checked >= PROCESS_EVERY {
                                *checked = now;
                                *cli = tern::foreground(&options.bin, pane.id)
                                    .and_then(|argv| tern::agent_cli(&argv));
                            }
                            pane.cli = cli.clone();
                        }
                        for pane in &panes {
                            if let Some(file) = &pane.session_file {
                                sources.entry(file.clone()).or_insert_with(|| Source {
                                    pane: pane.id,
                                    sub: None,
                                    tail: Tail::new(file.clone(), options.history),
                                });
                            }
                        }
                        if tx.send(Update::Panes(panes)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        if tx.send(Update::Error(format!("{error:#}"))).is_err() {
                            return;
                        }
                    }
                }
            }
            if now - last_scan >= SCAN_SUBS_EVERY {
                last_scan = now;
                let mains: Vec<(PaneId, PathBuf)> = sources
                    .iter()
                    .filter(|(_, source)| source.sub.is_none())
                    .map(|(path, source)| (source.pane, path.clone()))
                    .collect();
                for (pane, main) in mains {
                    for (name, file) in transcript::subagent_files(&main) {
                        if sources.contains_key(&file) || stale.contains(&file) {
                            continue;
                        }
                        if transcript::untouched_for(&file, STALE_SUB) {
                            stale.insert(file);
                            continue;
                        }
                        let tail = Tail::new(file.clone(), options.history / 4);
                        sources.insert(
                            file,
                            Source {
                                pane,
                                sub: Some(name),
                                tail,
                            },
                        );
                    }
                }
            }
            for source in sources.values_mut() {
                let events: Vec<Event> = source
                    .tail
                    .poll()
                    .iter()
                    .flat_map(transcript::parse)
                    .collect();
                if !events.is_empty() {
                    let update = Update::Events {
                        pane: source.pane,
                        sub: source.sub.clone(),
                        events,
                    };
                    if tx.send(update).is_err() {
                        return;
                    }
                }
            }
            if !baseline_sent {
                baseline_sent = true;
                if tx.send(Update::Baseline).is_err() {
                    return;
                }
            }
            thread::sleep(TAIL_EVERY);
        }
    });
}
