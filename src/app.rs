//! Interactive state: selection, live/replay playback, packets in flight, and key handling.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;

use crate::model::{Change, Kind, Message, Model, Node, PaneId};
use crate::tern;

pub const PACKET_MS: u64 = 1_100;
pub const FLASH_MS: u64 = 1_400;
/// A sent message with no delivery evidence yet is shown as waiting this long.
pub const WAIT_MS: i64 = 3 * 60_000;
/// Closed panes and shells stay on the graph this long after their last message.
const RECENT_PEER_MS: i64 = 60 * 60_000;
/// Rows one mouse wheel step scrolls; PgUp/PgDn scroll a few steps.
const SCROLL_ROWS: isize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Graph,
    Feed,
}

pub enum Mode {
    Live,
    Replay(Replay),
}

pub struct Replay {
    /// Replay clock, in transcript time (epoch ms).
    pub cursor: i64,
    pub speed: f64,
    pub paused: bool,
    /// Messages in time order, and how many have played.
    order: Vec<u64>,
    played: usize,
}

pub struct Packet {
    pub seq: u64,
    pub from: Node,
    pub to: Node,
    pub glyph: &'static str,
    pub color: Color,
    pub start: Instant,
}

pub enum Popup {
    Help,
    Message(u64),
}

pub struct App {
    pub model: Model,
    pub bin: String,
    pub self_pane: Option<PaneId>,
    pub selected: Option<PaneId>,
    pub focus: Focus,
    /// Selected feed row by message seq; `None` follows the newest message.
    pub feed_selected: Option<u64>,
    /// Feed scrolled by the wheel or PgUp/PgDn: its top row's seq. `None` keeps the newest (or
    /// the selected) row in view.
    pub feed_top: Option<u64>,
    /// The feed's top row index and its visible rows in the last frame.
    pub feed_view: (usize, usize),
    /// Rows the inspector is scrolled down; drawing clamps it to the content.
    pub inspector_scroll: u16,
    /// Rows the open popup is scrolled down; drawing clamps it to the content.
    pub popup_scroll: u16,
    /// `f`: only cross-pane messages in the feed. The selected agent, if any, narrows it further.
    pub cross_pane_only: bool,
    pub all_panes: bool,
    pub mode: Mode,
    pub packets: Vec<Packet>,
    pub flashes: HashMap<Node, (Instant, Color)>,
    pub animate: bool,
    pub color: bool,
    pub popup: Option<Popup>,
    pub compose: Option<String>,
    pub status: Option<(String, Instant)>,
    pub error: Option<String>,
    pub loaded: bool,
    pub quit: bool,
    /// Hit areas from the last frame, for the mouse.
    pub hit_nodes: Vec<(Rect, PaneId)>,
    pub hit_feed: Vec<(Rect, u64)>,
    /// The selected pane's id in the inspector; a click copies it.
    pub hit_copy: Option<(Rect, PaneId)>,
    /// The graph; a click on empty space in it deselects.
    pub hit_graph: Rect,
    /// The inspector and the feed; the wheel scrolls the one under the pointer.
    pub hit_inspector: Rect,
    pub hit_feed_area: Rect,
    last_tick: Instant,
}

impl App {
    pub fn new(bin: String, animate: bool, color: bool, all_panes: bool) -> Self {
        Self {
            model: Model::default(),
            bin,
            self_pane: std::env::var("TERN_PANE").ok().and_then(|p| p.parse().ok()),
            selected: None,
            focus: Focus::Graph,
            feed_selected: None,
            feed_top: None,
            feed_view: (0, 0),
            inspector_scroll: 0,
            popup_scroll: 0,
            cross_pane_only: false,
            all_panes,
            mode: Mode::Live,
            packets: Vec::new(),
            flashes: HashMap::new(),
            animate,
            color,
            popup: None,
            compose: None,
            status: None,
            error: None,
            loaded: false,
            quit: false,
            hit_nodes: Vec::new(),
            hit_feed: Vec::new(),
            hit_copy: None,
            hit_graph: Rect::default(),
            hit_inspector: Rect::default(),
            hit_feed_area: Rect::default(),
            last_tick: Instant::now(),
        }
    }

