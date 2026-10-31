//! Collects pane changes from `tern events` and transcript changes from native filesystem events.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;

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

const COALESCE: Duration = Duration::from_millis(200);
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);
const EVENT_FILTER: &str = "pane_spawned,pane_resized,pane_closed,pane_exited,layout_changed,title_changed,cwd_changed,bell";
/// Subagent transcripts untouched this long are finished history; they are not read.
pub const STALE_SUB: Duration = Duration::from_secs(24 * 3600);

struct Source {
    pane: PaneId,
    sub: Option<String>,
    tail: Tail,
}

enum Input {
    Tern,
    Files(notify::Result<notify::Event>),
    StreamEnded,
    Stop,
}

#[cfg(target_os = "macos")]
mod open_files {
    use super::*;
    use kqueue::{EventData, EventFilter, FilterFlag, Ident, Vnode};
    use std::io::{self, Write};
    use std::os::{fd::AsRawFd, unix::net::UnixStream};

    struct Change {
        paths: HashSet<PathBuf>,
        rearm: HashSet<PathBuf>,
        ready: Sender<io::Result<()>>,
    }

    /// FSEvents discovers paths; one kqueue descriptor per selected transcript
    /// supplies write notifications even while its producer keeps the file open.
    pub(super) struct Files {
        changes: Option<Sender<Change>>,
        wake: UnixStream,
        worker: Option<JoinHandle<()>>,
    }

