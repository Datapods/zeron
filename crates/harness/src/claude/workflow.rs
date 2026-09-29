//! Claude `Workflow` runs → nested subagent events.
//!
//! A workflow's agents never stream on the CLI's stdout (live-verified
//! 2.1.283: after the `Workflow` tool result the wire carries nothing until the
//! parent's `task_notification`). Their state lives only in the run's
//! transcript dir, which the tool result names:
//!
//! - `journal.jsonl`: `launched`, then per agent `started{agentId,label,phase}`
//!   and a terminal `result{agentId}` or `failed{agentId}`.
//! - `agent-<agentId>.jsonl`: that agent's transcript, in the session-log shape
//!   (`user`/`assistant` lines carrying API content blocks).
//!
//! [`WorkflowTail`] polls both and renders the run as a spawn doc: phase
//! headings plus one `Agent: <label>` spawn chip per agent, each chip's own
//! transcript tagged one level deeper.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use zeron_proto::{AgentEvent, DoneStatus, ToolCall};

use super::normalize::decode_tool_use;

fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

/// `meta` fields of an inline workflow script. The Workflow tool requires
/// `export const meta = {...}` to be a pure literal, so a string-aware brace
/// scan finds it without evaluating any JS.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct WorkflowMeta {
    pub name: Option<String>,
    pub description: Option<String>,
    pub phases: Vec<String>,
}