    pub fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// Transcript time the view shows: now when live, the replay clock otherwise.
    pub fn view_time(&self) -> i64 {
        match &self.mode {
            Mode::Live => Self::now_ms(),
            Mode::Replay(replay) => replay.cursor,
        }
    }

    pub fn visible_message(&self, message: &Message) -> bool {
        match &self.mode {
            Mode::Live => true,
            Mode::Replay(replay) => message.time() <= replay.cursor,
        }
    }

    /// Agents on the graph, in ring order: open agent panes by Tern session and tab, then panes
    /// that only appear in messages (closed panes, shells) while their traffic is recent.
    pub fn visible_agents(&self) -> Vec<PaneId> {
        let mut talked: HashMap<PaneId, i64> = HashMap::new();
        // A failed send (for example to a pane id that does not exist) is no contact.
        for message in self
            .model
            .messages
            .iter()
            .filter(|m| m.kind == Kind::Pane && !m.failed && self.visible_message(m))
        {
            for pane in [message.from.pane(), message.to.pane()] {
                let last = talked.entry(pane).or_default();
                *last = (*last).max(message.time());
            }
        }
        let now = self.view_time();
        let mut agents: Vec<_> = self
            .model
            .agents
            .values()
            .filter(|a| Some(a.pane.id) != self.self_pane)
            .filter(|a| {
                if a.present && a.is_agent() {
                    return true;
                }
                let recent = talked
                    .get(&a.pane.id)
                    .is_some_and(|last| now - last < RECENT_PEER_MS);
                (a.present || talked.contains_key(&a.pane.id)) && (self.all_panes || recent)
            })
            .collect();
        agents.sort_by(|a, b| {
            (!a.present, &a.pane.session, a.pane.tab, a.pane.id).cmp(&(
                !b.present,
                &b.pane.session,
                b.pane.tab,
                b.pane.id,
            ))
        });
        agents.into_iter().map(|a| a.pane.id).collect()
    }

    /// Messages for the feed, oldest first.
    pub fn feed(&self) -> Vec<&Message> {
        let mut messages: Vec<&Message> = self
            .model
            .messages
            .iter()
            .filter(|m| self.visible_message(m))
            .filter(|m| !self.cross_pane_only || m.kind == Kind::Pane)
            // An agent's messages in and out, its subagents' included; all when none is selected.
            .filter(|m| {
                self.selected
                    .is_none_or(|s| m.from.pane() == s || m.to.pane() == s)
            })
            .collect();
        messages.sort_by_key(|m| (m.time(), m.seq));
        messages
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
    }

    /// Turns model changes into packets and flashes. Live mode only; replay makes its own.
    pub fn absorb_changes(&mut self) {
        let changes = std::mem::take(&mut self.model.changes);
        if !matches!(self.mode, Mode::Live) {
            return;
        }
        for change in changes {
            match change {
                Change::Sent(seq) => self.launch(seq),
                Change::Delivered(seq) => {
                    if !self.packets.iter().any(|p| p.seq == seq)
                        && let Some(message) = self.model.message(seq)
                    {
                        let to = message.to.clone();
                        self.flash(to, Color::LightGreen);
                    }
                }
                Change::Failed(seq) => {
                    if let Some(message) = self.model.message(seq) {
                        let from = message.from.clone();
                        self.flash(from, Color::LightRed);
                    }
                }
            }
        }
    }

    fn launch(&mut self, seq: u64) {
        let Some(message) = self.model.message(seq) else {
            return;
        };
        if self.packets.iter().any(|p| p.seq == seq) {
            return;
        }
        let (glyph, color) = packet_style(message);
        let packet = Packet {
            seq,
            from: message.from.clone(),
            to: message.to.clone(),
            glyph,
            color,
            start: Instant::now(),
        };
        if self.animate {
            self.packets.push(packet);
        } else {
            self.flash(packet.to, color);
        }
    }