    impl Files {
        pub(super) fn new(tx: Sender<Input>) -> io::Result<Self> {
            let (mut reader, wake) = UnixStream::pair()?;
            let mut queue = kqueue::Watcher::new()?;
            queue.add_fd(
                reader.as_raw_fd(),
                EventFilter::EVFILT_READ,
                FilterFlag::empty(),
            )?;
            queue.watch()?;
            let (changes, rx) = mpsc::channel::<Change>();
            let worker = thread::spawn(move || {
                let mut watched = HashSet::new();
                while let Some(event) = queue.poll_forever(None) {
                    match event.ident {
                        Ident::Fd(_) => {
                            let mut byte = [0];
                            if reader.read_exact(&mut byte).is_err() {
                                break;
                            }
                            let Ok(change) = rx.recv() else { break };
                            let result = (|| -> io::Result<()> {
                                let removed: Vec<_> =
                                    watched.difference(&change.paths).cloned().collect();
                                for path in removed {
                                    let _ = queue.remove_filename(&path, EventFilter::EVFILT_VNODE);
                                    watched.remove(&path);
                                }
                                for path in &change.paths {
                                    if !watched.contains(path) || change.rearm.contains(path) {
                                        if watched.remove(path) {
                                            let _ = queue
                                                .remove_filename(path, EventFilter::EVFILT_VNODE);
                                        }
                                        queue.add_filename(
                                            path,
                                            EventFilter::EVFILT_VNODE,
                                            FilterFlag::NOTE_WRITE
                                                | FilterFlag::NOTE_EXTEND
                                                | FilterFlag::NOTE_DELETE
                                                | FilterFlag::NOTE_RENAME
                                                | FilterFlag::NOTE_REVOKE,
                                        )?;
                                        watched.insert(path.clone());
                                    }
                                }
                                queue.watch()
                            })();
                            let _ = change.ready.send(result);
                        }
                        Ident::Filename(_, path) => {
                            let kind = match event.data {
                                EventData::Vnode(Vnode::Delete | Vnode::Revoke) => {
                                    EventKind::Remove(notify::event::RemoveKind::Any)
                                }
                                EventData::Vnode(Vnode::Rename) => EventKind::Modify(
                                    notify::event::ModifyKind::Name(notify::event::RenameMode::Any),
                                ),
                                EventData::Error(error) => {
                                    let _ = tx.send(Input::Files(Err(notify::Error::io(error))));
                                    continue;
                                }
                                _ => EventKind::Modify(notify::event::ModifyKind::Data(
                                    notify::event::DataChange::Any,
                                )),
                            };
                            if tx
                                .send(Input::Files(Ok(
                                    notify::Event::new(kind).add_path(path.into())
                                )))
                                .is_err()
                            {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            });
            Ok(Self {
                changes: Some(changes),
                wake,
                worker: Some(worker),
            })
        }

        pub(super) fn sync(
            &mut self,
            paths: HashSet<PathBuf>,
            rearm: HashSet<PathBuf>,
        ) -> io::Result<()> {
            let (ready, rx) = mpsc::channel();
            self.changes
                .as_ref()
                .unwrap()
                .send(Change {
                    paths,
                    rearm,
                    ready,
                })
                .map_err(|_| io::Error::other("file watcher stopped"))?;
            self.wake.write_all(&[1])?;
            rx.recv()
                .map_err(|_| io::Error::other("file watcher stopped"))?
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            self.changes.take();
            let _ = self.wake.write_all(&[1]);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

/// Keep this alive for the UI's lifetime. Dropping it stops and reaps `tern events`.
pub struct Guard {
    tx: Sender<Input>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.tx.send(Input::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct EventStream {
    child: Child,
    reader: Option<JoinHandle<()>>,
    errors: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    started: Instant,
}

impl EventStream {
    fn start(bin: &str, tx: Sender<Input>) -> std::io::Result<Self> {
        let mut child = Command::new(bin)
            .args(["events", "--filter", EVENT_FILTER])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped event stdout");
        let stderr = child.stderr.take().expect("piped event stderr");
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if serde_json::from_slice::<Value>(&line)
                            .ok()
                            .as_ref()
                            .is_some_and(meaningful_event)
                            && tx.send(Input::Tern).is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Input::StreamEnded);
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&captured);
        let errors = thread::spawn(move || {
            let mut stderr = stderr;
            let mut bytes = [0; 1024];
            while let Ok(count) = stderr.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let mut text = capture.lock().unwrap();
                text.extend_from_slice(&bytes[..count]);
                let excess = text.len().saturating_sub(4096);
                text.drain(..excess);
            }
        });
        Ok(Self {
            child,
            reader: Some(reader),
            errors: Some(errors),
            stderr: captured,
            started: Instant::now(),
        })
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(errors) = self.errors.take() {
            let _ = errors.join();
        }
    }

    fn error(&mut self) -> String {
        self.stop();
        let stderr = self.stderr.lock().unwrap();
        let detail = String::from_utf8_lossy(&stderr);
        if detail.trim().is_empty() {
            "tern events disconnected; reconnecting".into()
        } else {
            format!("tern events disconnected: {}; reconnecting", detail.trim())
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn meaningful_event(event: &Value) -> bool {
    matches!(
        event.get("event").and_then(Value::as_str),
        Some(
            "pane_spawned"
                | "pane_resized"
                | "pane_closed"
                | "pane_exited"
                | "layout_changed"
                | "title_changed"
                | "cwd_changed"
                | "bell"
        )
    )
}

fn meaningful_file_event(event: &notify::Event) -> bool {
    // Opening/reading a transcript ourselves must never cause another refresh.
    !matches!(
        event.kind,
        EventKind::Access(_)
            | EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::AccessTime
            ))
    )
}

fn absolute_path(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = ancestor.canonicalize() {
            return if ancestor == path {
                real
            } else {
                real.join(path.strip_prefix(ancestor).unwrap())
            };
        }
    }
    path.to_path_buf()
}

#[derive(Default)]
struct Pending {
    refresh: bool,
    paths: HashSet<PathBuf>,
    rescan: bool,
    directories: HashSet<PathBuf>,
    replaced: HashSet<PathBuf>,
    deadline: Option<Instant>,
}

impl Pending {
    fn schedule(&mut self) {
        self.deadline
            .get_or_insert_with(|| Instant::now() + COALESCE);
    }
}

struct Collector {
    options: Options,
    tx: Sender<Update>,
    watcher: Option<RecommendedWatcher>,
    watches: HashSet<PathBuf>,
    mains: HashMap<PathBuf, PaneId>,
    sources: HashMap<PathBuf, Source>,
    clis: HashMap<PaneId, Option<String>>,
    baseline: bool,
    inspected: bool,
    #[cfg(target_os = "macos")]
    open_files: Option<open_files::Files>,
    watcher_error: Option<String>,
}

impl Collector {
    fn error(&self, message: String) -> bool {
        self.tx.send(Update::Error(message)).is_ok()
    }

    fn sync_watches(&mut self) {
        let mut desired = HashSet::new();
        for main in self.mains.keys() {
            // Watch the parent, not just existing files/subagent directories: a new
            // session's subtree may be created after the initial history drain.
            if let Some(parent) = main
                .parent()
                .and_then(|parent| parent.ancestors().find(|p| p.is_dir()))
            {
                desired.insert(parent.to_path_buf());
            }
        }
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let missing: Vec<_> = desired.difference(&self.watches).cloned().collect();
        for path in missing {
            match watcher.watch(&path, RecursiveMode::Recursive) {
                Ok(()) => {
                    self.watches.insert(path.clone());
                }
                Err(error) => {
                    self.watcher_error = Some(format!("watch {}: {error}", path.display()));
                }
            }
        }
        let obsolete: Vec<_> = self.watches.difference(&desired).cloned().collect();
        for path in obsolete {
            let _ = watcher.unwatch(&path);
            self.watches.remove(&path);
        }
    }

    #[cfg(target_os = "macos")]
    fn sync_open_files(&mut self, replaced: &HashSet<PathBuf>) {
        let paths: HashSet<_> = self
            .sources
            .iter()
            .filter(|(path, source)| {
                path.is_file()
                    && (source.sub.is_none() || !transcript::untouched_for(path, STALE_SUB))
            })
            .map(|(path, _)| path.clone())
            .collect();
        let rearm = paths.intersection(replaced).cloned().collect();
        if let Some(watcher) = self.open_files.as_mut()
            && let Err(error) = watcher.sync(paths, rearm)
        {
            self.watcher_error = Some(format!("native file watcher: {error}"));
        }
    }

    fn add_main(&mut self, path: &Path, pane: PaneId, dirty: &mut HashSet<PathBuf>) {
        if !self.sources.contains_key(path) {
            self.sources.insert(
                path.to_path_buf(),
                Source {
                    pane,
                    sub: None,
                    tail: Tail::new(path.to_path_buf(), self.options.history),
                },
            );
            dirty.insert(path.to_path_buf());
        } else if let Some(source) = self.sources.get_mut(path) {
            source.pane = pane;
            dirty.insert(path.to_path_buf());
        }
    }

    fn discover(&mut self, main: &Path, pane: PaneId, dirty: &mut HashSet<PathBuf>) {
        for (name, path) in transcript::subagent_files(main) {
            if !self.sources.contains_key(&path) && !transcript::untouched_for(&path, STALE_SUB) {
                self.sources.insert(
                    path.clone(),
                    Source {
                        pane,
                        sub: Some(name),
                        tail: Tail::new(path.clone(), self.options.history / 4),
                    },
                );
                dirty.insert(path);
            }
        }
    }

    fn refresh(&mut self, check_clis: bool, dirty: &mut HashSet<PathBuf>) -> bool {
        let mut panes = match tern::inspect(&self.options.bin) {
            Ok(panes) => panes,
            Err(error) => return self.error(format!("{error:#}")),
        };
        self.inspected = true;
        for pane in &mut panes {
            if let Some(path) = pane.session_file.as_mut() {
                *path = absolute_path(path);
            }
            if pane.session_file.is_none() {
                if check_clis || !self.clis.contains_key(&pane.id) {
                    let cli = tern::foreground(&self.options.bin, pane.id)
                        .and_then(|argv| tern::agent_cli(&argv));
                    self.clis.insert(pane.id, cli);
                }
                pane.cli = self.clis.get(&pane.id).cloned().flatten();
            }
        }
        self.clis.retain(|id, _| {
            panes
                .iter()
                .any(|pane| pane.id == *id && pane.session_file.is_none())
        });
        let mains: HashMap<_, _> = panes
            .iter()
            .filter_map(|pane| pane.session_file.clone().map(|file| (file, pane.id)))
            .collect();
        let new: Vec<_> = mains
            .iter()
            .filter(|(path, pane)| self.mains.get(*path) != Some(*pane))
            .map(|(path, pane)| (path.clone(), *pane))
            .collect();
        self.mains.extend(mains);
        self.sync_watches();
        for (path, pane) in new {
            self.add_main(&path, pane, dirty);
            self.discover(&path, pane, dirty);
            let subdir = path.with_extension("");
            for (file, source) in &mut self.sources {
                if file.starts_with(&subdir) {
                    source.pane = pane;
                }
            }
            dirty.extend(
                self.sources
                    .keys()
                    .filter(|file| file.starts_with(&subdir))
                    .cloned(),
            );
        }
        #[cfg(target_os = "macos")]
        self.sync_open_files(&HashSet::new());
        if self.tx.send(Update::Panes(panes)).is_err() {
            return false;
        }
        self.watcher_error
            .as_ref()
            .is_none_or(|error| self.error(error.clone()))
    }

    fn drain(&mut self, paths: HashSet<PathBuf>) -> bool {
        for path in paths {
            let Some(source) = self.sources.get_mut(&path) else {
                continue;
            };
            let events: Vec<Event> = source
                .tail
                .poll()
                .iter()
                .flat_map(transcript::parse)
                .collect();
            if !events.is_empty()
                && self
                    .tx
                    .send(Update::Events {
                        pane: source.pane,
                        sub: source.sub.clone(),
                        events,
                    })
                    .is_err()
            {
                return false;
            }
        }
        true
    }

    fn flush(&mut self, pending: Pending) -> bool {
        let mut dirty = HashSet::new();
        let mut activity = false;
        let mains: Vec<_> = self
            .mains
            .iter()
            .map(|(path, pane)| (path.clone(), *pane))
            .collect();
        for (main, pane) in mains {
            let subdir = main.with_extension("");
            let directory_changed = pending
                .directories
                .iter()
                .any(|dir| dir.starts_with(&subdir) || main.starts_with(dir));
            let relevant = pending.rescan
                || directory_changed
                || pending.paths.iter().any(|path| {
                    path == &main
                        || main.starts_with(path)
                        || path.starts_with(&subdir)
                            && (self.sources.contains_key(path)
                                || path.extension().is_some_and(|ext| ext == "jsonl")
                                || path.is_dir())
                });
            if !relevant {
                continue;
            }
            activity = true;
            let discover = pending.rescan
                || directory_changed
                || pending.paths.iter().any(|path| {
                    main.starts_with(path)
                        || path.starts_with(&subdir) && !self.sources.contains_key(path)
                });
            if discover {
                self.discover(&main, pane, &mut dirty);
            }
            for path in self.sources.keys() {
                if (path == &main || path.starts_with(&subdir))
                    && (pending.rescan
                        || pending.directories.iter().any(|dir| path.starts_with(dir))
                        || pending
                            .paths
                            .iter()
                            .any(|changed| path == changed || path.starts_with(changed)))
                {
                    dirty.insert(path.clone());
                }
            }
        }
        if (pending.refresh || activity || pending.rescan)
            && !self.refresh(pending.refresh, &mut dirty)
        {
            return false;
        }
        self.sync_watches();
        #[cfg(target_os = "macos")]
        self.sync_open_files(&pending.replaced);
        self.drain(dirty)
    }
}

pub fn spawn(options: Options, tx: Sender<Update>) -> Guard {
    let (input_tx, input_rx) = mpsc::channel();
    let worker_tx = input_tx.clone();
    let worker = thread::spawn(move || run(options, tx, worker_tx, input_rx));
    Guard {
        tx: input_tx,
        worker: Some(worker),
    }
}

fn run(options: Options, tx: Sender<Update>, input_tx: Sender<Input>, rx: Receiver<Input>) {
    let watcher_tx = input_tx.clone();
    let (watcher, watcher_error) =
        match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.as_ref().map_or(true, meaningful_file_event) {
                let _ = watcher_tx.send(Input::Files(event));
            }
        }) {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => (None, Some(format!("native filesystem watcher: {error}"))),
        };
    #[cfg(target_os = "macos")]
    let (open_files, watcher_error) = match open_files::Files::new(input_tx.clone()) {
        Ok(watcher) => (Some(watcher), watcher_error),
        Err(error) => (None, Some(format!("native file watcher: {error}"))),
    };
    let mut collector = Collector {
        options,
        tx,
        watcher,
        watches: HashSet::new(),
        mains: HashMap::new(),
        sources: HashMap::new(),
        clis: HashMap::new(),
        baseline: false,
        inspected: false,
        watcher_error,
        #[cfg(target_os = "macos")]
        open_files,
    };
    let mut stream = None;
    let mut retry = Some(Instant::now());
    let mut backoff = RETRY_MIN;
    let mut pending = Pending::default();
    loop {
        if retry.is_some_and(|deadline| Instant::now() >= deadline) {
            retry = None;
            match EventStream::start(&collector.options.bin, input_tx.clone()) {
                Ok(connected) => {
                    stream = Some(connected);
                    // Start the stream before inspection; buffered events reconcile
                    // later changes. The CLI has no subscription-ready acknowledgement.
                    let mut dirty = HashSet::new();
                    if !collector.refresh(true, &mut dirty) || !collector.drain(dirty) {
                        return;
                    }
                }
                Err(error) => {
                    if !collector.error(format!("cannot start tern events: {error}; reconnecting"))
                    {
                        return;
                    }
                    retry = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
            // An unavailable stream must not prevent the initial snapshot/history.
            if !collector.baseline && stream.is_none() {
                let mut dirty = HashSet::new();
                if !collector.refresh(true, &mut dirty) || !collector.drain(dirty) {
                    return;
                }
            }
        }
        if pending
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            && !collector.flush(std::mem::take(&mut pending))
        {
            return;
        }
        if !collector.baseline && collector.inspected {
            collector.baseline = true;
            if collector.tx.send(Update::Baseline).is_err() {
                return;
            }
        }
        let deadline = retry.into_iter().chain(pending.deadline).min();
        let input = match deadline {
            Some(deadline) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(input) => input,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv() {
                Ok(input) => input,
                Err(_) => return,
            },
        };
        match input {
            Input::Stop => return,
            Input::Tern => {
                pending.refresh = true;
                pending.schedule();
            }
            Input::Files(Ok(event)) => {
                pending.rescan |= event.need_rescan();
                let structural = event.kind.is_create()
                    || event.kind.is_remove()
                    || matches!(
                        event.kind,
                        EventKind::Modify(notify::event::ModifyKind::Name(_))
                    );
                for path in event.paths.iter().map(|path| absolute_path(path)) {
                    let relevant = collector.mains.keys().any(|main| {
                        path == *main
                            || main.starts_with(&path)
                            || path.starts_with(main.with_extension(""))
                                && (collector.sources.contains_key(&path)
                                    || path.extension().is_some_and(|ext| ext == "jsonl")
                                    || path.is_dir())
                    });
                    if !relevant {
                        continue;
                    }
                    if structural {
                        pending.replaced.insert(path.clone());
                    }
                    if path.is_dir() {
                        pending.directories.insert(path.clone());
                    } else if structural && let Some(parent) = path.parent() {
                        pending.directories.insert(parent.to_path_buf());
                    }
                    pending.paths.insert(path);
                }
                if pending.rescan || !pending.paths.is_empty() {
                    pending.schedule();
                }
            }
            Input::Files(Err(error)) => {
                let message = format!("filesystem events: {error}");
                collector.watcher_error = Some(message.clone());
                if !collector.error(message) {
                    return;
                }
            }
            Input::StreamEnded => {
                if let Some(mut ended) = stream.take() {
                    if ended.started.elapsed() >= RETRY_MAX {
                        backoff = RETRY_MIN;
                    }
                    if !collector.error(ended.error()) {
                        return;
                    }
                }
                retry = Some(Instant::now() + backoff);
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignores_client_feedback_and_file_reads() {
        for event in ["client_connected", "client_left", "unknown"] {
            assert!(!meaningful_event(&json!({"event": event})));
        }
        assert!(meaningful_event(
            &json!({"event": "pane_exited", "pane": 7, "status": 0})
        ));
        assert!(!meaningful_file_event(&notify::Event::new(
            EventKind::Access(notify::event::AccessKind::Read)
        )));
        assert!(meaningful_file_event(&notify::Event::new(
            EventKind::Modify(notify::event::ModifyKind::Any)
        )));
    }

    #[cfg(unix)]
    mod native {
        use super::*;
        use std::fs::{self, OpenOptions};
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        struct Fixture {
            dir: PathBuf,
        }

        impl Fixture {
            fn new(reconnect: bool) -> Self {
                let dir = std::env::temp_dir().join(format!(
                    "ternwatch-events-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir_all(&dir).unwrap();
                let dir = dir.canonicalize().unwrap();
                let main = dir.join("main.jsonl");
                fs::write(&main, "{\"type\":\"model_change\",\"model\":\"initial\"}\n").unwrap();
                let state = json!({
                    "sessions": [{"name":"test", "tabs":[{"number":1,"blocks":[
                        {"id":7,"program":"omp","title":"test"}
                    ]}]}],
                    "panes": [{"pane":7,"omp_session_file":main}]
                });
                fs::write(dir.join("state"), state.to_string()).unwrap();
                let failure = if reconnect {
                    "if [ ! -e \"$base/failed\" ]; then touch \"$base/failed\"; exit 1; fi"
                } else {
                    ":"
                };
                fs::write(dir.join("tern"), format!(
                    "#!/bin/sh\nbase='{}'\ncase \"$1\" in\ninspect) printf 'inspect\\n' >> \"$base/queries\"; cat \"$base/state\" ;;\nevents) printf 'events\\n' >> \"$base/connections\"; {failure}; exec sleep 60 ;;\nprocess) printf '{{}}' ;;\nesac\n", dir.display()
                )).unwrap();
                fs::set_permissions(dir.join("tern"), fs::Permissions::from_mode(0o700)).unwrap();
                Self { dir }
            }

            fn start(&self) -> (Guard, Receiver<Update>) {
                let (tx, rx) = mpsc::channel();
                let guard = spawn(
                    Options {
                        bin: self.dir.join("tern").to_string_lossy().into_owned(),
                        history: 1 << 20,
                    },
                    tx,
                );
                (guard, rx)
            }

            fn queries(&self) -> usize {
                fs::read_to_string(self.dir.join("queries"))
                    .unwrap()
                    .lines()
                    .count()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.dir);
            }
        }

        #[track_caller]
        fn until(rx: &Receiver<Update>, mut matches: impl FnMut(&Update) -> bool) -> Vec<Update> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut updates = Vec::new();
            loop {
                let update = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("collector update");
                let done = matches(&update);
                updates.push(update);
                if done {
                    return updates;
                }
            }
        }

        fn model(update: &Update, sub: Option<&str>, name: &str) -> bool {
            matches!(update, Update::Events { sub: actual, events, .. }
                if actual.as_deref() == sub && events.contains(&Event::Model(name.to_owned())))
        }

        #[test]
        fn idle_has_no_repeated_queries_and_reads_are_ignored() {
            let fixture = Fixture::new(false);
            let (_guard, rx) = fixture.start();
            let initial = until(&rx, |update| matches!(update, Update::Baseline));
            assert_eq!(
                initial
                    .iter()
                    .filter(|update| model(update, None, "initial"))
                    .count(),
                1
            );
            assert_eq!(fixture.queries(), 1);
            let _ = fs::read(fixture.dir.join("main.jsonl")).unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(1200)).is_err());
            assert_eq!(fixture.queries(), 1);
        }

        #[test]
        fn observes_partial_appends_and_new_nested_transcripts_without_history_replay() {
            let fixture = Fixture::new(false);
            let (_guard, rx) = fixture.start();
            until(&rx, |update| matches!(update, Update::Baseline));
            let mut main = OpenOptions::new()
                .append(true)
                .open(fixture.dir.join("main.jsonl"))
                .unwrap();
            write!(main, "{{\"type\":\"model_change\",\"model\":\"appended\"").unwrap();
            main.flush().unwrap();
            until(&rx, |update| matches!(update, Update::Panes(_)));
            writeln!(main, "}}").unwrap();
            main.flush().unwrap();
            let appended = until(&rx, |update| model(update, None, "appended"));
            assert!(!appended.iter().any(|update| model(update, None, "initial")));
            fs::write(
                fixture.dir.join("main.jsonl"),
                "{\"type\":\"model_change\",\"model\":\"short\"}\n",
            )
            .unwrap();
            until(&rx, |update| model(update, None, "short"));
            fs::write(
                fixture.dir.join("replacement"),
                "{\"type\":\"model_change\",\"model\":\"replacement\"}\n",
            )
            .unwrap();
            fs::rename(
                fixture.dir.join("replacement"),
                fixture.dir.join("main.jsonl"),
            )
            .unwrap();
            until(&rx, |update| model(update, None, "replacement"));
            let mut replacement = OpenOptions::new()
                .append(true)
                .open(fixture.dir.join("main.jsonl"))
                .unwrap();
            writeln!(
                replacement,
                "{{\"type\":\"model_change\",\"model\":\"after-replacement\"}}"
            )
            .unwrap();
            until(&rx, |update| model(update, None, "after-replacement"));
            fs::create_dir_all(fixture.dir.join("main/Parent")).unwrap();
            fs::write(
                fixture.dir.join("main/Parent/Child.jsonl"),
                "{\"type\":\"model_change\",\"model\":\"nested\"}\n",
            )
            .unwrap();
            until(&rx, |update| model(update, Some("Parent.Child"), "nested"));
            let mut nested = OpenOptions::new()
                .append(true)
                .open(fixture.dir.join("main/Parent/Child.jsonl"))
                .unwrap();
            writeln!(
                nested,
                "{{\"type\":\"model_change\",\"model\":\"nested-append\"}}"
            )
            .unwrap();
            until(&rx, |update| {
                model(update, Some("Parent.Child"), "nested-append")
            });
        }

        #[test]
        fn excludes_unrelated_history_from_open_file_watches() {
            let fixture = Fixture::new(false);
            fs::create_dir_all(fixture.dir.join("main")).unwrap();
            for n in 0..400 {
                fs::write(fixture.dir.join(format!("old-{n}.jsonl")), "{}\n").unwrap();
                fs::write(
                    fixture.dir.join(format!("main/__advisor.{n}.jsonl")),
                    "{}\n",
                )
                .unwrap();
            }
            let (_guard, rx) = fixture.start();
            until(&rx, |update| matches!(update, Update::Baseline));
            let mut main = OpenOptions::new()
                .append(true)
                .open(fixture.dir.join("main.jsonl"))
                .unwrap();
            writeln!(main, "{{\"type\":\"model_change\",\"model\":\"live\"}}").unwrap();
            until(&rx, |update| model(update, None, "live"));
        }

        #[test]
        fn keeps_final_closed_session_events_and_reassigns_reopened_children() {
            let fixture = Fixture::new(false);
            let child = fixture.dir.join("main/Child.jsonl");
            fs::create_dir_all(child.parent().unwrap()).unwrap();
            fs::write(&child, "{\"type\":\"model_change\",\"model\":\"child\"}\n").unwrap();
            let (_guard, rx) = fixture.start();
            until(&rx, |update| matches!(update, Update::Baseline));
            fs::write(fixture.dir.join("state"), "{\"sessions\":[],\"panes\":[]}").unwrap();
            let mut main = OpenOptions::new()
                .append(true)
                .open(fixture.dir.join("main.jsonl"))
                .unwrap();
            writeln!(main, "{{\"type\":\"model_change\",\"model\":\"closing\"}}").unwrap();
            until(
                &rx,
                |update| matches!(update, Update::Panes(panes) if panes.is_empty()),
            );
            let mut child_file = OpenOptions::new().append(true).open(&child).unwrap();
            writeln!(
                child_file,
                "{{\"type\":\"custom\",\"customType\":\"session_exit\"}}"
            )
            .unwrap();
            until(&rx, |update| {
                matches!(update, Update::Events { pane: 7, sub: Some(sub), events }
                if sub == "Child" && events.iter().any(|event| matches!(event, Event::Exit { .. })))
            });
            let state = json!({
                "sessions": [{"name":"test","tabs":[{"number":1,"blocks":[{"id":8,"program":"omp"}]}]}],
                "panes": [{"pane":8,"omp_session_file":fixture.dir.join("main.jsonl")}]
            });
            fs::write(fixture.dir.join("state"), state.to_string()).unwrap();
            writeln!(main, "{{\"type\":\"model_change\",\"model\":\"reopened\"}}").unwrap();
            until(
                &rx,
                |update| matches!(update, Update::Panes(panes) if panes.iter().any(|pane| pane.id == 8)),
            );
            writeln!(
                child_file,
                "{{\"type\":\"model_change\",\"model\":\"new-owner\"}}"
            )
            .unwrap();
            until(&rx, |update| {
                matches!(update, Update::Events { pane: 8, .. })
                    && model(update, Some("Child"), "new-owner")
            });
        }

        #[test]
        fn reconnects_without_replaying_baseline_history() {
            let fixture = Fixture::new(true);
            let (guard, rx) = fixture.start();
            let initial = until(&rx, |update| matches!(update, Update::Baseline));
            assert_eq!(
                initial
                    .iter()
                    .filter(|update| model(update, None, "initial"))
                    .count(),
                1
            );
            let recovered = until(&rx, |update| matches!(update, Update::Panes(_)));
            assert!(
                !recovered
                    .iter()
                    .any(|update| model(update, None, "initial"))
            );
            assert_eq!(
                fs::read_to_string(fixture.dir.join("connections"))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
            // Stream bookkeeping and inspect logs share this watched parent, but
            // creating unrelated siblings must not trigger another inspection.
            fs::write(fixture.dir.join("unrelated"), "created after reconnect").unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(1200)).is_err());
            assert_eq!(fixture.queries(), 2);
            let stopped = Instant::now();
            drop(guard);
            assert!(stopped.elapsed() < Duration::from_secs(5));
        }
    }
}
