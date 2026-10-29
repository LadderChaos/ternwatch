//! Tails omp session files (`~/.omp/agent/sessions/**.jsonl`) and turns their entries into events.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Appends of one JSONL file, read from a byte offset. The first read starts `history` bytes
/// before the end and drops the partial line it lands in.
pub struct Tail {
    pub path: PathBuf,
    offset: u64,
    started: bool,
    history: u64,
    partial: Vec<u8>,
}

impl Tail {
    pub fn new(path: PathBuf, history: u64) -> Self {
        Self {
            path,
            offset: 0,
            started: false,
            history,
            partial: Vec::new(),
        }
    }

    pub fn poll(&mut self) -> Vec<Value> {
        let Ok(mut file) = File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return Vec::new();
        };
        let mut skip_partial = false;
        if !self.started {
            self.started = true;
            self.offset = len.saturating_sub(self.history);
            skip_partial = self.offset > 0;
        } else if len < self.offset {
            // Rewritten from scratch: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::with_capacity((len - self.offset) as usize);
        if file
            .take(len - self.offset)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Vec::new();
        }
        self.offset += bytes.len() as u64;
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&bytes);
        let mut lines = data.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        // The last piece has no newline yet; keep it for the next poll.
        self.partial = lines.pop().unwrap_or_default().to_vec();
        if skip_partial && !lines.is_empty() {
            lines.remove(0);
        }
        lines
            .into_iter()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect()
    }
}

/// Facts the visualizer uses from one transcript entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Model(String),
    /// A user turn. Cross-pane messages arrive as user turns that start with `From <pane>`.
    User {
        ts: i64,
        text: String,
    },
    Assistant {
        ts: i64,
        text: String,
        tokens: u64,
        cost: f64,
    },
    ToolCall {
        ts: i64,
        id: String,
        name: String,
        args: Value,
        intent: String,
    },
    /// `tern_error`: the output has a `tern run:` / `tern send:` error line, the CLI's report of a
    /// send that failed (for example `no block is called …`).
    ToolResult {
        ts: i64,
        id: String,
        name: String,
        error: bool,
        tern_error: bool,
        details: Value,
    },
    /// An IRC message this agent received.
    IrcIn {
        ts: i64,
        from: String,
    },
    Todo {
        done: u32,
        total: u32,
    },
    Exit {
        ts: i64,
    },
}

pub fn parse(entry: &Value) -> Vec<Event> {
    let ts = timestamp(entry);
    match entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "model_change" => str_at(entry, "model")
            .map(|m| vec![Event::Model(m.to_string())])
            .unwrap_or_default(),
        "custom_message" if str_at(entry, "customType") == Some("irc:incoming") => {
            let from = entry
                .pointer("/details/from")
                .and_then(Value::as_str)
                .unwrap_or("?");
            vec![Event::IrcIn {
                ts,
                from: from.to_string(),
            }]
        }
        "custom" => match str_at(entry, "customType") {
            Some("session_exit") => vec![Event::Exit { ts }],
            Some("todo_hud_state") => todo(entry).into_iter().collect(),
            _ => Vec::new(),
        },
        "message" => message(entry, ts),
        _ => Vec::new(),
    }
}

