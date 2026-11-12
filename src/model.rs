//! Agents, subagents and the messages between them, built from Tern pane state and omp transcripts.
//!
//! Evidence model: a cross-pane message is *sent* when the sender's transcript records a
//! `tern run`/`tern send` to the pane, and *delivered* when the recipient's transcript records a
//! user turn starting `From <sender pane>`. An IRC message carries omp's own receipt outcome.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::pane::Pane;
use crate::transcript::Event;

pub type PaneId = u64;

/// How far apart a send and its delivery may be recorded.
const MATCH_BEFORE_MS: i64 = 5_000;
const MATCH_AFTER_MS: i64 = 15 * 60_000;
/// Possible pasted repeats must remain within three minutes of their root delivery.
const REPEAT_WINDOW_MS: i64 = 180_000;
/// A message counts as a reply when it goes back to the sender within this time.
const REPLY_WINDOW_MS: i64 = 30 * 60_000;
/// A subagent that has written nothing for this long is shown as idle.
const SUB_IDLE_MS: i64 = 120_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Node {
    Pane(PaneId),
    /// A subagent of the pane's omp session, by its dotted IRC name.
    Sub(PaneId, String),
}

impl Node {
    pub fn pane(&self) -> PaneId {
        match self {
            Node::Pane(pane) | Node::Sub(pane, _) => *pane,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Typed into another pane with `tern run` / `tern send`.
    Pane,
    Irc,
    Spawn,
    Yield,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub seq: u64,
    pub kind: Kind,
    pub from: Node,
    pub to: Node,
    pub text: String,
    pub sent: Option<i64>,
    pub delivered: Option<i64>,
    /// omp's IRC receipt outcome (`injected`, `woken`, …).
    pub outcome: Option<String>,
    pub failed: bool,
    pub reply_to: Option<u64>,
    /// Root sequence of a possible repeated inbound-only pane message; all records are retained.
    pub repeat_of: Option<u64>,
    pub replied: bool,
}

impl Message {
    pub fn time(&self) -> i64 {
        self.sent.or(self.delivered).unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Sub {
    pub last_ms: i64,
    pub done: bool,
    pub tool: Option<String>,
    pub tokens: u64,
}

impl Sub {
    pub fn running(&self, now: i64) -> bool {
        !self.done && now - self.last_ms < SUB_IDLE_MS
    }
}

#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub pane: Pane,
    /// Still open in Tern. A closed pane stays while its messages are shown.
    pub present: bool,
    pub model: Option<String>,
    pub tokens: u64,
    pub cost: f64,
    open_calls: Vec<(String, String, String)>,
    pub last_user: Option<(i64, String)>,
    pub last_reply: Option<(i64, String)>,
    pub todo: Option<(u32, u32)>,
    pub subs: BTreeMap<String, Sub>,
    pub last_ms: i64,
}

impl Agent {
    /// An omp agent with a transcript, or another agent CLI seen in the pane's foreground.
    pub fn is_agent(&self) -> bool {
        self.has_transcript() || self.pane.cli.is_some()
    }

    /// Delivery to this pane is observable only through an omp transcript.
    pub fn has_transcript(&self) -> bool {
        self.pane.session_file.is_some()
    }

    /// The tool this agent is running now, with omp's intent line.
    pub fn tool(&self) -> Option<(&str, &str)> {
        self.open_calls
            .last()
            .map(|(_, name, intent)| (name.as_str(), intent.as_str()))
    }

    /// Title split into omp's short label and the rest: `󰗑 DEMO: Merge PR` → (`󰗑`, `DEMO`, `Merge PR`).
    pub fn label(&self) -> (String, String, String) {
        split_title(&self.pane.title, &self.address())
    }

    /// What the backend's commands take for this pane.
    pub fn address(&self) -> String {
        match self.pane.address.as_str() {
            "" => self.pane.id.to_string(),
            address => address.to_string(),
        }
    }

    pub fn repo(&self) -> String {
        let cwd = self.pane.cwd.trim_end_matches('/');
        cwd.rsplit('/').next().unwrap_or(cwd).to_string()
    }
}

/// `address` names an untitled pane: `pane 7`, `pane t7`.
pub fn split_title(title: &str, address: &str) -> (String, String, String) {
    let title = title.trim();
    let (icon, rest) = match title.split_once(' ') {
        Some((first, rest)) if !first.is_empty() && !first.chars().any(|c| c.is_ascii()) => {
            (first, rest)
        }
        _ => ("", title),
    };
    let (label, description) = match rest.split_once(": ") {
        Some((label, description)) if label.chars().count() <= 14 => (label, description),
        _ => (rest, ""),
    };
    let label = if label.is_empty() {
        format!("pane {address}")
    } else {
        label.to_string()
    };
    (icon.to_string(), label, description.to_string())
}

/// What changed, for animation. Only produced after the initial history load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Sent(u64),
    Delivered(u64),
    Failed(u64),
}

#[derive(Default)]
pub struct Model {
    pub agents: BTreeMap<PaneId, Agent>,
    pub messages: Vec<Message>,
    /// Off while history loads, so old traffic does not animate.
    pub live: bool,
    pub changes: Vec<Change>,
    /// Tool call id → messages it sent, to mark them failed when the call fails.
    pending_calls: HashMap<String, Vec<u64>>,
    /// Original IRC sends, scoped by transcript source and tool call id until their receipts arrive.
    irc_calls: HashMap<Node, HashMap<String, (i64, String)>>,
    next_seq: u64,
}

impl Model {
    /// Replaces the pane list. Closed panes stay only while messages involve them.
    pub fn apply_panes(&mut self, panes: Vec<Pane>) {
        for agent in self.agents.values_mut() {
            agent.present = false;
        }
        for pane in panes {
            let agent = self.agents.entry(pane.id).or_default();
            agent.present = true;
            agent.pane = pane;
        }
        let messages = &self.messages;
        self.agents.retain(|id, agent| {
            agent.present
                || messages
                    .iter()
                    .any(|m| m.from.pane() == *id || m.to.pane() == *id)
        });
    }

    /// Applies events read from the transcript of `pane`'s main session (`sub == None`) or of one
    /// of its subagents.
    pub fn apply(&mut self, pane: PaneId, sub: Option<&str>, events: Vec<Event>) {
        let source = match sub {
            Some(name) => Node::Sub(pane, name.to_string()),
            None => Node::Pane(pane),
        };
        for event in events {
            self.apply_one(&source, event);
        }
    }

    fn apply_one(&mut self, source: &Node, event: Event) {
        let pane = source.pane();
        if let Some(ts) = event_time(&event) {
            let agent = self.agent(pane);
            agent.last_ms = agent.last_ms.max(ts);
            if let Node::Sub(_, name) = source {
                let sub = agent.subs.entry(name.clone()).or_default();
                sub.last_ms = sub.last_ms.max(ts);
            }
        }
        match event {
            Event::Model(model) => {
                if matches!(source, Node::Pane(_)) {
                    self.agent(pane).model = Some(model);
                }
            }
            Event::User { ts, text } => {
                if let (Node::Pane(_), Some((sender, title, body))) = (source, parse_from(&text)) {
                    // A pane that closed before we saw it has no title; the envelope names it.
                    let agent = self.agent(sender);
                    if agent.pane.title.is_empty() && !agent.present {
                        agent.pane.title = title.unwrap_or_default();
                    }
                    self.inbound(sender, pane, ts, body);
                } else if matches!(source, Node::Pane(_)) {
                    self.agent(pane).last_user = Some((ts, text));
                }
            }
            Event::Assistant {
                ts,
                text,
                tokens,
                cost,
            } => {
                let agent = self.agent(pane);
                match source {
                    Node::Sub(_, name) => {
                        agent.subs.entry(name.clone()).or_default().tokens += tokens
                    }
                    Node::Pane(_) => {
                        agent.tokens += tokens;
                        agent.cost += cost;
                        if !text.trim().is_empty() {
                            agent.last_reply = Some((ts, text));
                        }
                    }
                }
            }
            Event::ToolCall {
                ts,
                id,
                name,
                args,
                intent,
            } => self.tool_call(source, ts, id, name, args, intent),
            Event::ToolResult {
                ts,
                id,
                name,
                error,
                tern_error,
                details,
            } => {
                // A shell call fails for many reasons; only the CLI's own error line means the send failed.
                let failed = if name == "bash" { tern_error } else { error };
                self.tool_result(source, ts, &id, &name, failed, &details)
            }
            Event::IrcIn { .. } => {}
            Event::Todo { done, total } => {
                if matches!(source, Node::Pane(_)) {
                    self.agent(pane).todo = Some((done, total)).filter(|(_, total)| *total > 0);
                }
            }
            Event::Exit { .. } => {
                if let Node::Sub(_, name) = source {
                    let sub = self.agent(pane).subs.entry(name.clone()).or_default();
                    sub.done = true;
                    sub.tool = None;
                }
            }
        }
    }

    fn tool_call(
        &mut self,
        source: &Node,
        ts: i64,
        id: String,
        name: String,
        args: Value,
        intent: String,
    ) {
        let pane = source.pane();
        match source {
            Node::Pane(_) => self
                .agent(pane)
                .open_calls
                .push((id.clone(), name.clone(), intent)),
            Node::Sub(_, sub) => {
                self.agent(pane).subs.entry(sub.clone()).or_default().tool = Some(name.clone())
            }
        }
        match name.as_str() {
            "bash" => {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for (target, text) in tern_sends(command) {
                    let seq = self.outbound(source.clone(), target, ts, text);
                    self.pending_calls.entry(id.clone()).or_default().push(seq);
                }
            }
            "write" => {
                if args
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| path.starts_with("agent://"))
                    && let Some(text) = args.get("content").and_then(Value::as_str)
                {
                    self.irc_calls
                        .entry(source.clone())
                        .or_default()
                        .insert(id.clone(), (ts, text.to_owned()));
                }
                // tern-control's typed terminal device: `write xd://tern_terminal {action, block, …}`.
                if args.get("path").and_then(Value::as_str) == Some("xd://tern_terminal") {
                    let content = match args.get("content") {
                        Some(Value::String(raw)) => {
                            serde_json::from_str(raw).unwrap_or(Value::Null)
                        }
                        Some(value) => value.clone(),
                        None => Value::Null,
                    };
                    let action = content
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let target = content
                        .get("block")
                        .and_then(|b| b.as_u64().or_else(|| b.as_str()?.parse().ok()));
                    let text = content
                        .get("command")
                        .or_else(|| content.get("text"))
                        .and_then(Value::as_str);
                    if let (true, Some(target), Some(text)) = (
                        matches!(action, "run" | "send_text" | "paste"),
                        target,
                        text,
                    ) {
                        let seq = self.outbound(source.clone(), target, ts, text.to_string());
                        self.pending_calls.entry(id).or_default().push(seq);
                    }
                }
            }
            "task" => {
                let names = args
                    .get("tasks")
                    .and_then(Value::as_array)
                    .map(|tasks| {
                        tasks
                            .iter()
                            .filter_map(|t| t.get("name")?.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .or_else(|| {
                        args.get("name")
                            .and_then(Value::as_str)
                            .map(|n| vec![n.to_string()])
                    })
                    .unwrap_or_default();
                for child in names {
                    let child = match source {
                        Node::Sub(_, parent) => format!("{parent}.{child}"),
                        Node::Pane(_) => child,
                    };
                    let agent = self.agent(pane);
                    let sub = agent.subs.entry(child.clone()).or_default();
                    sub.last_ms = sub.last_ms.max(ts);
                    sub.done = false;
                    let text = args
                        .get("context")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.record(
                        Kind::Spawn,
                        source.clone(),
                        Node::Sub(pane, child),
                        text,
                        Some(ts),
                        Some(ts),
                        None,
                    );
                }
            }
            "yield" => {
                if let Node::Sub(_, name) = source {
                    let parent = match name.rsplit_once('.') {
                        Some((parent, _)) => Node::Sub(pane, parent.to_string()),
                        None => Node::Pane(pane),
                    };
                    let sub = self.agent(pane).subs.entry(name.clone()).or_default();
                    sub.done = true;
                    sub.tool = None;
                    let text = args.to_string();
                    self.record(
                        Kind::Yield,
                        source.clone(),
                        parent,
                        text,
                        Some(ts),
                        Some(ts),
                        None,
                    );
                }
            }
            _ => {}
        }
    }

    fn tool_result(
        &mut self,
        source: &Node,
        ts: i64,
        id: &str,
        name: &str,
        error: bool,
        details: &Value,
    ) {
        let pane = source.pane();
        if let Node::Pane(_) = source {
            self.agent(pane)
                .open_calls
                .retain(|(call, _, _)| call != id);
        }
        if let Some(seqs) = self.pending_calls.remove(id)
            && error
        {
            for seq in seqs {
                if let Some(message) = self.messages.iter_mut().find(|m| m.seq == seq) {
                    message.failed = true;
                    if self.live {
                        self.changes.push(Change::Failed(seq));
                    }
                }
            }
        }
        let send = self
            .irc_calls
            .get_mut(source)
            .and_then(|calls| calls.remove(id));
        let Some(irc) = details.get("message").filter(|_| name == "write") else {
            return;
        };
        if irc.get("op").and_then(Value::as_str) != Some("send") {
            return;
        }
        let from = irc_node(
            pane,
            irc.get("from").and_then(Value::as_str).unwrap_or("Main"),
        );
        for receipt in irc
            .get("receipts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(to) = receipt.get("to").and_then(Value::as_str) else {
                continue;
            };
            let outcome = receipt
                .get("outcome")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.record(
                Kind::Irc,
                from.clone(),
                irc_node(pane, to),
                send.as_ref()
                    .map(|(_, text)| text.clone())
                    .unwrap_or_default(),
                Some(send.as_ref().map_or(ts, |(sent, _)| *sent)),
                Some(ts),
                outcome,
            );
        }
    }

    /// A send recorded by the sender. Merges with a delivery already read from the recipient.
    fn outbound(&mut self, from: Node, to: PaneId, ts: i64, text: String) -> u64 {
        let body = strip_envelope(&text);
        let sender = from.pane();
        let candidate = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.kind == Kind::Pane
                    && m.sent.is_none()
                    && m.from.pane() == sender
                    && m.to == Node::Pane(to)
            })
            .filter(|(_, m)| {
                m.delivered
                    .is_some_and(|d| d >= ts - MATCH_BEFORE_MS && d <= ts + MATCH_AFTER_MS)
            })
            .min_by_key(|(_, m)| m.delivered)
            .map(|(index, _)| index);
        if let Some(index) = candidate {
            let message = &mut self.messages[index];
            message.sent = Some(ts);
            message.from = from;
            let seq = message.seq;
            self.classify_repeats(index);
            if self.live {
                self.changes.push(Change::Sent(seq));
                self.changes.push(Change::Delivered(seq));
            }
            return seq;
        }
        self.agent(to);
        let seq = self.record(Kind::Pane, from, Node::Pane(to), body, Some(ts), None, None);
        self.link_reply(seq);
        seq
    }

    /// A delivery recorded by the recipient. Merges with a send already read from the sender.
    fn inbound(&mut self, sender: PaneId, to: PaneId, ts: i64, body: String) {
        let candidate = self
            .messages
            .iter_mut()
            .filter(|m| {
                m.kind == Kind::Pane
                    && m.delivered.is_none()
                    && m.from.pane() == sender
                    && m.to == Node::Pane(to)
            })
            .filter(|m| {
                m.sent
                    .is_some_and(|s| s <= ts + MATCH_BEFORE_MS && s >= ts - MATCH_AFTER_MS)
            })
            .min_by_key(|m| m.sent);
        if let Some(message) = candidate {
            message.delivered = Some(ts);
            let seq = message.seq;
            if self.live {
                self.changes.push(Change::Delivered(seq));
            }
            return;
        }
        self.agent(sender);
        let seq = self.record(
            Kind::Pane,
            Node::Pane(sender),
            Node::Pane(to),
            body,
            None,
            Some(ts),
            None,
        );
        self.link_reply(seq);
        self.classify_inbound(self.messages.len() - 1);
    }

    /// Normal transcript appends need only one scan; older history needs a full regroup.
    fn classify_inbound(&mut self, index: usize) {
        let message = &self.messages[index];
        if !message.text.chars().any(|c| !c.is_whitespace()) {
            return;
        }
        let mut root = None;
        for candidate in &self.messages[..index] {
            if candidate.sent.is_some()
                || candidate.delivered.is_none()
                || !same_repeat_group(message, candidate)
            {
                continue;
            }
            if candidate.time() > message.time() {
                self.classify_repeats(index);
                return;
            }
            if candidate.repeat_of.is_none()
                && message.time() <= candidate.time().saturating_add(REPEAT_WINDOW_MS)
            {
                root = Some(candidate.seq);
            }
        }
        self.messages[index].repeat_of = root;
    }

    /// Recompute only this body and endpoint group, so late history or send evidence cannot
    /// leave links to a newer delivery or to a message that is no longer inbound-only.
    fn classify_repeats(&mut self, index: usize) {
        if !self.messages[index]
            .text
            .chars()
            .any(|c| !c.is_whitespace())
        {
            return;
        }
        for other in 0..self.messages.len() {
            if same_repeat_group(&self.messages[index], &self.messages[other]) {
                self.messages[other].repeat_of = None;
            }
        }
        let mut after = None;
        loop {
            let root = self
                .messages
                .iter()
                .filter(|message| {
                    message.sent.is_none()
                        && message.delivered.is_some()
                        && after.is_none_or(|end| message.time() > end)
                        && same_repeat_group(&self.messages[index], message)
                })
                .min_by_key(|message| (message.time(), message.seq));
            let Some(root) = root else { break };
            let (seq, start) = (root.seq, root.time());
            let end = start.saturating_add(REPEAT_WINDOW_MS);
            for other in 0..self.messages.len() {
                let message = &self.messages[other];
                if message.seq != seq
                    && message.sent.is_none()
                    && message.delivered.is_some()
                    && message.time() >= start
                    && message.time() <= end
                    && same_repeat_group(&self.messages[index], message)
                {
                    self.messages[other].repeat_of = Some(seq);
                }
            }
            after = Some(end);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        kind: Kind,
        from: Node,
        to: Node,
        text: String,
        sent: Option<i64>,
        delivered: Option<i64>,
        outcome: Option<String>,
    ) -> u64 {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.messages.push(Message {
            seq,
            kind,
            from,
            to,
            text,
            sent,
            delivered,
            outcome,
            failed: false,
            reply_to: None,
            repeat_of: None,
            replied: false,
        });
        if self.live {
            self.changes.push(Change::Sent(seq));
            if delivered.is_some() {
                self.changes.push(Change::Delivered(seq));
            }
        }
        seq
    }

    /// Marks `seq` as the reply to the latest unanswered message that went the other way.
    fn link_reply(&mut self, seq: u64) {
        let Some(message) = self.messages.iter().find(|m| m.seq == seq) else {
            return;
        };
        let (from, to, at) = (message.from.pane(), message.to.pane(), message.time());
        let original = self
            .messages
            .iter()
            .filter(|m| m.kind == Kind::Pane && !m.replied && m.seq != seq)
            .filter(|m| m.from.pane() == to && m.to.pane() == from)
            .filter(|m| m.time() <= at && at - m.time() <= REPLY_WINDOW_MS)
            .max_by_key(|m| m.time())
            .map(|m| m.seq);
        if let Some(original) = original {
            if let Some(m) = self.messages.iter_mut().find(|m| m.seq == original) {
                m.replied = true;
            }
            if let Some(m) = self.messages.iter_mut().find(|m| m.seq == seq) {
                m.reply_to = Some(original);
            }
        }
    }

    fn agent(&mut self, pane: PaneId) -> &mut Agent {
        self.agents.entry(pane).or_insert_with(|| Agent {
            pane: Pane {
                id: pane,
                address: pane.to_string(),
                ..Pane::default()
            },
            ..Agent::default()
        })
    }

    pub fn message(&self, seq: u64) -> Option<&Message> {
        self.messages.iter().find(|m| m.seq == seq)
    }

    /// Display name of a node: the pane's short label, or the subagent name.
    pub fn name(&self, node: &Node) -> String {
        match node {
            Node::Pane(pane) => self
                .agents
                .get(pane)
                .map(|a| a.label().1)
                .unwrap_or_else(|| format!("pane {pane}")),
            Node::Sub(_, name) => name.rsplit('.').next().unwrap_or(name).to_string(),
        }
    }

    pub fn running_subs(&self, now: i64) -> usize {
        self.agents
            .values()
            .flat_map(|a| a.subs.values())
            .filter(|s| s.running(now))
            .count()
    }
}

/// Compare without allocating normalized strings or changing the stored message body.
fn same_repeat_group(a: &Message, b: &Message) -> bool {
    a.kind == Kind::Pane
        && b.kind == Kind::Pane
        && a.from.pane() == b.from.pane()
        && a.to == b.to
        && a.text
            .chars()
            .filter(|c| !c.is_whitespace())
            .eq(b.text.chars().filter(|c| !c.is_whitespace()))
}

fn event_time(event: &Event) -> Option<i64> {
    match event {
        Event::User { ts, .. }
        | Event::Assistant { ts, .. }
        | Event::ToolCall { ts, .. }
        | Event::ToolResult { ts, .. }
        | Event::IrcIn { ts, .. }
        | Event::Exit { ts } => Some(*ts).filter(|ts| *ts > 0),
        Event::Model(_) | Event::Todo { .. } => None,
    }
}

fn irc_node(pane: PaneId, name: &str) -> Node {
    if name.eq_ignore_ascii_case("main") {
        Node::Pane(pane)
    } else {
        Node::Sub(pane, name.to_string())
    }
}

static FROM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\s*From (\d+)(?: \(([^)]*)\))?:\s*(.*)$").unwrap());
static ASSIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)=(\d+)\b").unwrap());
static TERN_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\btern\s+(run|send)\s").unwrap());
static REPLY_HINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\s*Reply with: tern run .*$").unwrap());
/// The envelope as the sender typed it, before the shell expanded `$TERN_PANE`.
static ENVELOPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*From (?:\d+|\$\{?TERN_PANE\}?)(?: \([^)]*\))?:\s*").unwrap());

