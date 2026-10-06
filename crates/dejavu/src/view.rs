//! Transcript events (`transcript-view.ts`) and the `show` message view
//! (`find.ts` `showSession`).
//!
//! [`load_transcript_events`] turns any store into one ordered list of user,
//! assistant, thinking, tool-call, and tool-result events, each numbered and
//! carrying an [`EventRef`] back to its record so `scrub` can edit it.
//! [`view_transcript`] filters that list for `dejavu transcript`.

use crate::js;
use crate::opencode::{
    OpenCodeLocator, open_opencode_database, opencode_schema, opencode_session_uses_v2,
    parse_opencode_locator,
};
use crate::paths::{compact_home, js_lower, project_from_transcript_path};
use crate::reader::{
    TreeEntry, branch_entries, compaction_summary_text, droid_user_text, load_branch_entries,
    load_recall_messages, parse_json_line, parse_jsonl, read_text,
};
use crate::sources::{default_roots, source_from_locator};
use crate::types::{RecallBlock, RecallMessage, TranscriptSource};
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Where an event lives in its store, so `dejavu scrub` can edit exactly that record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum EventRef {
    /// A JSONL line (1-based) and, for block content, the block position.
    Line {
        line: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        block: Option<usize>,
    },
    /// A legacy OpenCode `part` row.
    Part {
        #[serde(rename = "partId")]
        part_id: String,
        #[serde(rename = "messageId")]
        message_id: String,
    },
    /// An OpenCode v2 `session_message` row; `item` indexes assistant
    /// `$.content` or user `$.files`, and is absent for user text.
    SessionMessage {
        #[serde(rename = "sessionMessageId")]
        session_message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        item: Option<usize>,
    },
}

/// What an event is, with its kind-specific fields.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        input: Value,
        call_id: Option<String>,
    },
    ToolResult {
        name: Option<String>,
        call_id: Option<String>,
        output: String,
        is_error: bool,
    },
}

/// One transcript event. Serializes with the TypeScript object's key order.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptEvent {
    pub body: EventBody,
    /// Position in the complete event list, stable across `--thinking` and `--no-tools` filters.
    pub index: Option<usize>,
    pub reference: Option<EventRef>,
    pub timestamp: Option<String>,
    /// A Claude or Codex tool result gets its `name` from the matching call
    /// after numbering, so JSON prints `name` after `index`. Pi and OpenCode
    /// results carry the key from the start.
    pub name_after_index: bool,
}

impl TranscriptEvent {
    pub fn new(body: EventBody) -> TranscriptEvent {
        TranscriptEvent {
            body,
            index: None,
            reference: None,
            timestamp: None,
            name_after_index: false,
        }
    }

    fn at(body: EventBody, timestamp: Option<&str>, reference: Option<EventRef>) -> Self {
        TranscriptEvent {
            body,
            index: None,
            reference,
            timestamp: timestamp.map(str::to_string),
            name_after_index: false,
        }
    }

    /// The `kind` string: `user`, `assistant`, `thinking`, `tool_call`, or `tool_result`.
    pub fn kind(&self) -> &'static str {
        match self.body {
            EventBody::User { .. } => "user",
            EventBody::Assistant { .. } => "assistant",
            EventBody::Thinking { .. } => "thinking",
            EventBody::ToolCall { .. } => "tool_call",
            EventBody::ToolResult { .. } => "tool_result",
        }
    }

    pub fn is_tool(&self) -> bool {
        matches!(
            self.body,
            EventBody::ToolCall { .. } | EventBody::ToolResult { .. }
        )
    }

    /// The call id of a tool call or result.
    pub fn call_id(&self) -> Option<&str> {
        match &self.body {
            EventBody::ToolCall { call_id, .. } | EventBody::ToolResult { call_id, .. } => {
                call_id.as_deref()
            }
            _ => None,
        }
    }
}

impl Serialize for TranscriptEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("kind", self.kind())?;
        let mut late_name = None;
        match &self.body {
            EventBody::User { text }
            | EventBody::Assistant { text }
            | EventBody::Thinking { text } => {
                map.serialize_entry("text", text)?;
            }
            EventBody::ToolCall {
                name,
                input,
                call_id,
            } => {
                map.serialize_entry("name", name)?;
                map.serialize_entry("input", input)?;
                if let Some(call_id) = call_id {
                    map.serialize_entry("callId", call_id)?;
                }
            }
            EventBody::ToolResult {
                name,
                call_id,
                output,
                is_error,
            } => {
                if self.name_after_index {
                    late_name = name.as_ref();
                } else if let Some(name) = name {
                    map.serialize_entry("name", name)?;
                }
                if let Some(call_id) = call_id {
                    map.serialize_entry("callId", call_id)?;
                }
                map.serialize_entry("output", output)?;
                map.serialize_entry("isError", is_error)?;
            }
        }
        if let Some(timestamp) = &self.timestamp {
            map.serialize_entry("timestamp", timestamp)?;
        }
        if let Some(reference) = &self.reference {
            map.serialize_entry("ref", reference)?;
        }
        if let Some(index) = self.index {
            map.serialize_entry("index", &index)?;
        }
        if let Some(name) = late_name {
            map.serialize_entry("name", name)?;
        }
        map.end()
    }
}

/// Event counts over the complete (unfiltered) event list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TranscriptCounts {
    pub user: usize,
    pub assistant: usize,
    pub thinking: usize,
    #[serde(rename = "toolCalls")]
    pub tool_calls: usize,
    #[serde(rename = "toolResults")]
    pub tool_results: usize,
}

/// `dejavu transcript`'s data: the filtered events plus counts of all events.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TranscriptView {
    pub path: String,
    pub source: TranscriptSource,
    pub project: String,
    pub counts: TranscriptCounts,
    pub events: Vec<TranscriptEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptViewOptions {
    /// Include model thinking blocks (default false).
    pub thinking: bool,
    /// Include tool calls and tool results (default true).
    pub tools: bool,
}

impl Default for TranscriptViewOptions {
    fn default() -> Self {
        TranscriptViewOptions {
            thinking: false,
            tools: true,
        }
    }
}

/// A transcript's project and its numbered events.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedEvents {
    pub project: String,
    pub events: Vec<TranscriptEvent>,
}

// ---------------------------------------------------------------------------
// viewTranscript
// ---------------------------------------------------------------------------

/// `viewTranscript(locator, options)`: loads and filters a transcript.
/// Errors with `transcript has no viewable turns` when the filter leaves nothing.
pub fn view_transcript(
    locator: &str,
    options: TranscriptViewOptions,
) -> Result<TranscriptView, String> {
    let source = source_from_locator(locator, default_roots())?;
    let loaded = load_transcript_events(locator, source)?;
    filter_view(locator, source, loaded, options)
}

/// The filtering half of [`view_transcript`], for already-loaded events.
pub fn filter_view(
    locator: &str,
    source: TranscriptSource,
    loaded: LoadedEvents,
    options: TranscriptViewOptions,
) -> Result<TranscriptView, String> {
    let counts = count_events(&loaded.events);
    let events: Vec<TranscriptEvent> = loaded
        .events
        .into_iter()
        .filter(|event| match event.body {
            EventBody::Thinking { .. } => options.thinking,
            EventBody::ToolCall { .. } | EventBody::ToolResult { .. } => options.tools,
            _ => true,
        })
        .collect();
    if events.is_empty() {
        return Err("transcript has no viewable turns".into());
    }
    Ok(TranscriptView {
        path: locator.to_string(),
        source,
        project: loaded.project,
        counts,
        events,
    })
}

/// `countEvents(events)`.
pub fn count_events(events: &[TranscriptEvent]) -> TranscriptCounts {
    let mut counts = TranscriptCounts::default();
    for event in events {
        match event.body {
            EventBody::User { .. } => counts.user += 1,
            EventBody::Assistant { .. } => counts.assistant += 1,
            EventBody::Thinking { .. } => counts.thinking += 1,
            EventBody::ToolCall { .. } => counts.tool_calls += 1,
            EventBody::ToolResult { .. } => counts.tool_results += 1,
        }
    }
    counts
}

/// `loadTranscriptEvents(locator, source)`: every event of the transcript's
/// active branch, numbered from 0.
pub fn load_transcript_events(
    locator: &str,
    source: TranscriptSource,
) -> Result<LoadedEvents, String> {
    if source == TranscriptSource::Opencode {
        return load_opencode_events(locator);
    }
    if source == TranscriptSource::Droid {
        return load_droid_events(locator);
    }
    if source == TranscriptSource::Agy {
        return load_agy_events(locator);
    }
    let entries = load_branch_entries(locator, source)?;
    let (project, events) = match source {
        TranscriptSource::Claude => (
            claude_project(locator, &entries),
            entries
                .iter()
                .flat_map(|entry| claude_events(entry, false))
                .collect(),
        ),
        TranscriptSource::Pi
        | TranscriptSource::Omp
        | TranscriptSource::Openclaw
        | TranscriptSource::Hermes => (
            crate::search::read_transcript_project(locator, source),
            entries.iter().flat_map(pi_events).collect(),
        ),
        _ => (
            codex_project(&entries),
            entries.iter().flat_map(codex_events).collect(),
        ),
    };
    Ok(LoadedEvents {
        project,
        events: name_results(events),
    })
}