    fn flash(&mut self, node: Node, color: Color) {
        self.flashes.insert(node, (Instant::now(), color));
    }

    /// Advances replay and retires finished packets and flashes. Call once per frame.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now - self.last_tick;
        self.last_tick = now;
        let mut landed = Vec::new();
        self.packets.retain(|packet| {
            let done = now - packet.start >= Duration::from_millis(PACKET_MS);
            if done {
                landed.push(packet.seq);
            }
            !done
        });
        for seq in landed {
            if let Some(message) = self.model.message(seq)
                && (message.delivered.is_some() || message.kind != Kind::Pane)
            {
                let (to, color) = (message.to.clone(), packet_style(message).1);
                self.flash(
                    to,
                    if message.kind == Kind::Pane {
                        Color::LightGreen
                    } else {
                        color
                    },
                );
            }
        }
        self.flashes
            .retain(|_, (start, _)| now - *start < Duration::from_millis(FLASH_MS));
        if let Some((_, at)) = &self.status
            && now - *at > Duration::from_secs(5)
        {
            self.status = None;
        }
        self.advance_replay(elapsed);
    }

    fn advance_replay(&mut self, elapsed: Duration) {
        let Mode::Replay(replay) = &mut self.mode else {
            return;
        };
        if replay.paused {
            return;
        }
        let Some(next) = replay
            .order
            .get(replay.played)
            .and_then(|seq| self.model.message(*seq))
            .map(Message::time)
        else {
            if self.packets.is_empty() {
                replay.paused = true;
            }
            return;
        };
        replay.cursor += (elapsed.as_millis() as f64 * replay.speed) as i64;
        // Skip quiet stretches: never wait more than 1.5 s of real time for the next message.
        let max_wait = (1_500.0 * replay.speed) as i64;
        if next - replay.cursor > max_wait {
            replay.cursor = next - max_wait;
        }
        let mut due = Vec::new();
        while let Some(seq) = replay.order.get(replay.played).copied() {
            match self.model.message(seq) {
                Some(message) if message.time() <= replay.cursor => {
                    due.push(seq);
                    replay.played += 1;
                }
                Some(_) => break,
                None => replay.played += 1,
            }
        }
        for seq in due {
            self.launch(seq);
        }
    }

    pub fn start_replay(&mut self) {
        let mut order: Vec<&Message> = self.model.messages.iter().collect();
        order.sort_by_key(|m| (m.time(), m.seq));
        let cursor = order
            .first()
            .map(|m| m.time() - 1_000)
            .unwrap_or_else(Self::now_ms);
        self.packets.clear();
        self.flashes.clear();
        self.mode = Mode::Replay(Replay {
            cursor,
            speed: 8.0,
            paused: false,
            order: order.iter().map(|m| m.seq).collect(),
            played: 0,
        });
        self.feed_selected = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(text) = &mut self.compose {
            match key.code {
                KeyCode::Esc => self.compose = None,
                KeyCode::Enter => {
                    let text = std::mem::take(text);
                    self.compose = None;
                    self.send(text);
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            return;
        }
        if self.popup.is_some() {
            let page = 3 * SCROLL_ROWS;
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => self.popup = None,
                KeyCode::Up | KeyCode::Char('k') => self.scroll_popup(-1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_popup(1),
                KeyCode::PageUp => self.scroll_popup(-page),
                KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_popup(page),
                KeyCode::Home => self.popup_scroll = 0,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.select(None),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.open(Popup::Help),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Graph {
                    Focus::Feed
                } else {
                    Focus::Graph
                };
            }
            KeyCode::Left | KeyCode::Char('h') => self.step_agent(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step_agent(1),
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::Graph => self.step_agent(-1),
                Focus::Feed => self.step_feed(-1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::Graph => self.step_agent(1),
                Focus::Feed => self.step_feed(1),
            },
            KeyCode::Home => match self.focus {
                Focus::Graph => self.select(self.visible_agents().first().copied()),
                Focus::Feed => self.feed_selected = self.feed().first().map(|m| m.seq),
            },
            KeyCode::End => match self.focus {
                Focus::Graph => self.select(self.visible_agents().last().copied()),
                Focus::Feed => self.feed_selected = None,
            },
            KeyCode::Enter => match self.focus {
                Focus::Graph => self.jump(),
                Focus::Feed => {
                    let seq = self
                        .feed_selected
                        .or_else(|| self.feed().last().map(|m| m.seq));
                    if let Some(seq) = seq {
                        self.open(Popup::Message(seq));
                    }
                }
            },
            KeyCode::Char('m') => match self.selected.and_then(|s| self.model.agents.get(&s)) {
                Some(agent) if agent.present => self.compose = Some(String::new()),
                _ => self.set_status("select an open pane to message it"),
            },
            KeyCode::Char('r') => match self.mode {
                Mode::Live => self.start_replay(),
                Mode::Replay(_) => {
                    self.mode = Mode::Live;
                    self.packets.clear();
                }
            },
            KeyCode::Char(' ') => {
                if let Mode::Replay(replay) = &mut self.mode {
                    replay.paused = !replay.paused;
                }
            }
            KeyCode::Char('+' | '=') => self.scale_speed(2.0),
            KeyCode::Char('-') => self.scale_speed(0.5),
            KeyCode::Char('f') => {
                self.cross_pane_only = !self.cross_pane_only;
                self.feed_selected = None;
                self.feed_top = None;
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let delta = 3 * if key.code == KeyCode::PageUp {
                    -SCROLL_ROWS
                } else {
                    SCROLL_ROWS
                };
                match self.focus {
                    Focus::Graph => self.scroll_inspector(delta),
                    Focus::Feed => self.scroll_feed(delta),
                }
            }
            KeyCode::Char('a') => self.all_panes = !self.all_panes,
            KeyCode::Char('c') => self.copy_id(self.selected),
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.popup.is_some() {
            // The popup covers the panels: the wheel scrolls it, clicks do nothing.
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_popup(-SCROLL_ROWS),
                MouseEventKind::ScrollDown => self.scroll_popup(SCROLL_ROWS),
                _ => {}
            }
            return;
        }
        let at = Position::new(mouse.column, mouse.row);
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => -SCROLL_ROWS,
            MouseEventKind::ScrollDown => SCROLL_ROWS,
            MouseEventKind::Down(MouseButton::Left) => 0,
            _ => return,
        };
        if delta != 0 {
            if self.hit_inspector.contains(at) {
                self.scroll_inspector(delta);
            } else if self.hit_feed_area.contains(at) {
                self.scroll_feed(delta);
            }
            return;
        }
        // A left click.
        if let Some((_, pane)) = self.hit_copy.filter(|(rect, _)| rect.contains(at)) {
            self.copy_id(Some(pane));
        } else if let Some((_, pane)) = self.hit_nodes.iter().find(|(rect, _)| rect.contains(at)) {
            // A click selects the card and copies its id, ready for `tern run`.
            let pane = *pane;
            self.select(Some(pane));
            self.focus = Focus::Graph;
            self.copy_id(Some(pane));
        } else if let Some((_, seq)) = self.hit_feed.iter().find(|(rect, _)| rect.contains(at)) {
            self.feed_selected = Some(*seq);
            self.focus = Focus::Feed;
        } else if self.hit_graph.contains(at) {
            // Empty graph space: back to every agent's messages.
            self.select(None);
        }
    }

    /// Selects an agent (or none), which also picks whose messages the feed lists.
    pub fn select(&mut self, pane: Option<PaneId>) {
        if self.selected != pane {
            self.selected = pane;
            self.feed_selected = None;
            self.feed_top = None;
            self.inspector_scroll = 0;
        }
    }

    fn scroll_inspector(&mut self, delta: isize) {
        self.inspector_scroll = (self.inspector_scroll as isize + delta).max(0) as u16;
    }

    /// Opens a popup at its top.
    fn open(&mut self, popup: Popup) {
        self.popup = Some(popup);
        self.popup_scroll = 0;
    }

    fn scroll_popup(&mut self, delta: isize) {
        self.popup_scroll = self
            .popup_scroll
            .saturating_add_signed(delta.clamp(i16::MIN as isize, i16::MAX as isize) as i16);
    }

    fn scroll_feed(&mut self, delta: isize) {
        let feed: Vec<u64> = self.feed().iter().map(|m| m.seq).collect();
        let (start, rows) = self.feed_view;
        let max = feed.len().saturating_sub(rows);
        let next = (start as isize + delta).clamp(0, max as isize) as usize;
        // Scrolling to the bottom goes back to following new messages.
        (self.feed_top, self.feed_selected) = if next >= max {
            (None, None)
        } else {
            (Some(feed[next]), self.feed_selected)
        };
    }

    fn scale_speed(&mut self, factor: f64) {
        if let Mode::Replay(replay) = &mut self.mode {
            replay.speed = (replay.speed * factor).clamp(0.25, 256.0);
        }
    }

    fn step_agent(&mut self, delta: isize) {
        let agents = self.visible_agents();
        if agents.is_empty() {
            return;
        }
        let index = self
            .selected
            .and_then(|s| agents.iter().position(|a| *a == s));
        let next = match index {
            Some(index) => (index as isize + delta).rem_euclid(agents.len() as isize) as usize,
            None if delta < 0 => agents.len() - 1,
            None => 0,
        };
        self.select(Some(agents[next]));
    }

    fn step_feed(&mut self, delta: isize) {
        self.feed_top = None;
        let feed: Vec<u64> = self.feed().iter().map(|m| m.seq).collect();
        if feed.is_empty() {
            return;
        }
        let index = self
            .feed_selected
            .and_then(|s| feed.iter().position(|m| *m == s))
            .unwrap_or(feed.len() - 1);
        let next = (index as isize + delta).clamp(0, feed.len() as isize - 1) as usize;
        // Moving past the newest row goes back to following new messages.
        self.feed_selected = if delta > 0 && index + 1 == feed.len() {
            None
        } else {
            Some(feed[next])
        };
    }

    fn jump(&mut self) {
        let Some(pane) = self.selected else { return };
        match tern::focus(&self.bin, pane) {
            Ok(()) => self.set_status(format!("focused pane {pane}")),
            Err(error) => self.set_status(format!("{error:#}")),
        }
    }

    fn send(&mut self, text: String) {
        let Some(pane) = self.selected else { return };
        if text.trim().is_empty() {
            return;
        }
        let name = self.model.name(&Node::Pane(pane));
        match tern::send(&self.bin, pane, &text) {
            Ok(()) => self.set_status(format!("typed into {name}'s composer")),
            Err(error) => self.set_status(format!("{error:#}")),
        }
    }

    fn copy_id(&mut self, pane: Option<PaneId>) {
        let Some(pane) = pane else {
            return self.set_status("select a pane to copy its id");
        };
        let name = self.model.name(&Node::Pane(pane));
        match clipboard(&pane.to_string()) {
            Ok(()) => self.set_status(format!("copied {name}'s pane id {pane}")),
            Err(error) => self.set_status(format!("{error:#}")),
        }
    }
}

/// Puts `text` on the system clipboard with the first clipboard command that exists.
fn clipboard(text: &str) -> Result<()> {
    let commands: [(&str, &[&str]); 3] = [
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    for (program, args) in commands {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        // Dropping stdin closes it, so the command sees the end of the text.
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(text.as_bytes())?;
        if !child.wait()?.success() {
            bail!("{program} failed");
        }
        return Ok(());
    }
    bail!("no clipboard command found (pbcopy, wl-copy or xclip)")
}

/// Glyph and color of a message in flight: what kind of traffic it is.
pub fn packet_style(message: &Message) -> (&'static str, Color) {
    match message.kind {
        Kind::Pane if message.reply_to.is_some() => ("●", Color::Magenta),
        Kind::Pane => ("◆", Color::Cyan),
        Kind::Irc => ("•", Color::Yellow),
        Kind::Spawn => ("✦", Color::LightBlue),
        Kind::Yield => ("✓", Color::Green),
    }
}