/// `From <pane> (<title>): <body>` → (pane, title, body). Generic titles such as `(omp)` name the
/// harness, not the agent, and come back as `None`.
pub fn parse_from(text: &str) -> Option<(PaneId, Option<String>, String)> {
    let captures = FROM.captures(text)?;
    let pane = captures[1].parse().ok()?;
    let title = captures
        .get(2)
        .map(|t| t.as_str().trim().to_string())
        .filter(|t| {
            !t.is_empty()
                && !["omp", "pi", "codex", "claude"].contains(&t.to_ascii_lowercase().as_str())
        });
    Some((
        pane,
        title,
        REPLY_HINT.replace(&captures[3], "").trim().to_string(),
    ))
}

/// The message without the `From …:` envelope and the reply instruction the skill appends.
fn strip_envelope(text: &str) -> String {
    let body = ENVELOPE.replace(text, "");
    REPLY_HINT.replace(&body, "").trim().to_string()
}

/// Every `tern run <pane> <text>` and `tern send <pane> text|paste <text>` in a shell command.
/// Resolves `NAME=<digits>` assignments made in the same command; skips `$TERN_PANE` (the sender
/// itself) and targets given by title.
pub fn tern_sends(command: &str) -> Vec<(PaneId, String)> {
    let vars: HashMap<&str, &str> = ASSIGN
        .captures_iter(command)
        .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
        .collect();
    let mut sends = Vec::new();
    for found in TERN_CALL.captures_iter(command) {
        let start = found.get(0).unwrap().start();
        // `tern run …` inside a quoted message (a reply instruction) is text, not a command.
        if quoted_at(command, start) {
            continue;
        }
        let verb = &found[1];
        let words = shell_words(&command[found.get(0).unwrap().end()..]);
        let (target, text) = match (verb, words.as_slice()) {
            ("run", [target, text, ..]) => (target, text),
            ("send", [target, mode, text, ..]) if mode == "text" || mode == "paste" => {
                (target, text)
            }
            _ => continue,
        };
        let target = target
            .trim_start_matches('$')
            .trim_start_matches('{')
            .trim_end_matches('}');
        let target = vars.get(target).copied().unwrap_or(target);
        if let Ok(pane) = target.parse() {
            sends.push((pane, text.clone()));
        }
    }
    sends
}