/// Fills in a tool result's name from the matching earlier tool call when the
/// store records only a call id, and numbers every event.
fn name_results(mut events: Vec<TranscriptEvent>) -> Vec<TranscriptEvent> {
    let mut names: HashMap<String, String> = HashMap::new();
    for (index, event) in events.iter_mut().enumerate() {
        event.index = Some(index);
        match &mut event.body {
            EventBody::ToolCall {
                name,
                call_id: Some(call_id),
                ..
            } if !call_id.is_empty() => {
                names.insert(call_id.clone(), name.clone());
            }
            EventBody::ToolResult {
                name,
                call_id: Some(call_id),
                ..
            } if name.as_deref().is_none_or(str::is_empty) && !call_id.is_empty() => {
                // `!event.name` is also true for an empty name, which the lookup replaces.
                if let Some(found) = names.get(call_id.as_str()) {
                    *name = Some(found.clone());
                } else if name.is_some() {
                    *name = None;
                }
            }
            _ => {}
        }
    }
    events
}

fn line_ref(entry: &TreeEntry, block: Option<usize>) -> Option<EventRef> {
    Some(EventRef::Line {
        line: entry.line,
        block,
    })
}

// ---------------------------------------------------------------------------
// Claude Code: ~/.claude/projects/<project>/<session>.jsonl

fn claude_project(locator: &str, entries: &[TreeEntry]) -> String {
    match entries
        .iter()
        .find(|entry| entry.get("cwd").is_some_and(truthy))
        .and_then(TreeEntry::cwd)
    {
        Some(cwd) => compact_home(cwd).to_string(),
        None => project_from_transcript_path(locator, TranscriptSource::Claude),
    }
}

fn dialogue(role: &str, text: String) -> EventBody {
    if role == "user" {
        EventBody::User { text }
    } else {
        EventBody::Assistant { text }
    }
}

/// The events of one Claude-shaped row. `skip_injected` drops Droid's injected
/// user text ([`droid_user_text`]); Claude keeps its reminders as written.
fn claude_events(entry: &TreeEntry, skip_injected: bool) -> Vec<TranscriptEvent> {
    let Some(message) = entry.message() else {
        return Vec::new();
    };
    let role = match message.get("role").and_then(Value::as_str) {
        Some(role @ ("user" | "assistant")) => role,
        _ => return Vec::new(),
    };
    if skip_injected
        && crate::reader::is_droid_model_only(message.get("visibility").and_then(Value::as_str))
    {
        return Vec::new();
    }
    let authored = |text: String| -> Option<String> {
        if skip_injected && role == "user" {
            droid_user_text(&text).map(str::to_string)
        } else {
            Some(text)
        }
    };
    let timestamp = entry.timestamp();
    match message.get("content") {
        Some(Value::String(content)) => {
            match authored(content.clone()).filter(|text| !js_trim(text).is_empty()) {
                Some(text) => vec![TranscriptEvent::at(
                    dialogue(role, text),
                    timestamp,
                    line_ref(entry, None),
                )],
                None => Vec::new(),
            }
        }
        Some(Value::Array(blocks)) => blocks
            .iter()
            .enumerate()
            .filter_map(|(position, raw)| {
                let block = raw.as_object()?;
                let reference = line_ref(entry, Some(position));
                let body = match block.get("type").and_then(Value::as_str) {
                    Some("text") => dialogue(role, authored(nonblank(block.get("text"))?)?),
                    Some("thinking") => EventBody::Thinking {
                        text: nonblank(block.get("thinking"))?,
                    },
                    Some("tool_use") => EventBody::ToolCall {
                        name: string_or(block.get("name"), "unknown"),
                        input: js_ordered(or_empty_object(block.get("input"))),
                        call_id: string_of(block.get("id")),
                    },
                    Some("tool_result") => {
                        let mut event = TranscriptEvent::at(
                            EventBody::ToolResult {
                                name: None,
                                call_id: string_of(block.get("tool_use_id")),
                                output: text_of(block.get("content")),
                                is_error: block.get("is_error") == Some(&Value::Bool(true)),
                            },
                            timestamp,
                            reference,
                        );
                        event.name_after_index = true;
                        return Some(event);
                    }
                    Some("image") => dialogue(role, "[image]".into()),
                    _ => return None,
                };
                Some(TranscriptEvent::at(body, timestamp, reference))
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Droid: ~/.factory/sessions/<encoded cwd>/<session id>.jsonl
//
// Message rows hold Claude-shaped content; the project comes from the
// `session_start` row, which is not on the message branch.

fn load_droid_events(locator: &str) -> Result<LoadedEvents, String> {
    let entries = parse_jsonl(&read_text(locator)?);
    let project = droid_project(locator, &entries);
    let events = branch_entries(entries, TranscriptSource::Droid)
        .iter()
        .flat_map(
            |entry| match compaction_event(entry, TranscriptSource::Droid) {
                Some(event) => vec![event],
                None => claude_events(entry, true),
            },
        )
        .collect();
    Ok(LoadedEvents {
        project,
        events: name_results(events),
    })
}

fn droid_project(locator: &str, entries: &[TreeEntry]) -> String {
    match entries
        .iter()
        .find(|entry| entry.kind() == Some("session_start"))
        .and_then(TreeEntry::cwd)
        .filter(|cwd| !cwd.is_empty())
    {
        Some(cwd) => compact_home(cwd).to_string(),
        None => project_from_transcript_path(locator, TranscriptSource::Droid),
    }
}

// ---------------------------------------------------------------------------
// agy: ~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/logs/transcript_full.jsonl
//
// One step per line. A model reply lists its tool calls without IDs, and the
// steps after it report their outcomes in the same order, so results pair
// with calls first in, first out.

fn load_agy_events(locator: &str) -> Result<LoadedEvents, String> {
    let mut events = Vec::new();
    let mut pending: std::collections::VecDeque<String> = Default::default();
    for entry in parse_jsonl(&read_text(locator)?) {
        let Some(step) = crate::agy::Step::from_value(&entry.value) else {
            continue;
        };
        let timestamp = step.created_at.clone();
        let mut push = |body, block| {
            events.push(TranscriptEvent::at(
                body,
                timestamp.as_deref(),
                line_ref(&entry, block),
            ));
        };
        if let Some(text) = step.user_text() {
            push(EventBody::User { text }, None);
        } else if step.kind == "PLANNER_RESPONSE" {
            if let Some(text) = step
                .thinking
                .as_deref()
                .map(js_trim)
                .filter(|t| !t.is_empty())
            {
                push(
                    EventBody::Thinking {
                        text: text.to_string(),
                    },
                    None,
                );
            }
            if let Some(text) = step.assistant_text() {
                push(EventBody::Assistant { text }, None);
            }
            pending.clear();
            for (position, (name, input)) in step.tool_calls.iter().enumerate() {
                let call_id = format!("{}:{position}", entry.line);
                pending.push_back(call_id.clone());
                push(
                    EventBody::ToolCall {
                        name: name.clone(),
                        input: js_ordered(input.clone()),
                        call_id: Some(call_id),
                    },
                    Some(position),
                );
            }
        } else if step.is_tool_result() {
            let call_id = pending.pop_front();
            let name = call_id.is_none().then(|| js_lower(&step.kind).into_owned());
            push(
                EventBody::ToolResult {
                    name,
                    call_id,
                    output: step.tool_output(),
                    is_error: step.is_error(),
                },
                None,
            );
        }
    }
    Ok(LoadedEvents {
        project: project_from_transcript_path(locator, TranscriptSource::Agy),
        events: name_results(events),
    })
}

// ---------------------------------------------------------------------------
// Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl

fn codex_project(entries: &[TreeEntry]) -> String {
    for entry in entries {
        if entry.kind() == Some("session_meta")
            && let Some(cwd) = entry.payload().and_then(|payload| payload.get("cwd"))
            && truthy(cwd)
            && let Some(cwd) = cwd.as_str()
        {
            return compact_home(cwd).to_string();
        }
    }
    "~".into()
}

/// A compaction row's plaintext summary as a user event: a Droid `compaction_state`
/// of kind `llm_summary`, or a Codex `compacted` row with a `message`. It has no
/// source reference, so `scrub --drop` refuses it; `--pattern` still reaches it.
fn compaction_event(entry: &TreeEntry, source: TranscriptSource) -> Option<TranscriptEvent> {
    let text = match (source, entry.kind()) {
        (TranscriptSource::Droid, Some("compaction_state"))
            if entry.str_field("summaryKind") == Some("llm_summary") =>
        {
            compaction_summary_text(
                entry.str_field("summaryText")?,
                entry.get("removedCount").and_then(Value::as_f64),
            )
        }
        (TranscriptSource::Codex, Some("compacted")) => compaction_summary_text(
            entry.payload()?.get("message").and_then(Value::as_str)?,
            None,
        ),
        _ => None,
    }?;
    Some(TranscriptEvent::at(
        dialogue("user", text),
        entry.timestamp(),
        None,
    ))
}

fn codex_events(entry: &TreeEntry) -> Vec<TranscriptEvent> {
    if let Some(event) = compaction_event(entry, TranscriptSource::Codex) {
        return vec![event];
    }
    if entry.kind() != Some("response_item") {
        return Vec::new();
    }
    let Some(payload) = entry.payload() else {
        return Vec::new();
    };
    let timestamp = entry.timestamp();
    let reference = line_ref(entry, None);
    let call_id = || string_of(payload.get("call_id"));
    let body = match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = match payload.get("role").and_then(Value::as_str) {
                Some(role @ ("user" | "assistant")) => role,
                _ => return Vec::new(),
            };
            let text = if role == "user" {
                codex_user_text(payload.get("content"))
            } else {
                text_of(payload.get("content"))
            };
            if js_trim(&text).is_empty() {
                return Vec::new();
            }
            dialogue(role, text)
        }
        Some("reasoning") => {
            let parts: Vec<String> = [
                text_of(payload.get("summary")),
                text_of(payload.get("content")),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
            let text = parts.join("\n");
            if js_trim(&text).is_empty() {
                return Vec::new();
            }
            EventBody::Thinking { text }
        }
        Some("function_call") => EventBody::ToolCall {
            name: string_or(payload.get("name"), "unknown"),
            input: js_ordered(parse_json_arguments(payload.get("arguments"))),
            call_id: call_id(),
        },
        Some("custom_tool_call") => EventBody::ToolCall {
            name: string_or(payload.get("name"), "unknown"),
            input: js_ordered(match payload.get("input") {
                None | Some(Value::Null) => Value::String(String::new()),
                Some(value) => value.clone(),
            }),
            call_id: call_id(),
        },
        Some("local_shell_call") => {
            let action = payload.get("action");
            let command = action
                .and_then(Value::as_object)
                .and_then(|action| action.get("command"))
                .filter(|command| !command.is_null());
            let input = match (command, action) {
                (Some(command), _) => command.clone(),
                (None, Some(action)) if !action.is_null() => action.clone(),
                _ => Value::Object(Map::new()),
            };
            EventBody::ToolCall {
                name: "shell".into(),
                input: js_ordered(input),
                call_id: call_id(),
            }
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let output = text_of(payload.get("output"));
            let is_error = looks_like_codex_error(&output);
            let mut event = TranscriptEvent::at(
                EventBody::ToolResult {
                    name: None,
                    call_id: call_id(),
                    output,
                    is_error,
                },
                timestamp,
                reference,
            );
            event.name_after_index = true;
            return vec![event];
        }
        _ => return Vec::new(),
    };
    vec![TranscriptEvent::at(body, timestamp, reference)]
}

/// A function call's `arguments`: parsed JSON when it is a JSON string, else as stored.
fn parse_json_arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => {
            parse_json_line::<Value>(text).unwrap_or_else(|| Value::String(text.clone()))
        }
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(value) => value.clone(),
    }
}

/// `/^(?:Script failed|Error:|Process exited with code [1-9]|Exit code: [1-9])/i` on the trimmed start.
/// Codex sends AGENTS.md, environment context, and plugin lists as user content
/// blocks. `find` already ignores them; drop them here so show and transcript
/// open on the user's own words.
fn codex_user_text(content: Option<&Value>) -> String {
    let Some(Value::Array(items)) = content else {
        return text_of(content);
    };
    let kept: Vec<Value> = items
        .iter()
        .filter(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .is_none_or(crate::find::is_real_user_prompt)
        })
        .cloned()
        .collect();
    text_of(Some(&Value::Array(kept)))
}