pub(crate) fn parse_meta(script: &str) -> WorkflowMeta {
    let Some(start) = script.find("export const meta") else {
        return WorkflowMeta::default();
    };
    let Some(open) = script[start..].find('{').map(|i| start + i) else {
        return WorkflowMeta::default();
    };
    let body = &script[open..];
    let mut meta = WorkflowMeta::default();
    let mut depth = 0usize;
    let mut chars = body.char_indices().peekable();
    // The key most recently read at any depth; a string literal right after
    // `key:` is that key's value.
    let mut key = String::new();
    let mut ident = String::new();
    while let Some((_, c)) = chars.next() {
        match c {
            '{' | '[' => {
                depth += 1;
                ident.clear();
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            '\'' | '"' | '`' => {
                let mut value = String::new();
                while let Some((_, n)) = chars.next() {
                    match n {
                        '\\' => {
                            if let Some((_, escaped)) = chars.next() {
                                value.push(match escaped {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        n if n == c => break,
                        n => value.push(n),
                    }
                }
                match (depth, key.as_str()) {
                    (1, "name") => meta.name = Some(value),
                    (1, "description") => meta.description = Some(value),
                    (_, "title") if depth > 1 => meta.phases.push(value),
                    _ => {}
                }
                key.clear();
            }
            ':' => key = std::mem::take(&mut ident),
            c if c.is_alphanumeric() || c == '_' => ident.push(c),
            _ => ident.clear(),
        }
    }
    meta
}

/// The spawn-chip name for a `Workflow` call: its meta name, a saved
/// workflow's `name`, or the script file it iterates on.
pub(crate) fn chip_name(input: &Value) -> String {
    let title = input
        .get("script")
        .and_then(Value::as_str)
        .and_then(|s| parse_meta(s).name)
        .or_else(|| input.get("name").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| {
            let path = input.get("scriptPath").and_then(Value::as_str)?;
            let stem = Path::new(path).file_stem()?.to_str()?;
            // `<name>-wf_<run id>.js` is how the CLI names persisted scripts.
            Some(stem.split("-wf_").next().unwrap_or(stem).to_owned())
        })
        .filter(|t| !t.trim().is_empty());
    match title {
        Some(title) => format!("Workflow: {}", title.trim()),
        None => "Workflow".into(),
    }
}

/// What the workflow doc opens with: the run's description and phase plan.
pub(crate) fn opening_message(input: &Value) -> Option<String> {
    let meta = input
        .get("script")
        .and_then(Value::as_str)
        .map(parse_meta)
        .unwrap_or_default();
    let mut text = meta.description.unwrap_or_default();
    if !meta.phases.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("Phases: ");
        text.push_str(&meta.phases.join(" → "));
    }
    (!text.trim().is_empty()).then_some(text)
}

/// Fields the `Workflow` tool result announces, from its text content.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Launch {
    pub transcript_dir: Option<PathBuf>,
    pub task_id: Option<String>,
}

pub(crate) fn parse_launch(result_text: &str) -> Launch {
    let field = |label: &str| {
        result_text
            .lines()
            .find_map(|l| l.find(label).map(|at| &l[at + label.len()..]))
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    Launch {
        transcript_dir: field("Transcript dir:").map(PathBuf::from),
        task_id: field("Task ID:"),
    }
}

/// Complete lines appended to a file since the last read. A trailing partial
/// line (the CLI mid-write) is held until its newline lands.
struct LineReader {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl LineReader {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    fn read_new(&mut self) -> Vec<String> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let Ok(read) = file.read_to_end(&mut buf) else {
            return Vec::new();
        };
        self.offset += read as u64;
        self.partial.extend_from_slice(&buf);
        let Some(last_newline) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let rest = self.partial.split_off(last_newline + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        String::from_utf8_lossy(&complete)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }
}

struct AgentTail {
    chip_id: String,
    reader: LineReader,
    opened: bool,
    settled: bool,
    ended_with_text: bool,
}

/// One live workflow run, keyed by the `Workflow` tool-use id that launched it.
pub(crate) struct WorkflowTail {
    pub tool_id: String,
    journal: LineReader,
    dir: PathBuf,
    agents: HashMap<String, AgentTail>,
    phase: Option<String>,
}

impl WorkflowTail {
    pub fn new(tool_id: String, dir: PathBuf) -> Self {
        Self {
            tool_id,
            journal: LineReader::new(dir.join("journal.jsonl")),
            dir,
            agents: HashMap::new(),
            phase: None,
        }
    }

    /// Everything that landed on disk since the last poll.
    pub fn poll(&mut self) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for line in self.journal.read_new() {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let field = |k: &str| {
                entry
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            };
            let agent_id = field("agentId");
            match field("type").as_str() {
                "started" if !agent_id.is_empty() => {
                    self.start_agent(&agent_id, &field("label"), &field("phase"), &mut out);
                    self.drain_agent(&agent_id, &mut out);
                }
                kind @ ("result" | "failed") if !agent_id.is_empty() => {
                    // A resumed run replays cached results with no `started`.
                    self.start_agent(&agent_id, &field("label"), &field("phase"), &mut out);
                    self.drain_agent(&agent_id, &mut out);
                    self.close_with_result(&agent_id, &field("result"), &mut out);
                    let status = if kind == "failed" {
                        DoneStatus::Errored
                    } else {
                        DoneStatus::Completed
                    };
                    self.settle_agent(&agent_id, status, &mut out);
                }
                _ => {}
            }
        }
        let live: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| !a.settled)
            .map(|(id, _)| id.clone())
            .collect();
        for agent_id in live {
            self.drain_agent(&agent_id, &mut out);
        }
        out
    }

    /// Final drain once the CLI reports the run over, then settle every agent
    /// still open and the workflow chip itself.
    pub fn finish(mut self, status: DoneStatus) -> Vec<AgentEvent> {
        let mut out = self.poll();
        let open: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| !a.settled)
            .map(|(id, _)| id.clone())
            .collect();
        let agent_status = match status {
            DoneStatus::Completed => DoneStatus::Completed,
            _ => DoneStatus::Interrupted,
        };
        for agent_id in open {
            self.settle_agent(&agent_id, agent_status, &mut out);
        }
        out.push(tag(
            &self.tool_id,
            AgentEvent::Done {
                status,
                result: None,
                error: None,
                session_id: None,
            },
        ));
        out
    }

    fn start_agent(&mut self, agent_id: &str, label: &str, phase: &str, out: &mut Vec<AgentEvent>) {
        if self.agents.contains_key(agent_id) {
            return;
        }
        if !phase.is_empty() && self.phase.as_deref() != Some(phase) {
            self.phase = Some(phase.to_owned());
            out.push(tag(
                &self.tool_id,
                AgentEvent::TextDelta {
                    text: format!("**{phase}**\n\n"),
                },
            ));
        }
        // Tool-use ids are globally unique and doc-id clean; the agent id
        // keeps the chip id unique within the run.
        let chip_id = format!("{}-{agent_id}", self.tool_id);
        let name = if label.is_empty() {
            "Agent".to_owned()
        } else {
            format!("Agent: {label}")
        };
        out.push(tag(
            &self.tool_id,
            AgentEvent::ToolCall {
                id: chip_id.clone(),
                call: ToolCall::Unknown { name, input: None },
            },
        ));
        self.agents.insert(
            agent_id.to_owned(),
            AgentTail {
                chip_id,
                reader: LineReader::new(self.dir.join(format!("agent-{agent_id}.jsonl"))),
                opened: false,
                settled: false,
                ended_with_text: false,
            },
        );
    }

    fn drain_agent(&mut self, agent_id: &str, out: &mut Vec<AgentEvent>) {
        let Some(agent) = self.agents.get_mut(agent_id) else {
            return;
        };
        if agent.settled {
            return;
        }
        for line in agent.reader.read_new() {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            for event in transcript_events(&entry, &mut agent.opened) {
                agent.ended_with_text = matches!(event, AgentEvent::TextDelta { .. });
                out.push(tag(&self.tool_id, tag(&agent.chip_id, event)));
            }
        }
    }

    /// Agents that answer through a structured-output tool end their
    /// transcript on a tool call; the journal's result is the answer.
    fn close_with_result(&mut self, agent_id: &str, result: &str, out: &mut Vec<AgentEvent>) {
        let Some(agent) = self.agents.get_mut(agent_id) else {
            return;
        };
        if agent.settled || agent.ended_with_text || result.trim().is_empty() {
            return;
        }
        let text = match serde_json::from_str::<Value>(result) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => format!(
                "```json\n{}\n```\n\n",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| result.to_owned())
            ),
            _ => format!("{}\n\n", result.trim_end()),
        };
        agent.ended_with_text = true;
        out.push(tag(
            &self.tool_id,
            tag(&agent.chip_id, AgentEvent::TextDelta { text }),
        ));
    }

    fn settle_agent(&mut self, agent_id: &str, status: DoneStatus, out: &mut Vec<AgentEvent>) {
        let Some(agent) = self.agents.get_mut(agent_id) else {
            return;
        };
        if agent.settled {
            return;
        }
        agent.settled = true;
        let is_error = matches!(status, DoneStatus::Errored);
        out.push(tag(
            &self.tool_id,
            AgentEvent::ToolResult {
                id: agent.chip_id.clone(),
                is_error,
                output: None,
                diff: None,
            },
        ));
        out.push(tag(
            &self.tool_id,
            tag(
                &agent.chip_id,
                AgentEvent::Done {
                    status,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ),
        ));
    }
}

/// The workflow harness wraps each agent's computed task in a trust preamble
/// and indents it two spaces; the transcript shows the task itself.
fn strip_task_preamble(text: &str) -> String {
    const MARKER: &str = "The computed task text follows:";
    if !text.starts_with("[Workflow harness") {
        return text.to_owned();
    }
    let Some(at) = text.find(MARKER) else {
        return text.to_owned();
    };
    text[at + MARKER.len()..]
        .lines()
        .map(|l| l.strip_prefix("  ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn is_synthetic(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<system-reminder>") || t.starts_with("[Request interrupted")
}

/// One session-log line of an agent transcript → its unified events.
fn transcript_events(entry: &Value, opened: &mut bool) -> Vec<AgentEvent> {
    let content = entry.pointer("/message/content");
    match entry.get("type").and_then(Value::as_str) {
        Some("user") => {
            let mut out = Vec::new();
            let mut user_text = |text: &str, out: &mut Vec<AgentEvent>| {
                if text.trim().is_empty() || is_synthetic(text) {
                    return;
                }
                let text = if *opened {
                    text.to_owned()
                } else {
                    strip_task_preamble(text)
                };
                *opened = true;
                out.push(AgentEvent::UserMessage { text });
            };
            match content {
                Some(Value::String(text)) => user_text(text, &mut out),
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("tool_result") => out.push(AgentEvent::ToolResult {
                                id: block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned(),
                                is_error: block
                                    .get("is_error")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                                output: None,
                                diff: None,
                            }),
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    user_text(text, &mut out);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            out
        }
        Some("assistant") => content
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
                "text" => {
                    let text = block.get("text").and_then(Value::as_str)?.trim_end();
                    (!text.is_empty()).then(|| AgentEvent::TextDelta {
                        text: format!("{text}\n\n"),
                    })
                }
                "tool_use" => Some(AgentEvent::ToolCall {
                    id: block.get("id").and_then(Value::as_str)?.to_owned(),
                    call: decode_tool_use(
                        block.get("name").and_then(Value::as_str).unwrap_or(""),
                        block.get("input").unwrap_or(&Value::Null),
                    ),
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;

    const SCRIPT: &str = r#"export const meta = {
  name: 'review-changes',
  description: 'Review the diff, then verify "each" finding',
  phases: [{ title: 'Review', detail: 'x: y' }, { title: 'Verify' }],
}
const agent = { name: 'not-meta' }
"#;

    #[test]
    fn parses_meta_literal() {
        assert_eq!(
            parse_meta(SCRIPT),
            WorkflowMeta {
                name: Some("review-changes".into()),
                description: Some("Review the diff, then verify \"each\" finding".into()),
                phases: vec!["Review".into(), "Verify".into()],
            }
        );
        assert_eq!(parse_meta("const x = 1"), WorkflowMeta::default());
    }

    #[test]
    fn names_the_chip() {
        assert_eq!(
            chip_name(&json!({ "script": SCRIPT })),
            "Workflow: review-changes"
        );
        assert_eq!(chip_name(&json!({ "name": "spec" })), "Workflow: spec");
        assert_eq!(
            chip_name(
                &json!({ "scriptPath": "/p/workflows/scripts/logging-review-wf_14c307bb-153.js" })
            ),
            "Workflow: logging-review"
        );
        assert_eq!(chip_name(&json!({})), "Workflow");
        assert_eq!(
            opening_message(&json!({ "script": SCRIPT })).as_deref(),
            Some("Review the diff, then verify \"each\" finding\n\nPhases: Review → Verify")
        );
    }

    #[test]
    fn parses_launch_result() {
        let text = "Workflow launched in background. Task ID: w9jtcf73y\nSummary: x\nTranscript dir: /a/b/wf_1\nRun ID: wf_1\n";
        assert_eq!(
            parse_launch(text),
            Launch {
                transcript_dir: Some(PathBuf::from("/a/b/wf_1")),
                task_id: Some("w9jtcf73y".into()),
            }
        );
        assert_eq!(
            parse_launch("Error: script must export meta"),
            Launch::default()
        );
    }

    fn append(path: &Path, line: &Value) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn unwrap_tags(event: &AgentEvent) -> (Vec<&str>, &AgentEvent) {
        let mut parents = Vec::new();
        let mut leaf = event;
        while let AgentEvent::Subagent {
            parent_tool_use_id,
            event,
        } = leaf
        {
            parents.push(parent_tool_use_id.as_str());
            leaf = event;
        }
        (parents, leaf)
    }

    #[test]
    fn tails_journal_and_agent_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal.jsonl");
        let agent = dir.path().join("agent-a1.jsonl");
        let mut tail = WorkflowTail::new("toolu_wf".into(), dir.path().to_path_buf());
        assert!(tail.poll().is_empty());

        append(&journal, &json!({"type": "launched"}));
        append(
            &journal,
            &json!({"type": "started", "agentId": "a1", "label": "review:bugs", "phase": "Review"}),
        );
        append(
            &agent,
            &json!({"type": "user", "message": {"content": "[Workflow harness — computed task] blah. The computed task text follows:\n  \n  Find bugs\n    indented"}}),
        );
        append(
            &agent,
            &json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "hm"}]}}),
        );
        append(
            &agent,
            &json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "toolu_r", "name": "Read", "input": {"file_path": "/x"}}]}}),
        );
        // A partial line (CLI mid-write) waits for its newline.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&agent)
            .unwrap()
            .write_all(br#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_r"}]}}"#)
            .unwrap();

        let events = tail.poll();
        let shapes: Vec<_> = events.iter().map(unwrap_tags).collect();
        assert_eq!(shapes.len(), 4, "{shapes:#?}");
        assert!(
            matches!(shapes[0], (ref p, AgentEvent::TextDelta { text }) if p == &["toolu_wf"] && text == "**Review**\n\n")
        );
        assert!(
            matches!(shapes[1], (ref p, AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } })
            if p == &["toolu_wf"] && id == "toolu_wf-a1" && name == "Agent: review:bugs")
        );
        assert!(
            matches!(shapes[2], (ref p, AgentEvent::UserMessage { text })
            if p == &["toolu_wf", "toolu_wf-a1"] && text == "Find bugs\n  indented")
        );
        assert!(
            matches!(shapes[3], (ref p, AgentEvent::ToolCall { id, call: ToolCall::ReadFile { .. } })
            if p == &["toolu_wf", "toolu_wf-a1"] && id == "toolu_r")
        );

        std::fs::OpenOptions::new()
            .append(true)
            .open(&agent)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        append(
            &journal,
            &json!({"type": "result", "agentId": "a1", "result": "{\"bugs\":[]}"}),
        );
        let events = tail.poll();
        let shapes: Vec<_> = events.iter().map(unwrap_tags).collect();
        assert!(matches!(shapes[0], (_, AgentEvent::ToolResult { id, .. }) if id == "toolu_r"));
        // The transcript ended on a tool call: the result is the answer.
        assert!(matches!(shapes[1], (ref p, AgentEvent::TextDelta { text })
            if p == &["toolu_wf", "toolu_wf-a1"] && text == "```json\n{\n  \"bugs\": []\n}\n```\n\n"));
        assert!(
            matches!(shapes[2], (ref p, AgentEvent::ToolResult { id, is_error: false, .. })
            if p == &["toolu_wf"] && id == "toolu_wf-a1")
        );
        assert!(
            matches!(shapes[3], (ref p, AgentEvent::Done { status: DoneStatus::Completed, .. })
            if p == &["toolu_wf", "toolu_wf-a1"])
        );
        assert_eq!(shapes.len(), 4);

        // A second agent still running when the run is stopped settles as
        // interrupted, ahead of the workflow's own Done.
        append(
            &journal,
            &json!({"type": "started", "agentId": "a2", "label": "verify", "phase": "Verify"}),
        );
        let events = tail.finish(DoneStatus::Interrupted);
        let (parents, last) = unwrap_tags(events.last().unwrap());
        assert_eq!(parents, ["toolu_wf"]);
        assert!(matches!(
            last,
            AgentEvent::Done {
                status: DoneStatus::Interrupted,
                ..
            }
        ));
        assert!(
            events
                .iter()
                .map(unwrap_tags)
                .any(|(p, e)| p == ["toolu_wf", "toolu_wf-a2"]
                    && matches!(
                        e,
                        AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            ..
                        }
                    ))
        );
    }
}