/// True when byte `at` of the shell command lies inside single or double quotes.
fn quoted_at(command: &str, at: usize) -> bool {
    let (mut single, mut double, mut escaped) = (false, false, false);
    for c in command[..at].chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if !single => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ => {}
        }
    }
    single || double
}

/// Shell words up to the end of the simple command (`;`, `&`, `|` or a newline outside quotes).
fn shell_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(next @ ('"' | '\\' | '$' | '`')) => word.push(next),
                            Some(next) => {
                                word.push('\\');
                                word.push(next);
                            }
                            None => {}
                        },
                        c => word.push(c),
                    }
                }
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            ';' | '&' | '|' | '\n' => break,
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(ts: i64, text: &str) -> Event {
        Event::User {
            ts,
            text: text.to_string(),
        }
    }

    fn bash(ts: i64, id: &str, command: &str) -> Event {
        Event::ToolCall {
            ts,
            id: id.into(),
            name: "bash".into(),
            args: serde_json::json!({"command": command}),
            intent: String::new(),
        }
    }

    #[test]
    fn finds_sends_with_quotes_variables_and_chains() {
        let command = r#"A=395; B=399; tern run $B "From $TERN_PANE (omp): hi; there \"x\"" && tern send ${A} paste 'multi
line' ; tern run $TERN_PANE "self" ; tern send 7 keys enter ; tern run SDK "by title""#;
        assert_eq!(
            tern_sends(command),
            vec![
                (399, "From $TERN_PANE (omp): hi; there \"x\"".to_string()),
                (395, "multi\nline".to_string())
            ]
        );
    }

    #[test]
    fn a_reply_instruction_inside_the_message_is_not_a_send() {
        let command = r#"tern run 433 "From 429 (omp): What is 6 times 7? Reply with: tern run 429 \"From \$TERN_PANE: <reply>\"""#;
        assert_eq!(
            tern_sends(command)
                .iter()
                .map(|(pane, _)| *pane)
                .collect::<Vec<_>>(),
            vec![433]
        );
    }

    #[test]
    fn parses_the_envelope_and_drops_the_reply_hint() {
        let text = "From 395 (omp): What is 17 times 3? Reply with: tern run 395 \"From $TERN_PANE: <reply>\"";
        assert_eq!(
            parse_from(text),
            Some((395, None, "What is 17 times 3?".to_string()))
        );
        assert_eq!(parse_from("From me: no pane id"), None);
    }

    #[test]
    fn a_closed_sender_is_named_by_its_envelope_but_a_live_title_wins() {
        let mut model = Model::default();
        let envelope = |from: PaneId| Event::User {
            ts: 1,
            text: format!("From {from} (󰍩 SEER: Plan the merge): hi"),
        };
        model.apply_panes(vec![Pane {
            id: 2,
            title: "󰗑 DEMO: Merge".into(),
            ..Pane::default()
        }]);
        model.apply(1, None, vec![envelope(7), envelope(2)]);
        assert_eq!(model.name(&Node::Pane(7)), "SEER");
        assert_eq!(model.name(&Node::Pane(2)), "DEMO");
    }

    #[test]
    fn matches_send_and_delivery_in_either_read_order_and_links_the_reply() {
        for sender_first in [true, false] {
            let mut model = Model::default();
            let send = vec![bash(1_000, "c1", "tern run 2 \"From 1 (omp): ping\"")];
            let receive = vec![user(1_400, "From 1 (omp): ping")];
            if sender_first {
                model.apply(1, None, send);
                model.apply(2, None, receive);
            } else {
                model.apply(2, None, receive);
                model.apply(1, None, send);
            }
            model.apply(
                2,
                None,
                vec![bash(2_000, "c2", "tern run 1 \"From $TERN_PANE: pong\"")],
            );
            model.apply(1, None, vec![user(2_100, "From 2: pong")]);

            assert_eq!(model.messages.len(), 2, "sender_first={sender_first}");
            let (ping, pong) = (&model.messages[0], &model.messages[1]);
            assert_eq!(
                (ping.sent, ping.delivered, ping.text.as_str()),
                (Some(1_000), Some(1_400), "ping")
            );
            assert!(ping.replied);
            assert_eq!(
                (pong.sent, pong.delivered, pong.reply_to),
                (Some(2_000), Some(2_100), Some(ping.seq))
            );
        }
    }

    #[test]
    fn a_delivery_long_after_the_send_is_a_separate_message() {
        let mut model = Model::default();
        model.apply(1, None, vec![bash(0, "c1", "tern run 2 \"a\"")]);
        model.apply(2, None, vec![user(MATCH_AFTER_MS + 1, "From 1: a")]);
        assert_eq!(model.messages.len(), 2);
        assert_eq!(model.messages[0].delivered, None);
    }

    #[test]
    fn a_send_fails_only_when_tern_reports_an_error() {
        let mut model = Model {
            live: true,
            ..Model::default()
        };
        let result = |id: &str, tern_error| Event::ToolResult {
            ts: 1,
            id: id.into(),
            name: "bash".into(),
            error: true,
            tern_error,
            details: Value::Null,
        };
        // `tern run 2 a && grep -c x` exits 1 from grep: the shell call failed, the send did not.
        model.apply(
            1,
            None,
            vec![
                bash(0, "c1", "tern run 2 \"a\" && grep -c x"),
                result("c1", false),
            ],
        );
        model.apply(
            1,
            None,
            vec![bash(0, "c2", "tern run 3 \"b\""), result("c2", true)],
        );
        assert!(!model.messages[0].failed);
        assert!(model.messages[1].failed);
        assert_eq!(
            model.changes,
            vec![Change::Sent(1), Change::Sent(2), Change::Failed(2)]
        );
    }

    #[test]
    fn irc_bodies_follow_their_call_and_source_across_broadcast_receipts() {
        let mut model = Model::default();
        let call = |id: &str, body: &str| Event::ToolCall {
            ts: 10,
            id: id.into(),
            name: "write".into(),
            intent: String::new(),
            args: serde_json::json!({"path": "agent://all", "content": body}),
        };
        let receipt = |id: &str, from: &str| Event::ToolResult {
            ts: 20,
            id: id.into(),
            name: "write".into(),
            error: false,
            tern_error: false,
            details: serde_json::json!({"message": {"op": "send", "from": from,
                "receipts": [{"to": "Scout", "outcome": "injected"},
                             {"to": "Reviewer", "outcome": "woken"}]}}),
        };
        model.apply(9, None, vec![call("same", "parent\nfull message")]);
        model.apply(10, None, vec![call("same", "other pane")]);
        model.apply(9, Some("Worker"), vec![call("same", "worker body")]);
        model.apply(9, Some("Worker"), vec![receipt("same", "Worker")]);
        model.apply(10, None, vec![receipt("same", "Main")]);
        model.apply(9, None, vec![receipt("same", "Main")]);
        assert_eq!(model.messages.len(), 6);
        for (pair, (from, body)) in model.messages.as_chunks::<2>().0.iter().zip([
            (Node::Sub(9, "Worker".into()), "worker body"),
            (Node::Pane(10), "other pane"),
            (Node::Pane(9), "parent\nfull message"),
        ]) {
            for (message, (to, outcome)) in pair
                .iter()
                .zip([("Scout", "injected"), ("Reviewer", "woken")])
            {
                assert_eq!(message.from, from);
                assert_eq!(message.to, Node::Sub(from.pane(), to.into()));
                assert_eq!(message.text, body);
                assert_eq!(message.outcome.as_deref(), Some(outcome));
                assert_eq!(message.sent, Some(10));
                assert_eq!(message.delivered, Some(20));
            }
        }
        // A history tail may contain a receipt without its call; never reuse another body.
        model.apply(9, None, vec![receipt("missing", "Main")]);
        assert!(
            model.messages[6..]
                .iter()
                .all(|message| message.text.is_empty())
        );
        assert!(model.irc_calls.values().all(HashMap::is_empty));
    }

    #[test]
    fn irc_receipts_spawns_and_yields_become_edges_inside_the_pane() {
        let mut model = Model::default();
        let spawn = Event::ToolCall {
            ts: 1,
            id: "t".into(),
            name: "task".into(),
            intent: String::new(),
            args: serde_json::json!({"tasks": [{"name": "Scout"}]}),
        };
        let irc = Event::ToolResult {
            ts: 2,
            id: "w".into(),
            name: "write".into(),
            error: false,
            tern_error: false,
            details: serde_json::json!({"message": {"op": "send", "from": "Scout", "to": "Main",
                "receipts": [{"to": "Main", "outcome": "injected"}]}}),
        };
        let done = Event::ToolCall {
            ts: 3,
            id: "y".into(),
            name: "yield".into(),
            intent: String::new(),
            args: Value::Null,
        };
        model.apply(9, None, vec![spawn]);
        model.apply(9, Some("Scout"), vec![irc, done]);

        let edges: Vec<_> = model
            .messages
            .iter()
            .map(|m| (m.kind, m.from.clone(), m.to.clone()))
            .collect();
        let scout = Node::Sub(9, "Scout".into());
        assert_eq!(
            edges,
            vec![
                (Kind::Spawn, Node::Pane(9), scout.clone()),
                (Kind::Irc, scout.clone(), Node::Pane(9)),
                (Kind::Yield, scout, Node::Pane(9)),
            ]
        );
        assert_eq!(model.messages[1].outcome.as_deref(), Some("injected"));
        assert!(model.agents[&9].subs["Scout"].done);
    }

    #[test]
    fn splits_omp_titles() {
        assert_eq!(
            split_title("\u{f0b7} DEMO: Merge PR #327", "37"),
            ("\u{f0b7}".into(), "DEMO".into(), "Merge PR #327".into())
        );
        assert_eq!(
            split_title("omp", "5"),
            (String::new(), "omp".into(), String::new())
        );
        assert_eq!(
            split_title("", "t5"),
            (String::new(), "pane t5".into(), String::new())
        );
    }
}