fn looks_like_codex_error(output: &str) -> bool {
    let text = js_trim_start(output);
    let starts = |prefix: &str| {
        text.len() >= prefix.len()
            && text.is_char_boundary(prefix.len())
            && text[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    let nonzero_after = |prefix: &str| {
        starts(prefix)
            && text
                .as_bytes()
                .get(prefix.len())
                .is_some_and(|b| (b'1'..=b'9').contains(b))
    };
    starts("Script failed")
        || starts("Error:")
        || nonzero_after("Process exited with code ")
        || nonzero_after("Exit code: ")
}

// ---------------------------------------------------------------------------
// Pi: ~/.pi/agent/sessions/<project>/<session>.jsonl

fn pi_events(entry: &TreeEntry) -> Vec<TranscriptEvent> {
    if entry.kind() != Some("message") {
        return Vec::new();
    }
    let Some(message) = entry.message() else {
        return Vec::new();
    };
    let Some(role) = message.get("role").filter(|role| truthy(role)) else {
        return Vec::new();
    };
    let timestamp = entry.timestamp();
    let role = match role.as_str() {
        Some("toolResult") => {
            return vec![TranscriptEvent::at(
                EventBody::ToolResult {
                    name: string_of(message.get("toolName")),
                    call_id: string_of(message.get("toolCallId")),
                    output: text_of(message.get("content")),
                    is_error: message.get("isError") == Some(&Value::Bool(true)),
                },
                timestamp,
                line_ref(entry, None),
            )];
        }
        Some(role @ ("user" | "assistant")) => role,
        _ => return Vec::new(),
    };
    match message.get("content") {
        Some(Value::String(content)) => {
            if js_trim(content).is_empty() {
                Vec::new()
            } else {
                vec![TranscriptEvent::at(
                    dialogue(role, content.clone()),
                    timestamp,
                    line_ref(entry, None),
                )]
            }
        }
        Some(Value::Array(blocks)) => blocks
            .iter()
            .enumerate()
            .filter_map(|(position, raw)| {
                let block = raw.as_object()?;
                let body = match block.get("type").and_then(Value::as_str) {
                    Some("text") => dialogue(role, nonblank(block.get("text"))?),
                    Some("thinking") => EventBody::Thinking {
                        text: nonblank(block.get("thinking"))?,
                    },
                    Some("toolCall") => EventBody::ToolCall {
                        name: string_or(block.get("name"), "unknown"),
                        input: js_ordered(or_empty_object(block.get("arguments"))),
                        call_id: string_of(block.get("id")),
                    },
                    Some("image") => dialogue(role, "[image]".into()),
                    _ => return None,
                };
                Some(TranscriptEvent::at(
                    body,
                    timestamp,
                    line_ref(entry, Some(position)),
                ))
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// OpenCode: sqlite message + part rows

fn load_opencode_events(locator: &str) -> Result<LoadedEvents, String> {
    let OpenCodeLocator {
        database_path,
        session_id,
    } = parse_opencode_locator(locator)?;
    let database = open_opencode_database(&database_path)?;
    let schema = opencode_schema(&database)?;
    if opencode_session_uses_v2(&database, schema, &session_id)? {
        return load_opencode_v2_events(&database, &session_id);
    }
    let directory = session_directory(&database, "session", &session_id)?;
    let mut statement = database
        .prepare(
            "SELECT m.id AS message_id, m.data AS message_data, p.id AS part_id, p.data AS part_data
      FROM message m
      JOIN part p ON p.message_id = m.id
      WHERE m.session_id = ?1
      ORDER BY m.time_created, m.id, p.time_created, p.id",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([&session_id], |row| {
            Ok((
                column_text(row.get_ref(0)?),
                column_text(row.get_ref(1)?),
                column_text(row.get_ref(2)?),
                column_text(row.get_ref(3)?),
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut events = Vec::new();
    for row in rows {
        let (message_id, message_data, part_id, part_data) = row.map_err(|e| e.to_string())?;
        let message = safe_json(message_data.as_deref());
        let part = safe_json(part_data.as_deref());
        let role = if message.get("role").and_then(Value::as_str) == Some("assistant") {
            "assistant"
        } else {
            "user"
        };
        let timestamp = created_timestamp(&message)?;
        let reference = Some(EventRef::Part {
            part_id: part_id.unwrap_or_else(|| "null".into()),
            message_id: message_id.unwrap_or_else(|| "null".into()),
        });
        let push = |events: &mut Vec<TranscriptEvent>, body| {
            events.push(TranscriptEvent::at(
                body,
                timestamp.as_deref(),
                reference.clone(),
            ));
        };
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = nonblank(part.get("text")) {
                    push(&mut events, dialogue(role, text));
                }
            }
            Some("reasoning") => {
                if let Some(text) = nonblank(part.get("text")) {
                    push(&mut events, EventBody::Thinking { text });
                }
            }
            Some("file") => {
                let name = string_or(
                    part.get("filename"),
                    &string_or(part.get("mime"), "attachment"),
                );
                push(&mut events, dialogue(role, format!("[file: {name}]")));
            }
            // A slash command that runs as a subagent, such as `/usage`.
            Some("subtask") => {
                let label = match nonblank(part.get("command")) {
                    Some(command) => format!("/{command}"),
                    None => string_or(part.get("agent"), "subtask"),
                };
                let mut text = format!("[subtask {label}]");
                if let Some(description) = nonblank(part.get("description")) {
                    text = format!("{text} {description}");
                }
                if let Some(prompt) = nonblank(part.get("prompt")) {
                    text = format!("{text}\n{prompt}");
                }
                push(&mut events, dialogue(role, text));
            }
            Some("tool") => {
                let empty = Map::new();
                let state = part
                    .get("state")
                    .and_then(Value::as_object)
                    .unwrap_or(&empty);
                let name = string_or(part.get("tool"), "unknown");
                let call_id = string_of(part.get("callID"));
                push(
                    &mut events,
                    EventBody::ToolCall {
                        name: name.clone(),
                        input: js_ordered(or_empty_object(state.get("input"))),
                        call_id: call_id.clone(),
                    },
                );
                let status = state.get("status").and_then(Value::as_str);
                let is_error = status == Some("error");
                let output = if is_error {
                    text_of(state.get("error"))
                } else {
                    text_of(state.get("output"))
                };
                if status == Some("completed") || is_error {
                    push(
                        &mut events,
                        EventBody::ToolResult {
                            name: Some(name),
                            call_id,
                            output,
                            is_error,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    Ok(LoadedEvents {
        project: compact_home(directory.as_deref().unwrap_or("~")).to_string(),
        events: name_results(events),
    })
}

/// OpenCode v2 keeps one JSON row per message: user text and files, or assistant content items.
fn load_opencode_v2_events(
    database: &Connection,
    session_id: &str,
) -> Result<LoadedEvents, String> {
    let directory = session_directory(database, "session_v2", session_id)?;
    let mut statement = database
        .prepare(
            "SELECT id, type, data FROM session_message
    WHERE session_id = ?1 AND type IN ('user', 'assistant')
    ORDER BY seq",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                column_text(row.get_ref(0)?),
                column_text(row.get_ref(1)?),
                column_text(row.get_ref(2)?),
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut events = Vec::new();
    for row in rows {
        let (id, kind, data) = row.map_err(|e| e.to_string())?;
        let id = id.unwrap_or_else(|| "null".into());
        let data = safe_json(data.as_deref());
        let timestamp = created_timestamp(&data)?;
        let at = |item: Option<usize>| EventRef::SessionMessage {
            session_message_id: id.clone(),
            item,
        };
        let event =
            |body, reference| TranscriptEvent::at(body, timestamp.as_deref(), Some(reference));
        if kind.as_deref() == Some("user") {
            if let Some(text) = nonblank(data.get("text")) {
                events.push(event(EventBody::User { text }, at(None)));
            }
            if let Some(Value::Array(files)) = data.get("files") {
                for (item, raw) in files.iter().enumerate() {
                    let empty = Map::new();
                    let file = raw.as_object().unwrap_or(&empty);
                    let name =
                        string_or(file.get("name"), &string_or(file.get("mime"), "attachment"));
                    events.push(event(
                        EventBody::User {
                            text: format!("[file: {name}]"),
                        },
                        at(Some(item)),
                    ));
                }
            }
            continue;
        }
        let Some(Value::Array(content)) = data.get("content") else {
            continue;
        };
        for (item, raw) in content.iter().enumerate() {
            let empty = Map::new();
            let block = raw.as_object().unwrap_or(&empty);
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = nonblank(block.get("text")) {
                        events.push(event(EventBody::Assistant { text }, at(Some(item))));
                    }
                }
                Some("reasoning") => {
                    if let Some(text) = nonblank(block.get("text")) {
                        events.push(event(EventBody::Thinking { text }, at(Some(item))));
                    }
                }
                Some("tool") => {
                    let state = block
                        .get("state")
                        .and_then(Value::as_object)
                        .unwrap_or(&empty);
                    let name = string_or(block.get("name"), "unknown");
                    let call_id = string_of(block.get("id"));
                    events.push(event(
                        EventBody::ToolCall {
                            name: name.clone(),
                            input: js_ordered(or_empty_object(state.get("input"))),
                            call_id: call_id.clone(),
                        },
                        at(Some(item)),
                    ));
                    let status = state.get("status").and_then(Value::as_str);
                    let is_error = status == Some("error");
                    let output = if is_error {
                        let message = state
                            .get("error")
                            .and_then(Value::as_object)
                            .and_then(|error| error.get("message"));
                        match message {
                            Some(Value::String(message)) => message.clone(),
                            _ => text_of(state.get("error")),
                        }
                    } else {
                        text_of(state.get("content"))
                    };
                    if status == Some("completed") || is_error {
                        events.push(event(
                            EventBody::ToolResult {
                                name: Some(name),
                                call_id,
                                output,
                                is_error,
                            },
                            at(Some(item)),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(LoadedEvents {
        project: compact_home(directory.as_deref().unwrap_or("~")).to_string(),
        events: name_results(events),
    })
}

/// `SELECT directory FROM <table> WHERE id = ?`; `None` for a missing row or an empty directory.
fn session_directory(
    database: &Connection,
    table: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    let mut statement = database
        .prepare(&format!("SELECT directory FROM {table} WHERE id = ?1"))
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([session_id]).map_err(|e| e.to_string())?;
    let directory = match rows.next().map_err(|e| e.to_string())? {
        Some(row) => column_text(row.get_ref(0).map_err(|e| e.to_string())?),
        None => None,
    };
    Ok(directory.filter(|directory| !directory.is_empty()))
}

/// A column as text: TEXT as stored, numbers as their digits, anything else `None`.
fn column_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Integer(number) => Some(number.to_string()),
        ValueRef::Real(number) => Some(js::number_to_string(number)),
        _ => None,
    }
}

/// `JSON.parse(text)` as an object, or an empty object.
fn safe_json(text: Option<&str>) -> Map<String, Value> {
    match text.and_then(parse_json_line::<Value>) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// `new Date(data.time.created).toISOString()` when `created` is a number.
fn created_timestamp(data: &Map<String, Value>) -> Result<Option<String>, String> {
    match data
        .get("time")
        .and_then(Value::as_object)
        .and_then(|time| time.get("created"))
        .and_then(Value::as_f64)
    {
        Some(ms) => iso_from_millis(ms).map(Some),
        None => Ok(None),
    }
}

/// `new Date(ms).toISOString()`: `YYYY-MM-DDTHH:mm:ss.sssZ`. Errors with
/// `Invalid time value` outside JavaScript's date range.
pub fn iso_from_millis(ms: f64) -> Result<String, String> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return Err("Invalid time value".into());
    }
    let ms = ms.trunc() as i64;
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{}{:06}", if year < 0 { "-" } else { "+" }, year.abs())
    };
    Ok(format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    ))
}

// ---------------------------------------------------------------------------
// showSession (find.ts)
// ---------------------------------------------------------------------------

/// One message of `dejavu show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShowMessage {
    pub role: String,
    pub text: String,
}

/// `dejavu show`'s data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShowResult {
    pub path: String,
    pub source: TranscriptSource,
    #[serde(rename = "messageCount")]
    pub message_count: usize,
    pub messages: Vec<ShowMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowOptions {
    pub full: bool,
    pub around: Option<String>,
    pub tools: bool,
    pub max_chars: Option<usize>,
}

impl Default for ShowOptions {
    fn default() -> Self {
        ShowOptions {
            full: false,
            around: None,
            tools: true,
            max_chars: None,
        }
    }
}

/// `showSession(locator, options)`.
pub fn show_session(locator: &str, options: &ShowOptions) -> Result<ShowResult, String> {
    if options.max_chars == Some(0) {
        return Err("--max-chars needs an integer >= 1".into());
    }
    if options.full && options.max_chars.is_some() {
        return Err("--full cannot be combined with --max-chars".into());
    }
    let source = source_from_locator(locator, default_roots())?;
    let messages = crate::query::prepare_recall_messages(load_recall_messages(locator, None)?);
    show_messages(locator, source, messages, options)
}

/// The windowing and rendering half of [`show_session`], for loaded messages.
pub fn show_messages(
    locator: &str,
    source: TranscriptSource,
    mut messages: Vec<RecallMessage>,
    options: &ShowOptions,
) -> Result<ShowResult, String> {
    let max_chars = options.max_chars.unwrap_or(700);
    if !options.tools {
        messages = messages
            .into_iter()
            .map(|mut message| {
                message
                    .content
                    .retain(|block| !matches!(block, RecallBlock::ToolCall { .. }));
                message
            })
            .filter(|message| {
                message.content.iter().any(|block| match block {
                    RecallBlock::Image => true,
                    RecallBlock::Text { text } => !js_trim(text).is_empty(),
                    RecallBlock::ToolCall { .. } => false,
                })
            })
            .collect();
    }
    if messages.is_empty() {
        return Err("transcript has no recallable messages".into());
    }
    let mut omitted: Option<BTreeSet<usize>> = None;
    if let Some(around) = &options.around {
        let needle = js_lower(around);
        let matches: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.content.iter().any(|block| {
                    block
                        .text()
                        .is_some_and(|text| js_lower(text).contains(needle.as_ref()))
                })
            })
            .map(|(index, _)| index)
            .collect();
        if matches.is_empty() {
            return Err(format!("no message contains '{around}'"));
        }
        let mut keep = BTreeSet::new();
        for index in matches {
            for neighbor in index.saturating_sub(3)..=(index + 3).min(messages.len() - 1) {
                keep.insert(neighbor);
            }
        }
        let mut gaps = BTreeSet::new();
        let total = messages.len();
        let mut slots: Vec<Option<RecallMessage>> = messages.into_iter().map(Some).collect();
        let mut windowed = Vec::new();
        let mut previous: Option<usize> = None;
        for index in keep {
            if index > previous.map_or(0, |p| p + 1) {
                gaps.insert(windowed.len());
            }
            windowed.extend(slots[index].take());
            previous = Some(index);
        }
        if previous.is_some_and(|p| p < total - 1) {
            gaps.insert(windowed.len());
        }
        messages = windowed;
        omitted = Some(gaps);
    }
    let is_gap = |index: usize| omitted.as_ref().is_some_and(|gaps| gaps.contains(&index));
    let mut rendered = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if is_gap(index) {
            rendered.push(omitted_marker());
        }
        let parts: Vec<String> = message
            .content
            .iter()
            .map(|block| match block {
                RecallBlock::Text { text } => text.clone(),
                RecallBlock::ToolCall { name, .. } => format!("[tool: {name}]"),
                RecallBlock::Image => "[image]".into(),
            })
            .collect();
        let joined = parts.join("\n");
        let text = js_trim(&joined);
        if text.is_empty() {
            continue;
        }
        let text = if options.full || js::len(text) <= max_chars {
            text.to_string()
        } else if let Some((excerpt, _)) = options.around.as_ref().and_then(|term| {
            crate::window::centered_excerpt(text, std::slice::from_ref(term), max_chars)
        }) {
            excerpt
        } else {
            format!("{} [...]", js::prefix(text, max_chars))
        };
        rendered.push(ShowMessage {
            role: message.role.clone(),
            text,
        });
    }
    if is_gap(messages.len()) {
        rendered.push(omitted_marker());
    }
    Ok(ShowResult {
        path: locator.to_string(),
        source,
        message_count: rendered.len(),
        messages: rendered,
    })
}

fn omitted_marker() -> ShowMessage {
    ShowMessage {
        role: "…".into(),
        text: "[messages omitted]".into(),
    }
}

// ---------------------------------------------------------------------------
// JavaScript value helpers
// ---------------------------------------------------------------------------

/// JavaScript truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn string_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn string_or(value: Option<&Value>, fallback: &str) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// A string field with non-blank text.
fn nonblank(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !js_trim(text).is_empty())
        .map(str::to_string)
}

/// `value ?? {}`.
fn or_empty_object(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(value) => value.clone(),
    }
}

/// `textOf(value)`: flattens the text-bearing shapes every store uses for
/// content: a string, or a list of text-like blocks; other values as JSON.
pub fn text_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|raw| match raw {
                    Value::String(text) => Some(text.as_str()),
                    Value::Object(block) => {
                        if let Some(Value::String(text)) = block.get("text") {
                            Some(text)
                        } else if let Some(Value::String(text)) = block.get("thinking") {
                            Some(text)
                        } else if matches!(
                            block.get("type").and_then(Value::as_str),
                            Some("image" | "input_image")
                        ) {
                            Some("[image]")
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect();
            parts.join("\n")
        }
        Some(value) => js::stringify(&js_ordered(value.clone())),
    }
}

/// Whether `key` is an array index (`0` to `2^32 - 2` in canonical form),
/// which JavaScript objects enumerate first, in ascending order.
fn is_array_index(key: &str) -> bool {
    if key.is_empty() || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if key.len() > 1 && key.starts_with('0') {
        return false;
    }
    key.parse::<u64>().is_ok_and(|n| n < u32::MAX as u64)
}

/// Reorders object keys as a parsed JavaScript object holds them: array-index
/// keys first in numeric order, then the rest in insertion order. Values
/// without such keys come back unchanged.
pub fn js_ordered(mut value: Value) -> Value {
    reorder_keys(&mut value);
    value
}

/// In-place form of [`js_ordered`].
pub fn reorder_keys(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(reorder_keys),
        Value::Object(map) => {
            map.values_mut().for_each(reorder_keys);
            if map.keys().any(|key| is_array_index(key)) {
                let entries = std::mem::take(map);
                let (mut indexed, named): (Vec<_>, Vec<_>) = entries
                    .into_iter()
                    .partition(|(key, _)| is_array_index(key));
                indexed.sort_by_key(|(key, _)| key.parse::<u64>().unwrap_or(0));
                map.extend(indexed);
                map.extend(named);
            }
        }
        _ => {}
    }
}

fn is_js_space(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}'
}

/// `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

/// `String.prototype.trimStart`.
pub fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(is_js_space)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::opencode::opencode_locator;
    use crate::render::{RenderTranscriptOptions, render_transcript};
    use serde_json::json;

    /// A temporary directory removed on drop.
    pub(crate) struct TempDir(pub String);

    impl TempDir {
        pub fn new(label: &str) -> TempDir {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "dejavu-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path.to_string_lossy().into_owned())
        }

        pub fn write_jsonl(&self, relative: &str, lines: &[Value]) -> String {
            let path = format!("{}/{relative}", self.0);
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
            std::fs::write(&path, format!("{}\n", text.join("\n"))).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bare(events: &[TranscriptEvent]) -> Vec<Value> {
        events
            .iter()
            .map(|event| {
                let mut value = serde_json::to_value(event).unwrap();
                let map = value.as_object_mut().unwrap();
                map.shift_remove("index");
                map.shift_remove("ref");
                value
            })
            .collect()
    }

    fn refs(events: &[TranscriptEvent]) -> Vec<Value> {
        events
            .iter()
            .map(|event| serde_json::to_value(&event.reference).unwrap())
            .collect()
    }

    #[test]
    fn claude_follows_the_branch_and_pairs_tool_use_with_tool_result() {
        let dir = TempDir::new("transcript");
        let path = dir.write_jsonl(
            ".claude/projects/-work-demo/s.jsonl",
            &[
                json!({ "uuid": "u1", "parentUuid": null, "type": "user", "cwd": "/work/demo", "timestamp": "2026-08-01T08:00:00Z", "message": { "role": "user", "content": "hello" } }),
                json!({ "uuid": "a1", "parentUuid": "u1", "type": "assistant", "timestamp": "2026-08-01T08:00:01Z", "message": { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "plan" },
                    { "type": "text", "text": "looking" },
                    { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
                ] } }),
                json!({ "uuid": "r1", "parentUuid": "a1", "type": "user", "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "a\nb", "is_error": false }] } }),
                json!({ "uuid": "a2", "parentUuid": "r1", "type": "assistant", "message": { "role": "assistant", "content": [{ "type": "text", "text": "done" }] } }),
                json!({ "uuid": "dead", "parentUuid": "u1", "type": "assistant", "message": { "role": "assistant", "content": [{ "type": "text", "text": "abandoned branch" }] } }),
                json!({ "type": "last-prompt", "leafUuid": "a2" }),
            ],
        );
        let loaded = load_transcript_events(&path, TranscriptSource::Claude).unwrap();
        assert_eq!(loaded.project, "/work/demo");
        let indexes: Vec<_> = loaded.events.iter().map(|e| e.index).collect();
        assert_eq!(indexes, (0..6).map(Some).collect::<Vec<_>>());
        assert_eq!(
            refs(&loaded.events),
            vec![
                json!({ "line": 1 }),
                json!({ "line": 2, "block": 0 }),
                json!({ "line": 2, "block": 1 }),
                json!({ "line": 2, "block": 2 }),
                json!({ "line": 3, "block": 0 }),
                json!({ "line": 4, "block": 0 }),
            ]
        );
        assert_eq!(
            bare(&loaded.events),
            vec![
                json!({ "kind": "user", "text": "hello", "timestamp": "2026-08-01T08:00:00Z" }),
                json!({ "kind": "thinking", "text": "plan", "timestamp": "2026-08-01T08:00:01Z" }),
                json!({ "kind": "assistant", "text": "looking", "timestamp": "2026-08-01T08:00:01Z" }),
                json!({ "kind": "tool_call", "name": "Bash", "input": { "command": "ls" }, "callId": "t1", "timestamp": "2026-08-01T08:00:01Z" }),
                json!({ "kind": "tool_result", "callId": "t1", "output": "a\nb", "isError": false, "name": "Bash" }),
                json!({ "kind": "assistant", "text": "done" }),
            ]
        );
        // The late `name` key follows `index`, as nameResults added it.
        assert_eq!(
            js::stringify(&loaded.events[4]),
            r#"{"kind":"tool_result","callId":"t1","output":"a\nb","isError":false,"ref":{"line":3,"block":0},"index":4,"name":"Bash"}"#
        );
    }

    pub(crate) fn droid_file(dir: &TempDir) -> String {
        let reminder =
            json!({ "type": "text", "text": "<system-reminder>tool catalog</system-reminder>" });
        dir.write_jsonl(
            ".factory/sessions/-work-app/sess.jsonl",
            &[
                json!({ "type": "session_start", "id": "sess", "title": "t", "cwd": "/work/droid", "owner": "o", "version": 2 }),
                json!({ "type": "message", "id": "m0", "timestamp": "2026-09-01T10:00:00Z", "message": { "role": "user", "visibility": "llm_only", "content": [reminder] } }),
                json!({ "type": "message", "id": "m1", "parentId": "m0", "timestamp": "2026-09-01T10:00:01Z", "message": { "role": "user", "content": [reminder, { "type": "text", "text": "fix the build" }] } }),
                json!({ "type": "message", "id": "m2", "parentId": "m1", "message": { "role": "assistant", "content": [{ "type": "text", "text": "abandoned" }] } }),
                json!({ "type": "message", "id": "m3", "parentId": "m1", "timestamp": "2026-09-01T10:00:02Z", "message": { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "plan" },
                    { "type": "text", "text": "running" },
                    { "type": "tool_use", "id": "t1", "name": "Execute", "input": { "command": "make" } },
                ] } }),
                json!({ "type": "message", "id": "m4", "parentId": "m3", "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": [{ "type": "text", "text": "make: *** Error 2" }], "is_error": true }] } }),
                json!({ "type": "message", "id": "m5", "parentId": "m4", "message": { "role": "assistant", "content": [{ "type": "text", "text": "built" }] } }),
                json!({ "type": "agent_turn_outcome", "turnId": "x", "reason": "done" }),
            ],
        )
    }

    #[test]
    fn droid_renders_like_claude_on_the_active_branch_without_injected_reminders() {
        let dir = TempDir::new("transcript");
        let path = droid_file(&dir);
        let loaded = load_transcript_events(&path, TranscriptSource::Droid).unwrap();
        assert_eq!(loaded.project, "/work/droid");
        assert_eq!(
            refs(&loaded.events),
            vec![
                json!({ "line": 3, "block": 1 }),
                json!({ "line": 5, "block": 0 }),
                json!({ "line": 5, "block": 1 }),
                json!({ "line": 5, "block": 2 }),
                json!({ "line": 6, "block": 0 }),
                json!({ "line": 7, "block": 0 }),
            ]
        );
        assert_eq!(
            bare(&loaded.events),
            vec![
                json!({ "kind": "user", "text": "fix the build", "timestamp": "2026-09-01T10:00:01Z" }),
                json!({ "kind": "thinking", "text": "plan", "timestamp": "2026-09-01T10:00:02Z" }),
                json!({ "kind": "assistant", "text": "running", "timestamp": "2026-09-01T10:00:02Z" }),
                json!({ "kind": "tool_call", "name": "Execute", "input": { "command": "make" }, "callId": "t1", "timestamp": "2026-09-01T10:00:02Z" }),
                json!({ "kind": "tool_result", "callId": "t1", "output": "make: *** Error 2", "isError": true, "name": "Execute" }),
                json!({ "kind": "assistant", "text": "built" }),
            ]
        );

        let view = view_transcript(&path, TranscriptViewOptions::default()).unwrap();
        assert_eq!(view.source, TranscriptSource::Droid);
        assert_eq!(
            js::stringify(&view.counts),
            r#"{"user":1,"assistant":2,"thinking":1,"toolCalls":1,"toolResults":1}"#
        );
        let text = render_transcript(&view, RenderTranscriptOptions::default());
        assert!(text.contains("▶ Execute make"), "{text}");
        assert!(text.contains("◀ Execute error"), "{text}");
        assert!(!text.contains("system-reminder") && !text.contains("abandoned"));

        let shown = show_session(&path, &ShowOptions::default()).unwrap();
        assert_eq!(shown.source, TranscriptSource::Droid);
        let shown: Vec<_> = shown
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("user", "fix the build"),
                ("assistant", "running\n[tool: Execute]"),
                ("assistant", "built"),
            ]
        );

        let profile =
            crate::profile::measure_transcript(&crate::profile::profile_view(&view), 2000).unwrap();
        let profile = serde_json::to_value(&profile).unwrap();
        assert_eq!(profile["source"], "droid");
        assert_eq!(profile["project"], "/work/droid");
        assert_eq!(profile["metrics"]["toolCalls"], 1);
        assert_eq!(profile["metrics"]["errorFlaggedResults"], 1);
    }

    #[test]
    fn droid_project_falls_back_to_the_session_directory() {
        let dir = TempDir::new("transcript");
        let path = dir.write_jsonl(
            ".factory/sessions/-work-app/s.jsonl",
            &[json!({ "type": "message", "id": "m0", "message": { "role": "user", "content": "hi" } })],
        );
        let loaded = load_transcript_events(&path, TranscriptSource::Droid).unwrap();
        assert_eq!(loaded.project, "work/app");
        assert_eq!(
            bare(&loaded.events),
            vec![json!({ "kind": "user", "text": "hi" })]
        );
    }

    #[test]
    fn agy_pairs_results_with_calls_in_order_and_hides_injected_input() {
        let dir = TempDir::new("transcript");
        let path = dir.write_jsonl(
            ".gemini/antigravity-cli/brain/c1/.system_generated/logs/transcript_full.jsonl",
            &[
                json!({ "source": "USER_EXPLICIT", "type": "USER_INPUT", "created_at": "2026-10-05T10:00:00Z", "content": "<USER_REQUEST>\nlist it\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nt\n</ADDITIONAL_METADATA>" }),
                json!({ "source": "SYSTEM_SDK", "type": "USER_INPUT", "content": "<USER_REQUEST>\n<project-memory>m</project-memory>\n</USER_REQUEST>" }),
                json!({ "source": "SYSTEM", "type": "CONVERSATION_HISTORY", "content": "null" }),
                json!({ "source": "MODEL", "type": "PLANNER_RESPONSE", "thinking": "plan", "tool_calls": [
                    { "name": "run_command", "args": { "CommandLine": "\"ls\"" } },
                    { "name": "view_file", "args": { "AbsolutePath": "\"/a\"" } },
                ] }),
                json!({ "source": "MODEL", "type": "RUN_COMMAND", "status": "DONE", "content": "Created At: x\nCompleted At: y\n\na.txt" }),
                json!({ "source": "SYSTEM", "type": "ERROR_MESSAGE", "status": "DONE", "error": "no such file" }),
                json!({ "source": "MODEL", "type": "PLANNER_RESPONSE", "content": "Done." }),
            ],
        );
        let loaded = load_transcript_events(&path, TranscriptSource::Agy).unwrap();
        assert_eq!(
            bare(&loaded.events),
            vec![
                json!({ "kind": "user", "text": "list it", "timestamp": "2026-10-05T10:00:00Z" }),
                json!({ "kind": "thinking", "text": "plan" }),
                json!({ "kind": "tool_call", "name": "run_command", "input": { "CommandLine": "ls" }, "callId": "4:0" }),
                json!({ "kind": "tool_call", "name": "view_file", "input": { "AbsolutePath": "/a" }, "callId": "4:1" }),
                json!({ "kind": "tool_result", "name": "run_command", "callId": "4:0", "output": "a.txt", "isError": false }),
                json!({ "kind": "tool_result", "name": "view_file", "callId": "4:1", "output": "no such file", "isError": true }),
                json!({ "kind": "assistant", "text": "Done." }),
            ]
        );
    }

    #[test]
    fn codex_user_messages_drop_harness_instruction_blocks() {
        let dir = TempDir::new("transcript");
        let path = dir.write_jsonl(
            ".codex/sessions/2026/08/02/rollout.jsonl",
            &[
                json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "# AGENTS.md instructions for /work\n<INSTRUCTIONS>rules</INSTRUCTIONS>" },
                    { "type": "input_text", "text": "<environment_context>\n  <cwd>/work</cwd>\n</environment_context>" },
                ] } }),
                json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "<recommended_plugins>x</recommended_plugins>" },
                    { "type": "input_text", "text": "fix the build" },
                ] } }),
            ],
        );
        let loaded = load_transcript_events(&path, TranscriptSource::Codex).unwrap();
        assert_eq!(
            bare(&loaded.events),
            vec![json!({ "kind": "user", "text": "fix the build" })]
        );
    }

    #[test]
    fn codex_reads_function_custom_and_shell_calls_with_their_outputs() {
        let dir = TempDir::new("transcript");
        let path = dir.write_jsonl(
            ".codex/sessions/2026/08/02/rollout.jsonl",
            &[
                json!({ "type": "session_meta", "payload": { "cwd": "/work/codex" } }),
                json!({ "type": "response_item", "timestamp": "2026-08-02T09:00:00Z", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "fix it" }] } }),
                json!({ "type": "response_item", "payload": { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "system stuff" }] } }),
                json!({ "type": "response_item", "payload": { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "**Thinking**" }] } }),
                json!({ "type": "response_item", "payload": { "type": "function_call", "name": "wait", "arguments": "{\"ms\":5}", "call_id": "c1" } }),
                json!({ "type": "response_item", "payload": { "type": "function_call_output", "call_id": "c1", "output": "ok" } }),
                json!({ "type": "response_item", "payload": { "type": "custom_tool_call", "name": "exec", "input": "ls", "call_id": "c2" } }),
                json!({ "type": "response_item", "payload": { "type": "custom_tool_call_output", "call_id": "c2", "output": [{ "type": "input_text", "text": "Script failed\n" }, { "type": "input_text", "text": "boom" }] } }),
                json!({ "type": "response_item", "payload": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "fixed" }] } }),
                json!({ "type": "event_msg", "payload": { "type": "token_count" } }),
            ],
        );
        let loaded = load_transcript_events(&path, TranscriptSource::Codex).unwrap();
        assert_eq!(loaded.project, "/work/codex");
        let kinds: Vec<_> = loaded.events.iter().map(TranscriptEvent::kind).collect();
        assert_eq!(
            kinds,
            [
                "user",
                "thinking",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "assistant"
            ]
        );
        let e = &loaded.events;
        assert_eq!(
            e[2].body,
            EventBody::ToolCall {
                name: "wait".into(),
                input: json!({ "ms": 5 }),
                call_id: Some("c1".into())
            }
        );
        assert_eq!(
            e[3].body,
            EventBody::ToolResult {
                name: Some("wait".into()),
                call_id: Some("c1".into()),
                output: "ok".into(),
                is_error: false
            }
        );
        assert!(
            matches!(&e[4].body, EventBody::ToolCall { name, input, .. } if name == "exec" && input == "ls")
        );
        assert_eq!(
            e[5].body,
            EventBody::ToolResult {
                name: Some("exec".into()),
                call_id: Some("c2".into()),
                output: "Script failed\n\nboom".into(),
                is_error: true
            }
        );
    }

    #[test]
    fn pi_reads_tool_call_blocks_and_tool_result_messages() {
        let dir = TempDir::new("transcript");
        let path = dir.write_jsonl(
            ".pi/agent/sessions/--work--/s.jsonl",
            &[
                json!({ "type": "session", "id": "s" }),
                json!({ "type": "message", "id": "m1", "parentId": null, "timestamp": "2026-08-03T10:00:00Z", "message": { "role": "user", "content": [{ "type": "text", "text": "hi" }] } }),
                json!({ "type": "message", "id": "m2", "parentId": "m1", "message": { "role": "assistant", "content": [{ "type": "thinking", "thinking": "hm" }, { "type": "toolCall", "id": "call1", "name": "bash", "arguments": { "command": "pwd" } }] } }),
                json!({ "type": "message", "id": "m3", "parentId": "m2", "message": { "role": "toolResult", "toolCallId": "call1", "toolName": "bash", "content": [{ "type": "text", "text": "/work" }], "isError": false } }),
                json!({ "type": "message", "id": "m4", "parentId": "m3", "message": { "role": "assistant", "content": [{ "type": "text", "text": "you are in /work" }] } }),
            ],
        );
        let loaded = load_transcript_events(&path, TranscriptSource::Pi).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded.events[3].reference).unwrap(),
            json!({ "line": 4 })
        );
        assert_eq!(
            bare(&loaded.events),
            vec![
                json!({ "kind": "user", "text": "hi", "timestamp": "2026-08-03T10:00:00Z" }),
                json!({ "kind": "thinking", "text": "hm" }),
                json!({ "kind": "tool_call", "name": "bash", "input": { "command": "pwd" }, "callId": "call1" }),
                json!({ "kind": "tool_result", "name": "bash", "callId": "call1", "output": "/work", "isError": false }),
                json!({ "kind": "assistant", "text": "you are in /work" }),
            ]
        );
    }

    pub(crate) fn opencode_v2_fixture(path: &str) {
        let database = Connection::open(path).unwrap();
        database.execute_batch("CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT, time_updated INTEGER NOT NULL);
            CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
            INSERT INTO session_v2 VALUES ('ses', '/work/next', 'Next', 1);").unwrap();
        let rows = [
            (
                "u1",
                "user",
                json!({ "text": "question", "files": [{ "name": "shot.png", "mime": "image/png", "data": "AAAA" }], "time": { "created": 1_788_220_800_000_i64 } }),
            ),
            (
                "a1",
                "assistant",
                json!({ "content": [
                { "type": "reasoning", "text": "why" },
                { "type": "tool", "id": "g1", "name": "grep", "state": { "status": "completed", "input": { "pattern": "x" }, "content": [{ "type": "text", "text": "1 match" }] } },
                { "type": "tool", "id": "g2", "name": "read", "state": { "status": "error", "input": { "path": "/nope" }, "error": { "type": "tool.execution", "message": "ENOENT" } } },
                { "type": "tool", "id": "g3", "name": "bash", "state": { "status": "running", "input": { "command": "sleep" } } },
                { "type": "text", "text": "answer" },
            ] }),
            ),
            ("s1", "synthetic", json!({ "text": "hidden" })),
        ];
        for (seq, (id, kind, data)) in rows.iter().enumerate() {
            database
                .execute(
                    "INSERT INTO session_message VALUES (?1, 'ses', ?2, ?3, ?3, ?3, ?4)",
                    rusqlite::params![id, kind, seq as i64, data.to_string()],
                )
                .unwrap();
        }
    }

    #[test]
    fn opencode_v2_reads_user_text_files_and_assistant_items() {
        let dir = TempDir::new("transcript-v2");
        let path = format!("{}/v2.db", dir.0);
        opencode_v2_fixture(&path);
        let loaded =
            load_transcript_events(&opencode_locator(&path, "ses"), TranscriptSource::Opencode)
                .unwrap();
        assert_eq!(loaded.project, "/work/next");
        assert_eq!(
            refs(&loaded.events),
            vec![
                json!({ "sessionMessageId": "u1" }),
                json!({ "sessionMessageId": "u1", "item": 0 }),
                json!({ "sessionMessageId": "a1", "item": 0 }),
                json!({ "sessionMessageId": "a1", "item": 1 }),
                json!({ "sessionMessageId": "a1", "item": 1 }),
                json!({ "sessionMessageId": "a1", "item": 2 }),
                json!({ "sessionMessageId": "a1", "item": 2 }),
                json!({ "sessionMessageId": "a1", "item": 3 }),
                json!({ "sessionMessageId": "a1", "item": 4 }),
            ]
        );
        assert_eq!(
            bare(&loaded.events),
            vec![
                json!({ "kind": "user", "text": "question", "timestamp": "2026-09-01T00:00:00.000Z" }),
                json!({ "kind": "user", "text": "[file: shot.png]", "timestamp": "2026-09-01T00:00:00.000Z" }),
                json!({ "kind": "thinking", "text": "why" }),
                json!({ "kind": "tool_call", "name": "grep", "input": { "pattern": "x" }, "callId": "g1" }),
                json!({ "kind": "tool_result", "name": "grep", "callId": "g1", "output": "1 match", "isError": false }),
                json!({ "kind": "tool_call", "name": "read", "input": { "path": "/nope" }, "callId": "g2" }),
                json!({ "kind": "tool_result", "name": "read", "callId": "g2", "output": "ENOENT", "isError": true }),
                json!({ "kind": "tool_call", "name": "bash", "input": { "command": "sleep" }, "callId": "g3" }),
                json!({ "kind": "assistant", "text": "answer" }),
            ]
        );
    }

    pub(crate) fn opencode_legacy_fixture(
        path: &str,
        rows: &[(&str, &str, i64, Value)],
        parts: &[(&str, &str, i64, Value)],
    ) {
        let database = Connection::open(path).unwrap();
        database.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
            INSERT INTO session VALUES ('ses', '/work/oc', 'OC', 1);").unwrap();
        for (id, session, time, data) in rows {
            database
                .execute(
                    "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![id, session, time, data.to_string()],
                )
                .unwrap();
        }
        for (id, message, time, data) in parts {
            database
                .execute(
                    "INSERT INTO part VALUES (?1, ?2, 'ses', ?3, ?4)",
                    rusqlite::params![id, message, time, data.to_string()],
                )
                .unwrap();
        }
    }

    #[test]
    fn opencode_expands_tool_parts_into_a_call_and_a_result() {
        let dir = TempDir::new("transcript-oc");
        let path = format!("{}/oc.db", dir.0);
        opencode_legacy_fixture(
            &path,
            &[
                (
                    "m1",
                    "ses",
                    1,
                    json!({ "role": "user", "time": { "created": 1_785_801_600_000_i64 } }),
                ),
                ("m2", "ses", 2, json!({ "role": "assistant" })),
            ],
            &[
                ("p1", "m1", 1, json!({ "type": "text", "text": "question" })),
                (
                    "p1b",
                    "m1",
                    1,
                    json!({ "type": "subtask", "command": "usage", "description": "Show usage", "prompt": "Print it" }),
                ),
                ("p2", "m2", 2, json!({ "type": "reasoning", "text": "why" })),
                (
                    "p3",
                    "m2",
                    3,
                    json!({ "type": "tool", "tool": "grep", "callID": "g1", "state": { "status": "completed", "input": { "pattern": "x" }, "output": "1 match" } }),
                ),
                (
                    "p4",
                    "m2",
                    4,
                    json!({ "type": "tool", "tool": "read", "callID": "g2", "state": { "status": "error", "input": { "path": "/nope" }, "error": "ENOENT" } }),
                ),
                ("p5", "m2", 5, json!({ "type": "step-finish" })),
                ("p6", "m2", 6, json!({ "type": "text", "text": "answer" })),
            ],
        );
        let loaded =
            load_transcript_events(&opencode_locator(&path, "ses"), TranscriptSource::Opencode)
                .unwrap();
        assert_eq!(loaded.project, "/work/oc");
        assert_eq!(
            serde_json::to_value(&loaded.events[3].reference).unwrap(),
            json!({ "partId": "p3", "messageId": "m2" })
        );
        assert_eq!(
            bare(&loaded.events),
            vec![
                json!({ "kind": "user", "text": "question", "timestamp": "2026-08-04T00:00:00.000Z" }),
                json!({ "kind": "user", "text": "[subtask /usage] Show usage\nPrint it", "timestamp": "2026-08-04T00:00:00.000Z" }),
                json!({ "kind": "thinking", "text": "why" }),
                json!({ "kind": "tool_call", "name": "grep", "input": { "pattern": "x" }, "callId": "g1" }),
                json!({ "kind": "tool_result", "name": "grep", "callId": "g1", "output": "1 match", "isError": false }),
                json!({ "kind": "tool_call", "name": "read", "input": { "path": "/nope" }, "callId": "g2" }),
                json!({ "kind": "tool_result", "name": "read", "callId": "g2", "output": "ENOENT", "isError": true }),
                json!({ "kind": "assistant", "text": "answer" }),
            ]
        );
    }

    fn fixture_events() -> Vec<TranscriptEvent> {
        let output: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        vec![
            TranscriptEvent::new(EventBody::User { text: "q".into() }),
            TranscriptEvent::new(EventBody::Thinking { text: "t".into() }),
            TranscriptEvent::new(EventBody::ToolCall {
                name: "Bash".into(),
                input: json!({ "command": "ls -la" }),
                call_id: None,
            }),
            TranscriptEvent::new(EventBody::ToolResult {
                name: Some("Bash".into()),
                call_id: None,
                output: output.join("\n"),
                is_error: false,
            }),
            TranscriptEvent::new(EventBody::Assistant { text: "a".into() }),
        ]
    }

    fn fixture_view(options: TranscriptViewOptions) -> TranscriptView {
        let loaded = LoadedEvents {
            project: "p".into(),
            events: fixture_events(),
        };
        filter_view("x", TranscriptSource::Claude, loaded, options).unwrap()
    }

    #[test]
    fn hides_thinking_by_default_includes_it_on_request_and_can_drop_tools() {
        let by_default = fixture_view(TranscriptViewOptions::default());
        let kinds: Vec<_> = by_default
            .events
            .iter()
            .map(TranscriptEvent::kind)
            .collect();
        assert_eq!(kinds, ["user", "tool_call", "tool_result", "assistant"]);
        assert_eq!(
            by_default.counts,
            TranscriptCounts {
                user: 1,
                assistant: 1,
                thinking: 1,
                tool_calls: 1,
                tool_results: 1
            }
        );
        let with_thinking = fixture_view(TranscriptViewOptions {
            thinking: true,
            tools: false,
        });
        let kinds: Vec<_> = with_thinking
            .events
            .iter()
            .map(TranscriptEvent::kind)
            .collect();
        assert_eq!(kinds, ["user", "thinking", "assistant"]);
    }

    #[test]
    fn renders_labeled_turns_tool_calls_and_truncated_results() {
        let view = fixture_view(TranscriptViewOptions::default());
        let text = render_transcript(&view, RenderTranscriptOptions::default());
        assert!(text.contains("USER"));
        assert!(text.contains("ASSISTANT"));
        assert!(text.contains("▶ Bash ls -la"));
        assert!(text.contains("◀ Bash result"));
        assert!(text.contains("[... 20 lines,"));
        let full = render_transcript(
            &view,
            RenderTranscriptOptions {
                full: true,
                color: false,
            },
        );
        assert!(full.contains("line 19"));
        assert!(!full.contains("[..."));
    }

    #[test]
    fn javascript_key_order_and_timestamps() {
        let value = js_ordered(json!({ "b": 1, "10": 2, "2": 3, "01": 4 }));
        assert_eq!(js::stringify(&value), r#"{"2":3,"10":2,"b":1,"01":4}"#);
        assert_eq!(
            iso_from_millis(1_788_220_800_123.0).unwrap(),
            "2026-09-01T00:00:00.123Z"
        );
        assert_eq!(js_trim("\u{feff} x\u{85}"), "x\u{85}");
        assert!(looks_like_codex_error("  exit code: 2"));
        assert!(!looks_like_codex_error("Exit code: 0"));
    }

    fn claude_transcript(dir: &TempDir, texts: &[String]) -> String {
        let lines: Vec<Value> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                json!({
                    "uuid": format!("message-{index}"),
                    "parentUuid": if index == 0 { Value::Null } else { json!(format!("message-{}", index - 1)) },
                    "message": { "role": if index % 2 == 0 { "user" } else { "assistant" }, "content": [{ "type": "text", "text": text }] },
                    "timestamp": format!("2026-08-{:02}T08:00:00Z", index + 1),
                })
            })
            .collect();
        dir.write_jsonl(
            ".claude/projects/-tmp-project/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl",
            &lines,
        )
    }

    fn around(target: &str) -> ShowOptions {
        ShowOptions {
            around: Some(target.into()),
            ..ShowOptions::default()
        }
    }

    #[test]
    fn show_around_preserves_late_matches_and_unmatched_neighbor_prefixes() {
        let dir = TempDir::new("show-centered");
        let neighbor = "unmatched context ".repeat(80);
        let matching = format!("{}OVERWRITE{}", "İ😀é".repeat(2200), "tail ".repeat(200));
        let path = claude_transcript(&dir, &[neighbor.clone(), matching.clone()]);
        let shown = show_session(&path, &around("overwrite")).unwrap();
        assert_eq!(
            shown.messages[0].text,
            format!("{} [...]", js::prefix(&neighbor, 700))
        );
        assert!(shown.messages[1].text.contains("OVERWRITE"));
        assert!(shown.messages[1].text.starts_with('…'));
        assert!(shown.messages[1].text.ends_with('…'));
        assert!(js::len(&shown.messages[1].text) <= 700);
        let full = show_session(
            &path,
            &ShowOptions {
                full: true,
                ..around("overwrite")
            },
        )
        .unwrap();
        assert_eq!(full.messages[1].text, matching.trim_end());
    }

    #[test]
    fn show_places_leading_and_trailing_markers_around_one_window() {
        let dir = TempDir::new("show");
        let texts: Vec<String> = (0..10)
            .map(|i| {
                if i == 5 {
                    "message 5 target".into()
                } else {
                    format!("message {i}")
                }
            })
            .collect();
        let path = claude_transcript(&dir, &texts);
        let result = show_session(&path, &around("target")).unwrap();
        let texts: Vec<_> = result.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "[messages omitted]",
                "message 2",
                "message 3",
                "message 4",
                "message 5 target",
                "message 6",
                "message 7",
                "message 8",
                "[messages omitted]"
            ]
        );
    }

    #[test]
    fn show_places_an_internal_marker_between_disjoint_windows() {
        let dir = TempDir::new("show");
        let texts: Vec<String> = (0..13)
            .map(|i| {
                if i == 2 || i == 10 {
                    format!("message {i} target")
                } else {
                    format!("message {i}")
                }
            })
            .collect();
        let path = claude_transcript(&dir, &texts);
        let result = show_session(&path, &around("target")).unwrap();
        let texts: Vec<_> = result.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "message 0",
                "message 1",
                "message 2 target",
                "message 3",
                "message 4",
                "message 5",
                "[messages omitted]",
                "message 7",
                "message 8",
                "message 9",
                "message 10 target",
                "message 11",
                "message 12"
            ]
        );
    }
}
