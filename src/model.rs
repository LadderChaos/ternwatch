//! Agents, subagents and the messages between them, built from Tern pane state and omp transcripts.
//!
//! Evidence model: a cross-pane message is *sent* when the sender's transcript records a
//! `tern run`/`tern send` to the pane, and *delivered* when the recipient's transcript records a
//! user turn starting `From <sender pane>`. An IRC message carries omp's own receipt outcome.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::tern::Pane;
use crate::transcript::Event;

pub type PaneId = u64;

/// How far apart a send and its delivery may be recorded.
const MATCH_BEFORE_MS: i64 = 5_000;
const MATCH_AFTER_MS: i64 = 15 * 60_000;
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
        split_title(&self.pane.title, self.pane.id)
    }

    pub fn repo(&self) -> String {
        let cwd = self.pane.cwd.trim_end_matches('/');
        cwd.rsplit('/').next().unwrap_or(cwd).to_string()
    }
}

pub fn split_title(title: &str, id: PaneId) -> (String, String, String) {
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
        format!("pane {id}")
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
    next_seq: u64,
}

impl Model {
    pub fn apply_panes(&mut self, panes: Vec<Pane>) {
        for agent in self.agents.values_mut() {
            agent.present = false;
        }
        for pane in panes {
            let agent = self.agents.entry(pane.id).or_default();
            agent.present = true;
            agent.pane = pane;
        }
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
                String::new(),
                Some(ts),
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
            .iter_mut()
            .filter(|m| {
                m.kind == Kind::Pane
                    && m.sent.is_none()
                    && m.from.pane() == sender
                    && m.to == Node::Pane(to)
            })
            .filter(|m| {
                m.delivered
                    .is_some_and(|d| d >= ts - MATCH_BEFORE_MS && d <= ts + MATCH_AFTER_MS)
            })
            .min_by_key(|m| m.delivered);
        if let Some(message) = candidate {
            message.sent = Some(ts);
            message.from = from;
            let seq = message.seq;
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
            split_title("\u{f0b7} DEMO: Merge PR #327", 37),
            ("\u{f0b7}".into(), "DEMO".into(), "Merge PR #327".into())
        );
        assert_eq!(
            split_title("omp", 5),
            (String::new(), "omp".into(), String::new())
        );
        assert_eq!(
            split_title("", 5),
            (String::new(), "pane 5".into(), String::new())
        );
    }
}