#[cfg(test)]
mod repeat_tests {
    use super::*;

    #[test]
    fn reported_inbound_pair_keeps_both_bodies_and_links_the_repeat() {
        let mut model = Model::default();
        let first = "Completed the review.\nReady for the next task.";
        let second = "Completed  the review. Ready\tfor the next task.";
        for (ts, text) in [(1_793_723_902_395, first), (1_793_724_045_756, second)] {
            model.apply(
                301_601_193_459_780,
                None,
                vec![Event::User {
                    ts,
                    text: format!("From 311866165297173 (agy): {text}"),
                }],
            );
        }
        assert_eq!(model.messages.len(), 2);
        let root = &model.messages[0];
        let repeat = &model.messages[1];
        assert_ne!(root.seq, repeat.seq);
        assert_eq!(root.repeat_of, None);
        assert_eq!(repeat.repeat_of, Some(root.seq));
        assert_eq!((root.text.as_str(), repeat.text.as_str()), (first, second));
        assert_eq!((root.sent, repeat.sent), (None, None));
        assert_eq!(root.delivered, Some(1_793_723_902_395));
        assert_eq!(repeat.delivered, Some(1_793_724_045_756));
    }

    #[test]
    fn repeats_require_nonempty_matching_bodies_endpoints_and_pane_kind() {
        let mut model = Model::default();
        for (from, to, text) in [
            (1, 2, ""),
            (1, 2, " \t\n\u{2003}"),
            (1, 2, "hello"),
            (3, 2, "hello"),
            (1, 3, "hello"),
            (1, 2, "Hello"),
        ] {
            model.inbound(from, to, 10_000, text.into());
        }
        model.record(
            Kind::Irc,
            Node::Pane(1),
            Node::Pane(2),
            "hello".into(),
            None,
            Some(10_001),
            None,
        );
        model.inbound(1, 2, 10_002, "he\u{2003}llo".into());
        assert!(model.messages[..7].iter().all(|m| m.repeat_of.is_none()));
        assert_eq!(model.messages[7].repeat_of, Some(model.messages[2].seq));
    }