fn message(entry: &Value, ts: i64) -> Vec<Event> {
    let Some(message) = entry.get("message") else {
        return Vec::new();
    };
    let content = message.get("content").and_then(Value::as_array);
    match message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "user" => {
            let text = match message.get("content") {
                Some(Value::String(text)) => text.clone(),
                _ => joined_text(content, "text"),
            };
            vec![Event::User { ts, text }]
        }
        "assistant" => {
            let mut events = Vec::new();
            for part in content.into_iter().flatten() {
                if part.get("type").and_then(Value::as_str) == Some("toolCall") {
                    let args = match part.get("arguments") {
                        Some(Value::String(raw)) => {
                            serde_json::from_str(raw).unwrap_or(Value::Null)
                        }
                        Some(value) => value.clone(),
                        None => Value::Null,
                    };
                    events.push(Event::ToolCall {
                        ts,
                        id: str_at(part, "id").unwrap_or_default().to_string(),
                        name: str_at(part, "name").unwrap_or_default().to_string(),
                        intent: str_at(part, "intent").unwrap_or_default().to_string(),
                        args,
                    });
                }
            }
            // `model_change` is written once per session start, often outside the history read.
            if let (Some(provider), Some(model)) =
                (str_at(message, "provider"), str_at(message, "model"))
            {
                events.push(Event::Model(format!("{provider}/{model}")));
            }
            let usage = message.get("usage");
            events.push(Event::Assistant {
                ts,
                text: joined_text(content, "text"),
                tokens: usage
                    .and_then(|u| u.get("totalTokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cost: usage
                    .and_then(|u| u.pointer("/cost/total"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            });
            events
        }
        "toolResult" => vec![Event::ToolResult {
            ts,
            id: str_at(message, "toolCallId")
                .unwrap_or_default()
                .to_string(),
            name: str_at(message, "toolName").unwrap_or_default().to_string(),
            error: message
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tern_error: {
                let text = joined_text(message.get("content").and_then(Value::as_array), "text");
                text.contains("tern run: ") || text.contains("tern send: ")
            },
            details: message.get("details").cloned().unwrap_or(Value::Null),
        }],
        _ => Vec::new(),
    }
}

fn todo(entry: &Value) -> Option<Event> {
    let fingerprint = entry.pointer("/data/fingerprint").and_then(Value::as_str)?;
    let groups: Value = serde_json::from_str(fingerprint).ok()?;
    let (mut done, mut total) = (0, 0);
    for task in groups
        .as_array()?
        .iter()
        .filter_map(|g| g.get("tasks")?.as_array())
        .flatten()
    {
        total += 1;
        if matches!(
            task.get("status").and_then(Value::as_str),
            Some("completed" | "done" | "dropped")
        ) {
            done += 1;
        }
    }
    Some(Event::Todo { done, total })
}

fn joined_text(content: Option<&Vec<Value>>, kind: &str) -> String {
    content
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Entry time in epoch milliseconds; 0 when missing.
pub fn timestamp(entry: &Value) -> i64 {
    entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.timestamp_millis())
        .unwrap_or(0)
}

/// True when the file was last written more than `age` ago.
pub fn untouched_for(path: &Path, age: std::time::Duration) -> bool {
    let since = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    since.is_some_and(|since| since > age)
}

/// Subagent transcripts of a session: `<session>/<Name>.jsonl`, nested as `<Name>/<Child>.jsonl`.
/// Returns dotted names (`Parent.Child`), as omp's IRC addresses them. Advisor transcripts
/// (`__advisor.*`) are not agents and are skipped.
pub fn subagent_files(session_file: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    collect(&session_file.with_extension(""), "", 0, &mut found);
    found
}

fn collect(dir: &Path, prefix: &str, depth: usize, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // `__advisor.*` are advisors; `<timestamp>_<id>` files are side sessions, not subagents.
        let side_session = stem.len() > 11
            && stem.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            && stem.as_bytes()[4] == b'-';
        if stem.starts_with("__") || side_session {
            continue;
        }
        let name = if prefix.is_empty() {
            stem.to_string()
        } else {
            format!("{prefix}.{stem}")
        };
        if depth < 3 {
            collect(&path.with_extension(""), &name, depth + 1, found);
        }
        found.push((name, path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn tail_skips_the_cut_line_and_keeps_an_unfinished_line_for_later() {
        let dir = std::env::temp_dir().join(format!("ternwatch-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{{\"type\":\"title\",\"pad\":\"xxxxxxxxxxxxxxxxxxxx\"}}"
        )
        .unwrap();
        writeln!(file, "{{\"n\":1}}").unwrap();
        write!(file, "{{\"n\":").unwrap();
        file.flush().unwrap();

        // 14 bytes of history start inside the title line, which is dropped as cut.
        let mut tail = Tail::new(path.clone(), 14);
        assert_eq!(tail.poll(), vec![json!({"n": 1})]);
        writeln!(file, "2}}").unwrap();
        file.flush().unwrap();
        assert_eq!(tail.poll(), vec![json!({"n": 2})]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_tool_calls_with_string_or_object_arguments() {
        let entry = json!({"type": "message", "timestamp": "2026-10-29T12:45:37.000Z", "message": {
            "role": "assistant",
            "content": [
                {"type": "toolCall", "id": "t1", "name": "bash", "arguments": "{\"command\":\"tern run 5 hi\"}"},
                {"type": "toolCall", "id": "t2", "name": "write", "arguments": {"path": "agent://Main"}}
            ],
            "usage": {"totalTokens": 10, "cost": {"total": 0.5}}
        }});
        let events = parse(&entry);
        assert!(matches!(&events[0], Event::ToolCall { name, args, .. }
            if name == "bash" && args["command"] == "tern run 5 hi"));
        assert!(
            matches!(&events[1], Event::ToolCall { args, .. } if args["path"] == "agent://Main")
        );
        assert!(matches!(events[2], Event::Assistant { tokens: 10, .. }));
    }

    #[test]
    fn counts_finished_todos() {
        let fingerprint =
            r#"[{"name":"T","tasks":[{"status":"completed"},{"status":"in_progress"}]}]"#;
        let entry = json!({"type": "custom", "customType": "todo_hud_state", "data": {"fingerprint": fingerprint}});
        assert_eq!(parse(&entry), vec![Event::Todo { done: 1, total: 2 }]);
    }
}