    #[test]
    fn repeat_window_is_inclusive_and_does_not_slide() {
        let mut model = Model::default();
        for ts in [
            0,
            REPEAT_WINDOW_MS,
            REPEAT_WINDOW_MS + 1,
            REPEAT_WINDOW_MS * 2 + 1,
        ] {
            model.inbound(1, 2, ts, "same".into());
        }
        assert_eq!(model.messages[0].repeat_of, None);
        assert_eq!(model.messages[1].repeat_of, Some(model.messages[0].seq));
        assert_eq!(model.messages[2].repeat_of, None);
        assert_eq!(model.messages[3].repeat_of, Some(model.messages[2].seq));
    }

    #[test]
    fn observed_resends_stay_separate_in_both_read_orders() {
        for sends_first in [true, false] {
            let mut model = Model::default();
            if sends_first {
                model.outbound(Node::Pane(1), 2, 9_000, "same".into());
                model.outbound(Node::Pane(1), 2, 19_000, "same".into());
            }
            model.inbound(1, 2, 10_000, "same".into());
            model.inbound(1, 2, 20_000, "same".into());
            if !sends_first {
                model.outbound(Node::Pane(1), 2, 9_000, "same".into());
                model.outbound(Node::Pane(1), 2, 19_000, "same".into());
            }
            assert_eq!(model.messages.len(), 2);
            assert!(
                model.messages.iter().all(|m| {
                    m.sent.is_some() && m.delivered.is_some() && m.repeat_of.is_none()
                })
            );
        }
    }

    #[test]
    fn late_outbound_for_either_root_or_repeat_reclassifies_remaining_deliveries() {
        for confirm_root in [true, false] {
            let mut model = Model::default();
            for ts in [10_000, 20_000, 21_000] {
                model.inbound(1, 2, ts, "same".into());
            }
            let confirmed = if confirm_root { 0 } else { 1 };
            // Subagent sends also confirm the original pane-level delivery.
            model.outbound(
                Node::Sub(1, "Worker".into()),
                2,
                if confirm_root { 9_000 } else { 19_000 },
                "same".into(),
            );
            assert_eq!(model.messages.len(), 3);
            assert!(model.messages[confirmed].sent.is_some());
            assert_eq!(model.messages[confirmed].repeat_of, None);
            let remaining_root = if confirm_root { 1 } else { 0 };
            assert_eq!(model.messages[remaining_root].repeat_of, None);
            assert_eq!(
                model.messages[2].repeat_of,
                Some(model.messages[remaining_root].seq)
            );
        }
    }

    #[test]
    fn history_arrival_order_cannot_change_chronological_groups() {
        for order in [
            [0, 120_000, 240_000, 300_000],
            [300_000, 240_000, 120_000, 0],
            [120_000, 300_000, 0, 240_000],
        ] {
            let mut model = Model::default();
            for ts in order {
                model.inbound(1, 2, ts, "same".into());
            }
            for message in &model.messages {
                let expected_root_time = match message.time() {
                    120_000 => Some(0),
                    300_000 => Some(240_000),
                    _ => None,
                };
                assert_eq!(
                    message
                        .repeat_of
                        .map(|seq| model.message(seq).unwrap().time()),
                    expected_root_time
                );
            }
        }
    }

    #[test]
    fn equal_timestamps_choose_first_sequence_and_extreme_timestamps_are_safe() {
        for ts in [i64::MIN, i64::MAX] {
            let mut model = Model::default();
            model.inbound(1, 2, ts, "same".into());
            model.inbound(1, 2, ts, "same".into());
            assert_eq!(model.messages[0].repeat_of, None);
            assert_eq!(model.messages[1].repeat_of, Some(model.messages[0].seq));
        }
    }
}
