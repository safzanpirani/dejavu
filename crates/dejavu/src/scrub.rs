//! In-place transcript redaction (`transcript-scrub.ts`).
//!
//! JSONL stores are rewritten through a temporary file in the same directory
//! that is renamed over the original, after a `.bak-<epoch>` copy is written.
//! OpenCode databases are snapshotted to `.bak-<epoch>` first, then updated in
//! one transaction.

use crate::js;
use crate::opencode::{
    OpenCodeLocator, open_opencode_database, opencode_schema, parse_opencode_locator,
};
use crate::reader::{fs_error, parse_json_line, read_text};
use crate::sources::{default_roots, source_from_locator};
use crate::types::TranscriptSource;
use crate::view::{EventBody, EventRef, TranscriptEvent, load_transcript_events, reorder_keys};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

pub use crate::DEFAULT_PLACEHOLDER;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubOptions {
    /// Event numbers from `dejavu transcript` whose content is replaced by the
    /// placeholder. Dropping a tool call also drops its result.
    pub drop: Vec<usize>,
    /// Case-insensitive literal fragments. Every line of every string field
    /// containing one is removed, in every record of the store.
    pub patterns: Vec<String>,
    pub placeholder: Option<String>,
    /// Report what would change without writing.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScrubResult {
    pub path: String,
    pub source: TranscriptSource,
    #[serde(rename = "dryRun")]
    pub dry_run: bool,
    pub backup: Option<String>,
    /// Event numbers that were redacted, including tool results pulled in by their call.
    #[serde(rename = "droppedEvents")]
    pub dropped_events: Vec<usize>,
    /// Lines removed from string fields by pattern matching.
    #[serde(rename = "patternLines")]
    pub pattern_lines: usize,
    /// Records (JSONL lines or database rows) rewritten.
    #[serde(rename = "changedRecords")]
    pub changed_records: usize,
}

/// Fields that carry conversational text or tool payloads. Structural fields
/// (type, id, role, names) are left alone.
const TEXT_KEYS: [&str; 11] = [
    "text",
    "thinking",
    "output",
    "stdout",
    "stderr",
    "error",
    "command",
    "lastPrompt",
    "title",
    "summary",
    "description",
];
const PAYLOAD_KEYS: [&str; 6] = [
    "input",
    "arguments",
    "action",
    "toolUseResult",
    "content",
    "metadata",
];

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// `scrubTranscript(locator, options)`.
pub fn scrub_transcript(locator: &str, options: &ScrubOptions) -> Result<ScrubResult, String> {
    scrub_transcript_at(locator, options, now_seconds())
}

/// [`scrub_transcript`] with the backup suffix's epoch seconds given.
pub fn scrub_transcript_at(
    locator: &str,
    options: &ScrubOptions,
    epoch_seconds: u64,
) -> Result<ScrubResult, String> {
    let mut drop = options.drop.clone();
    drop.sort_unstable();
    drop.dedup();
    let patterns: Vec<String> = options
        .patterns
        .iter()
        .map(|pattern| crate::view::js_trim(pattern))
        .filter(|pattern| !pattern.is_empty())
        .map(lower)
        .collect();
    if drop.is_empty() && patterns.is_empty() {
        return Err("scrub needs at least one --drop event number or --pattern".into());
    }
    let placeholder = options
        .placeholder
        .clone()
        .unwrap_or_else(|| DEFAULT_PLACEHOLDER.to_string());
    let source = source_from_locator(locator, default_roots())?;
    if source == TranscriptSource::Agy {
        return Err("scrub cannot redact agy conversations: agy keeps copies in transcript.jsonl, logs/chunks/, and the binary conversations/<id>.db, so redacting one file would leave the text in the others".into());
    }
    let events = load_transcript_events(locator, source)?.events;
    let targets = resolve_drops(&events, &drop)?;
    let context = ScrubContext {
        placeholder,
        patterns,
        targets,
    };
    if source == TranscriptSource::Opencode {
        scrub_opencode(locator, source, &context, options.dry_run, epoch_seconds)
    } else {
        scrub_jsonl(locator, source, &context, options.dry_run, epoch_seconds)
    }
}

struct ScrubContext {
    placeholder: String,
    patterns: Vec<String>,
    targets: Vec<TranscriptEvent>,
}

fn lower(text: &str) -> String {
    crate::paths::js_lower(text).into_owned()
}

fn resolve_drops(
    events: &[TranscriptEvent],
    drop: &[usize],
) -> Result<Vec<TranscriptEvent>, String> {
    let by_index: HashMap<usize, &TranscriptEvent> = events
        .iter()
        .filter_map(|event| event.index.map(|index| (index, event)))
        .collect();
    let mut chosen: BTreeMap<usize, &TranscriptEvent> = BTreeMap::new();
    for &index in drop {
        let Some(&event) = by_index.get(&index) else {
            return Err(format!(
                "no event #{index}; the transcript has events #0 to #{}",
                events.len() as i64 - 1
            ));
        };
        if event.reference.is_none() {
            return Err(format!(
                "event #{index} has no source reference and cannot be scrubbed"
            ));
        }
        chosen.insert(index, event);
        if let EventBody::ToolCall {
            call_id: Some(call_id),
            ..
        } = &event.body
            && !call_id.is_empty()
        {
            for candidate in events {
                if matches!(&candidate.body, EventBody::ToolResult { call_id: Some(id), .. } if id == call_id)
                    && candidate.reference.is_some()
                    && let Some(candidate_index) = candidate.index
                {
                    chosen.insert(candidate_index, candidate);
                }
            }
        }
    }
    Ok(chosen.into_values().cloned().collect())
}

// ---------------------------------------------------------------------------
// redaction primitives

/// `redactNode(node, placeholder)`: replaces every text-bearing field beneath
/// `node` with the placeholder while keeping ids, types, and names intact.
pub fn redact_node(node: &Value, placeholder: &str) -> Value {
    match node {
        Value::String(_) => Value::String(placeholder.to_string()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_node(item, placeholder))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let nested = matches!(value, Value::Object(_) | Value::Array(_));
                let redacted = if TEXT_KEYS.contains(&key.as_str()) {
                    redact_node(value, placeholder)
                } else if PAYLOAD_KEYS.contains(&key.as_str()) {
                    if value.is_string() || nested {
                        redact_node(value, placeholder)
                    } else {
                        value.clone()
                    }
                } else if nested {
                    redact_node(value, placeholder)
                } else {
                    value.clone()
                };
                out.insert(key.clone(), redacted);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `redactEntry(entry, event, source, placeholder)`: applies one event's
/// redaction to its parsed JSONL entry. Returns true when something changed.
pub fn redact_entry(
    entry: &mut Map<String, Value>,
    event: &TranscriptEvent,
    source: TranscriptSource,
    placeholder: &str,
) -> bool {
    let before = js::stringify(&*entry);
    let block = match &event.reference {
        Some(EventRef::Line { block, .. }) => *block,
        _ => None,
    };
    if source == TranscriptSource::Codex {
        if let Some(payload) = entry.get_mut("payload")
            && payload.is_object()
        {
            *payload = redact_node(payload, placeholder);
        }
    } else {
        if let Some(Value::Object(message)) = entry.get_mut("message")
            && let Some(content) = message.get_mut("content")
        {
            match (block, &mut *content) {
                (Some(block), Value::Array(items)) => {
                    if let Some(item) = items.get_mut(block) {
                        *item = redact_node(item, placeholder);
                    }
                }
                (_, Value::String(_)) => *content = Value::String(placeholder.to_string()),
                (_, other) => *other = redact_node(other, placeholder),
            }
        }
        if source == TranscriptSource::Claude
            && matches!(event.body, EventBody::ToolResult { .. })
            && let Some(result) = entry.get_mut("toolUseResult")
        {
            *result = redact_node(result, placeholder);
        }
    }
    js::stringify(&*entry) != before
}

/// `scrubPatterns(node, patterns, placeholder, counter)`: removes matching
/// lines from every string in the tree and renames object keys that contain a
/// pattern. `patterns` must be lowercase.
pub fn scrub_patterns(
    node: Value,
    patterns: &[String],
    placeholder: &str,
    lines: &mut usize,
) -> Value {
    match node {
        Value::String(text) => {
            let lowered = lower(&text);
            if !patterns
                .iter()
                .any(|pattern| lowered.contains(pattern.as_str()))
            {
                return Value::String(text);
            }
            let kept: Vec<&str> = text
                .split('\n')
                .filter(|line| {
                    let line = lower(line);
                    let hit = patterns
                        .iter()
                        .any(|pattern| line.contains(pattern.as_str()));
                    if hit {
                        *lines += 1;
                    }
                    !hit
                })
                .collect();
            let joined = kept.join("\n");
            if crate::view::js_trim(&joined).is_empty() {
                Value::String(placeholder.to_string())
            } else {
                Value::String(joined)
            }
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| scrub_patterns(item, patterns, placeholder, lines))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let mut name = key;
                let lowered = lower(&name);
                if patterns
                    .iter()
                    .any(|pattern| lowered.contains(pattern.as_str()))
                {
                    *lines += 1;
                    name = placeholder.to_string();
                    let mut suffix = 2;
                    while out.contains_key(&name) {
                        name = format!("{placeholder}-{suffix}");
                        suffix += 1;
                    }
                }
                let value = scrub_patterns(value, patterns, placeholder, lines);
                // Assigning an existing key keeps its position, as in JavaScript.
                out.insert(name, value);
            }
            Value::Object(out)
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// JSONL stores (Claude, Codex, Pi)

fn scrub_jsonl(
    locator: &str,
    source: TranscriptSource,
    context: &ScrubContext,
    dry_run: bool,
    epoch_seconds: u64,
) -> Result<ScrubResult, String> {
    let text = read_text(locator)?;
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    drop(text);
    let mut by_line: HashMap<usize, Vec<&TranscriptEvent>> = HashMap::new();
    for event in &context.targets {
        if let Some(EventRef::Line { line, .. }) = &event.reference {
            by_line.entry(*line).or_default().push(event);
        }
    }
    let mut pattern_lines = 0;
    let mut changed_records = 0;
    for (index, raw) in lines.iter_mut().enumerate() {
        if crate::view::js_trim(raw).is_empty() {
            continue;
        }
        let Some(Value::Object(mut entry)) = parse_json_line::<Value>(raw) else {
            continue;
        };
        let targets = by_line.get(&(index + 1));
        if targets.is_none() && context.patterns.is_empty() {
            continue;
        }
        // JavaScript objects list array-index keys first; compare and write in that order.
        let mut ordered = Value::Object(std::mem::take(&mut entry));
        reorder_keys(&mut ordered);
        let Value::Object(mut entry) = ordered else {
            continue;
        };
        let before = js::stringify(&entry);
        for event in targets.into_iter().flatten() {
            redact_entry(&mut entry, event, source, &context.placeholder);
        }
        let mut value = Value::Object(entry);
        if !context.patterns.is_empty() {
            value = scrub_patterns(
                value,
                &context.patterns,
                &context.placeholder,
                &mut pattern_lines,
            );
        }
        let after = js::stringify(&value);
        if after != before {
            changed_records += 1;
            *raw = after;
        }
    }
    let backup = format!("{locator}.bak-{epoch_seconds}");
    if !dry_run && changed_records > 0 {
        // Every rewritten line was produced by the serializer; any other line
        // that does not parse would make the result unreadable, so stop first.
        if lines.iter().any(|line| {
            !crate::view::js_trim(line).is_empty() && parse_json_line::<Value>(line).is_none()
        }) {
            return Err(format!(
                "{locator} has lines that are not valid JSON; refusing to rewrite it"
            ));
        }
        std::fs::copy(locator, &backup).map_err(|error| fs_error(&error, "copyfile", locator))?;
        write_replacing(locator, &lines.join("\n"))?;
    }
    Ok(ScrubResult {
        path: locator.to_string(),
        source,
        dry_run,
        backup: (!dry_run && changed_records > 0).then_some(backup),
        dropped_events: context
            .targets
            .iter()
            .filter_map(|event| event.index)
            .collect(),
        pattern_lines,
        changed_records,
    })
}

/// Writes `text` to a temporary file beside `path` (with the original's
/// permissions) and renames it over `path`.
fn write_replacing(path: &str, text: &str) -> Result<(), String> {
    use std::io::Write;
    let target = std::path::Path::new(path);
    let directory = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = directory.join(format!(".{name}.scrub-{}.tmp", std::process::id()));
    let temporary_text = temporary.to_string_lossy().into_owned();
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| fs_error(&error, "open", &temporary_text))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| fs_error(&error, "write", &temporary_text))?;
        if let Ok(metadata) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&temporary, metadata.permissions());
        }
        std::fs::rename(&temporary, path).map_err(|error| fs_error(&error, "rename", path))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

// ---------------------------------------------------------------------------
// OpenCode SQLite store

enum Table {
    Part,
    Message,
    SessionMessage,
}

impl Table {
    fn name(&self) -> &'static str {
        match self {
            Table::Part => "part",
            Table::Message => "message",
            Table::SessionMessage => "session_message",
        }
    }
}

fn rows_of(
    database: &Connection,
    table: &str,
    present: bool,
    session_id: &str,
) -> Result<Vec<(String, Option<String>)>, String> {
    if !present {
        return Ok(Vec::new());
    }
    let mut statement = database
        .prepare(&format!(
            "SELECT id, data FROM {table} WHERE session_id = ?1"
        ))
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([session_id], |row| {
            let text = |value: ValueRef<'_>| match value {
                ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                ValueRef::Integer(n) => Some(n.to_string()),
                ValueRef::Real(n) => Some(js::number_to_string(n)),
                _ => None,
            };
            Ok((
                text(row.get_ref(0)?).unwrap_or_else(|| "null".into()),
                text(row.get_ref(1)?),
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

fn parse_ordered(data: Option<&str>) -> Option<Value> {
    let mut value = parse_json_line::<Value>(data.unwrap_or("null"))?;
    reorder_keys(&mut value);
    Some(value)
}

fn scrub_opencode(
    locator: &str,
    source: TranscriptSource,
    context: &ScrubContext,
    dry_run: bool,
    epoch_seconds: u64,
) -> Result<ScrubResult, String> {
    let OpenCodeLocator {
        database_path,
        session_id,
    } = parse_opencode_locator(locator)?;
    let mut part_targets: HashSet<&str> = HashSet::new();
    // v2 session_message id -> dropped content or file positions; None marks the user text itself.
    let mut v2_targets: HashMap<&str, HashSet<Option<usize>>> = HashMap::new();
    for event in &context.targets {
        match &event.reference {
            Some(EventRef::Part { part_id, .. }) => {
                part_targets.insert(part_id);
            }
            Some(EventRef::SessionMessage {
                session_message_id,
                item,
            }) => {
                v2_targets
                    .entry(session_message_id)
                    .or_default()
                    .insert(*item);
            }
            _ => {}
        }
    }
    let placeholder = context.placeholder.as_str();
    let mut pattern_lines = 0;
    let mut updates: Vec<(Table, String, String)> = Vec::new();
    {
        let database = open_opencode_database(&database_path)?;
        // A hybrid store can hold legacy parts and v2 rows for one session, so both are scrubbed.
        let schema = opencode_schema(&database)?;
        let parts = rows_of(&database, "part", schema.legacy, &session_id)?;
        let messages = rows_of(&database, "message", schema.legacy, &session_id)?;
        let session_messages = rows_of(&database, "session_message", schema.v2, &session_id)?;
        for (id, data) in parts {
            let Some(Value::Object(mut data)) = parse_ordered(data.as_deref()) else {
                continue;
            };
            let before = js::stringify(&data);
            if part_targets.contains(id.as_str()) {
                if let Some(Value::Object(state)) = data.get_mut("state") {
                    for (key, value) in state.iter_mut() {
                        match key.as_str() {
                            "input" | "metadata" => *value = redact_node(value, placeholder),
                            "output" | "error" => *value = Value::String(placeholder.to_string()),
                            _ => {}
                        }
                    }
                }
                for key in ["text", "title"] {
                    if let Some(value @ Value::String(_)) = data.get_mut(key) {
                        *value = Value::String(placeholder.to_string());
                    }
                }
            }
            let mut value = Value::Object(data);
            if !context.patterns.is_empty() {
                value = scrub_patterns(value, &context.patterns, placeholder, &mut pattern_lines);
            }
            let after = js::stringify(&value);
            if after != before {
                updates.push((Table::Part, id, after));
            }
        }
        if !context.patterns.is_empty() {
            for (id, data) in messages {
                let Some(value) = parse_ordered(data.as_deref()) else {
                    continue;
                };
                let before = js::stringify(&value);
                let after = js::stringify(&scrub_patterns(
                    value,
                    &context.patterns,
                    placeholder,
                    &mut pattern_lines,
                ));
                if after != before {
                    updates.push((Table::Message, id, after));
                }
            }
        }
        for (id, data) in session_messages {
            let Some(Value::Object(mut data)) = parse_ordered(data.as_deref()) else {
                continue;
            };
            let before = js::stringify(&data);
            if let Some(items) = v2_targets.get(id.as_str()) {
                if items.contains(&None)
                    && let Some(value @ Value::String(_)) = data.get_mut("text")
                {
                    *value = Value::String(placeholder.to_string());
                }
                for key in ["content", "files"] {
                    if let Some(Value::Array(list)) = data.get_mut(key) {
                        for (position, item) in list.iter_mut().enumerate() {
                            if items.contains(&Some(position)) {
                                *item = redact_node(item, placeholder);
                            }
                        }
                    }
                }
            }
            let mut value = Value::Object(data);
            if !context.patterns.is_empty() {
                value = scrub_patterns(value, &context.patterns, placeholder, &mut pattern_lines);
            }
            let after = js::stringify(&value);
            if after != before {
                updates.push((Table::SessionMessage, id, after));
            }
        }
    }
    let changed_records = updates.len();
    let backup = format!("{database_path}.bak-{epoch_seconds}");
    if !dry_run && !updates.is_empty() {
        let mut database = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;
        database
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        // VACUUM INTO writes a consistent snapshot, including pages still in the WAL.
        if std::fs::metadata(&backup).is_ok() {
            std::fs::remove_file(&backup).map_err(|error| fs_error(&error, "unlink", &backup))?;
        }
        database
            .execute("VACUUM INTO ?1", [&backup])
            .map_err(|e| format!("could not back up {database_path}: {e}"))?;
        let transaction = database.transaction().map_err(|e| e.to_string())?;
        for (table, id, data) in &updates {
            transaction
                .execute(
                    &format!("UPDATE {} SET data = ?1 WHERE id = ?2", table.name()),
                    [data, id],
                )
                .map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())?;
    }
    Ok(ScrubResult {
        path: locator.to_string(),
        source,
        dry_run,
        backup: (!dry_run && changed_records > 0).then_some(backup),
        dropped_events: context
            .targets
            .iter()
            .filter_map(|event| event.index)
            .collect(),
        pattern_lines,
        changed_records,
    })
}

/// `parseDropList(values)`: event numbers from `--drop` values such as `4`, `7-9`, or `4,7-9`.
pub fn parse_drop_list(values: &[String]) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for value in values {
        for part in value.split(',') {
            let token = crate::view::js_trim(part);
            if token.is_empty() {
                continue;
            }
            let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
            let number = |text: &str| -> Result<usize, String> {
                text.parse::<usize>()
                    .map_err(|_| format!("--drop event number '{text}' is too large"))
            };
            if let Some((start, end)) = token.split_once('-')
                && digits(start)
                && digits(end)
            {
                let (start, end) = (number(start)?, number(end)?);
                if end < start {
                    return Err(format!("--drop range '{token}' runs backwards"));
                }
                if end - start > 10_000_000 {
                    return Err(format!("--drop range '{token}' is too large"));
                }
                out.extend(start..=end);
                continue;
            }
            if !digits(token) {
                return Err(format!(
                    "--drop expects event numbers like 4, 7-9, or 4,7-9 (got '{token}')"
                ));
            }
            out.push(number(token)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::opencode_locator;
    use crate::view::tests::{TempDir, droid_file, opencode_legacy_fixture};
    use serde_json::json;

    fn claude_file(dir: &TempDir) -> String {
        dir.write_jsonl(
            ".claude/projects/-work-demo/s.jsonl",
            &[
                json!({ "uuid": "u1", "parentUuid": null, "type": "user", "message": { "role": "user", "content": "the host bohrium is down\nplease check" } }),
                json!({ "uuid": "a1", "parentUuid": "u1", "type": "assistant", "message": { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "bohrium again" },
                    { "type": "text", "text": "checking" },
                    { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ssh bohrium uptime" } },
                ] } }),
                json!({ "uuid": "r1", "parentUuid": "a1", "type": "user", "toolUseResult": { "stdout": "bohrium up 3 days" }, "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "bohrium up 3 days" }] } }),
                json!({ "uuid": "a2", "parentUuid": "r1", "type": "assistant", "message": { "role": "assistant", "content": [{ "type": "text", "text": "all good" }] } }),
                json!({ "uuid": "dead", "parentUuid": "u1", "type": "assistant", "message": { "role": "assistant", "content": [{ "type": "text", "text": "dead branch mentions bohrium" }] } }),
                json!({ "type": "last-prompt", "leafUuid": "a2", "lastPrompt": "the host bohrium is down" }),
            ],
        )
    }

    fn read_lines(path: &str) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .split('\n')
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn options(drop: &[usize], patterns: &[&str], dry_run: bool) -> ScrubOptions {
        ScrubOptions {
            drop: drop.to_vec(),
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
            placeholder: None,
            dry_run,
        }
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let dir = TempDir::new("scrub");
        let path = claude_file(&dir);
        let before = std::fs::read_to_string(&path).unwrap();
        let result = scrub_transcript_at(&path, &options(&[3], &["bohrium"], true), 1).unwrap();
        assert!(result.dry_run);
        assert_eq!(result.backup, None);
        assert_eq!(result.dropped_events, [3, 4]);
        assert_eq!(result.changed_records, 5);
        assert_eq!(result.pattern_lines, 4);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn dropping_a_tool_call_redacts_the_call_its_result_and_the_tool_use_result_copy() {
        let dir = TempDir::new("scrub");
        let path = claude_file(&dir);
        let result = scrub_transcript_at(&path, &options(&[3], &[], false), 1).unwrap();
        assert_eq!(result.dropped_events, [3, 4]);
        assert_eq!(
            result.backup.as_deref(),
            Some(format!("{path}.bak-1").as_str())
        );
        assert!(std::fs::metadata(result.backup.unwrap()).is_ok());
        let lines = read_lines(&path);
        assert_eq!(
            lines[1]["message"]["content"][2],
            json!({ "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "[redacted]" } })
        );
        assert_eq!(
            lines[1]["message"]["content"][1],
            json!({ "type": "text", "text": "checking" })
        );
        assert_eq!(
            lines[2]["message"]["content"][0],
            json!({ "type": "tool_result", "tool_use_id": "t1", "content": "[redacted]" })
        );
        assert_eq!(lines[2]["toolUseResult"], json!({ "stdout": "[redacted]" }));
        let events = load_transcript_events(&path, TranscriptSource::Claude)
            .unwrap()
            .events;
        assert!(
            matches!(&events[3].body, EventBody::ToolCall { input, .. } if *input == json!({ "command": "[redacted]" }))
        );
        assert!(
            matches!(&events[4].body, EventBody::ToolResult { output, .. } if output == "[redacted]")
        );
        let leftovers: Vec<_> = std::fs::read_dir(std::path::Path::new(&path).parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn dropping_a_user_turn_keeps_the_parent_chain() {
        let dir = TempDir::new("scrub");
        let path = claude_file(&dir);
        let mut request = options(&[0], &[], false);
        request.placeholder = Some("please continue".into());
        scrub_transcript_at(&path, &request, 1).unwrap();
        assert_eq!(
            read_lines(&path)[0],
            json!({ "uuid": "u1", "parentUuid": null, "type": "user", "message": { "role": "user", "content": "please continue" } })
        );
        let events = load_transcript_events(&path, TranscriptSource::Claude)
            .unwrap()
            .events;
        assert_eq!(events.len(), 6);
    }

    #[test]
    fn patterns_remove_matching_lines_from_every_record() {
        let dir = TempDir::new("scrub");
        let path = claude_file(&dir);
        let result = scrub_transcript_at(&path, &options(&[], &["BOHRIUM"], false), 1).unwrap();
        assert_eq!(result.pattern_lines, 7);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.to_lowercase().contains("bohrium"));
        let lines = read_lines(&path);
        assert_eq!(lines[0]["message"]["content"], "please check");
        assert_eq!(
            lines[1]["message"]["content"][0],
            json!({ "type": "thinking", "thinking": "[redacted]" })
        );
        assert_eq!(
            lines[1]["message"]["content"][2]["input"]["command"],
            "[redacted]"
        );
        assert_eq!(lines[2]["toolUseResult"]["stdout"], "[redacted]");
        assert_eq!(lines[4]["message"]["content"][0]["text"], "[redacted]");
        assert_eq!(lines[5]["lastPrompt"], "[redacted]");
    }

    #[test]
    fn rejects_unknown_event_numbers_and_empty_requests() {
        let dir = TempDir::new("scrub");
        let path = claude_file(&dir);
        let error = scrub_transcript_at(&path, &options(&[99], &[], false), 1).unwrap_err();
        assert!(error.contains("no event #99"), "{error}");
        let error = scrub_transcript_at(&path, &options(&[], &[], false), 1).unwrap_err();
        assert!(error.contains("at least one"));
    }

    #[test]
    fn droid_redacts_a_tool_call_and_its_result_and_keeps_the_tree() {
        let dir = TempDir::new("scrub");
        let path = droid_file(&dir);
        let before = read_lines(&path);
        let result = scrub_transcript_at(&path, &options(&[3], &["make"], false), 1).unwrap();
        assert_eq!(result.source, TranscriptSource::Droid);
        assert_eq!(result.dropped_events, [3, 4]);
        assert_eq!(
            result.backup.as_deref(),
            Some(format!("{path}.bak-1").as_str())
        );
        assert_eq!(
            std::fs::read_to_string(result.backup.unwrap()).unwrap(),
            before
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        );
        let after = read_lines(&path);
        assert_eq!(after.len(), before.len());
        assert_eq!(
            after[4]["message"]["content"][2],
            json!({ "type": "tool_use", "id": "t1", "name": "Execute", "input": { "command": "[redacted]" } })
        );
        assert_eq!(
            after[5]["message"]["content"][0],
            json!({ "type": "tool_result", "tool_use_id": "t1", "content": [{ "type": "text", "text": "[redacted]" }], "is_error": true })
        );
        // Ids, parent links, and the session_start row are untouched.
        for (old, new) in before.iter().zip(&after) {
            assert_eq!(old["id"], new["id"]);
            assert_eq!(old["parentId"], new["parentId"]);
        }
        assert_eq!(after[0], before[0]);
        let events = load_transcript_events(&path, TranscriptSource::Droid)
            .unwrap()
            .events;
        assert_eq!(events.len(), 6);
        let leftovers: Vec<_> = std::fs::read_dir(std::path::Path::new(&path).parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn codex_redacts_function_calls_with_their_outputs_only() {
        let dir = TempDir::new("scrub");
        let lines = [
            json!({ "type": "session_meta", "payload": { "cwd": "/work" } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "run it" }] } }),
            json!({ "type": "response_item", "payload": { "type": "function_call", "name": "exec", "arguments": "{\"cmd\":\"cat secret\"}", "call_id": "c1" } }),
            json!({ "type": "response_item", "payload": { "type": "function_call_output", "call_id": "c1", "output": "TOKEN=abc" } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "done" }] } }),
        ];
        let path = dir.write_jsonl(".codex/sessions/2026/09/02/rollout.jsonl", &lines);
        let result = scrub_transcript_at(&path, &options(&[1], &[], false), 1).unwrap();
        assert_eq!(result.dropped_events, [1, 2]);
        let after = read_lines(&path);
        assert_eq!(
            after[2]["payload"],
            json!({ "type": "function_call", "name": "exec", "arguments": "[redacted]", "call_id": "c1" })
        );
        assert_eq!(
            after[3]["payload"],
            json!({ "type": "function_call_output", "call_id": "c1", "output": "[redacted]" })
        );
        assert_eq!(after[1], lines[1]);
        assert_eq!(after[4], lines[4]);
    }

    fn table(path: &str, name: &str) -> HashMap<String, Value> {
        let database = Connection::open(path).unwrap();
        let mut statement = database
            .prepare(&format!("SELECT id, data FROM {name}"))
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    serde_json::from_str(&row.get::<_, String>(1)?).unwrap(),
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn opencode_updates_targeted_parts_and_pattern_matches_after_a_backup() {
        let dir = TempDir::new("scrub-oc");
        let path = format!("{}/oc.db", dir.0);
        opencode_legacy_fixture(
            &path,
            &[
                (
                    "m1",
                    "ses",
                    1,
                    json!({ "role": "user", "summary": { "title": "bohrium outage" } }),
                ),
                ("m2", "ses", 2, json!({ "role": "assistant" })),
            ],
            &[
                (
                    "p1",
                    "m1",
                    1,
                    json!({ "type": "text", "text": "check bohrium\nand the rest" }),
                ),
                (
                    "p2",
                    "m2",
                    2,
                    json!({ "type": "tool", "tool": "bash", "callID": "b1", "state": { "status": "completed", "input": { "command": "ssh bohrium" }, "output": "up", "title": "ssh bohrium" } }),
                ),
                ("p3", "m2", 3, json!({ "type": "text", "text": "all good" })),
            ],
        );
        let locator = opencode_locator(&path, "ses");
        let result = scrub_transcript_at(&locator, &options(&[1], &["bohrium"], false), 7).unwrap();
        assert_eq!(result.source, TranscriptSource::Opencode);
        assert_eq!(result.dropped_events, [1, 2]);
        assert_eq!(
            result.backup.as_deref(),
            Some(format!("{path}.bak-7").as_str())
        );
        assert_eq!(result.changed_records, 3);
        let backup = table(&format!("{path}.bak-7"), "part");
        assert_eq!(backup["p1"]["text"], "check bohrium\nand the rest");
        let parts = table(&path, "part");
        let messages = table(&path, "message");
        assert_eq!(parts["p1"]["text"], "and the rest");
        assert_eq!(
            parts["p2"]["state"],
            json!({ "status": "completed", "input": { "command": "[redacted]" }, "output": "[redacted]", "title": "[redacted]" })
        );
        assert_eq!(parts["p3"]["text"], "all good");
        assert_eq!(messages["m1"]["summary"]["title"], "[redacted]");
        let events = load_transcript_events(&locator, TranscriptSource::Opencode)
            .unwrap()
            .events;
        let kinds: Vec<_> = events.iter().map(TranscriptEvent::kind).collect();
        assert_eq!(kinds, ["user", "tool_call", "tool_result", "assistant"]);
    }

    #[test]
    fn opencode_v2_redacts_targeted_items_and_pattern_lines() {
        let dir = TempDir::new("scrub-v2");
        let path = format!("{}/v2.db", dir.0);
        {
            let database = Connection::open(&path).unwrap();
            database.execute_batch("CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT, time_updated INTEGER NOT NULL);
                CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
                INSERT INTO session_v2 VALUES ('ses', '/work/next', 'Next', 1);").unwrap();
            let rows = [
                (
                    "u1",
                    "user",
                    json!({ "text": "check bohrium\nand the rest" }),
                ),
                (
                    "a1",
                    "assistant",
                    json!({ "content": [
                    { "type": "tool", "id": "b1", "name": "bash", "state": { "status": "completed", "input": { "command": "ssh host" }, "content": [{ "type": "text", "text": "up" }] } },
                    { "type": "text", "text": "all good" },
                ] }),
                ),
                ("u2", "user", json!({ "text": "secret question" })),
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
        let locator = opencode_locator(&path, "ses");
        let result =
            scrub_transcript_at(&locator, &options(&[1, 4], &["bohrium"], false), 7).unwrap();
        assert_eq!(result.dropped_events, [1, 2, 4]);
        assert_eq!(result.changed_records, 3);
        assert_eq!(
            result.backup.as_deref(),
            Some(format!("{path}.bak-7").as_str())
        );
        let rows = table(&path, "session_message");
        assert_eq!(rows["u1"]["text"], "and the rest");
        assert_eq!(
            rows["a1"]["content"],
            json!([
                { "type": "tool", "id": "b1", "name": "bash", "state": { "status": "completed", "input": { "command": "[redacted]" }, "content": [{ "type": "text", "text": "[redacted]" }] } },
                { "type": "text", "text": "all good" },
            ])
        );
        assert_eq!(rows["u2"]["text"], "[redacted]");
    }

    #[test]
    fn rewritten_records_keep_javascript_float_digits() {
        // Needs serde_json's float_roundtrip: the fast parser is one ULP off for these.
        let text = "[0.0027873299999999998,0.00046433399999999995,0.0009192660000000001]";
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(js::stringify(&value), text);
    }

    #[test]
    fn parse_drop_list_accepts_numbers_ranges_and_comma_lists() {
        let values = |list: &[&str]| list.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_drop_list(&values(&["4", "7-9", "1,2"])).unwrap(),
            [4, 7, 8, 9, 1, 2]
        );
        assert!(
            parse_drop_list(&values(&["9-7"]))
                .unwrap_err()
                .contains("backwards")
        );
        assert!(
            parse_drop_list(&values(&["x"]))
                .unwrap_err()
                .contains("event numbers")
        );
    }

    #[test]
    fn redact_node_keeps_structure_and_ids() {
        assert_eq!(
            redact_node(
                &json!({ "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls", "nested": { "description": "x" } } }),
                "[r]"
            ),
            json!({ "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "[r]", "nested": { "description": "[r]" } } })
        );
    }

    #[test]
    fn scrub_patterns_drops_matching_lines_and_renames_keys() {
        let mut lines = 0;
        let patterns = vec!["drop".to_string()];
        assert_eq!(
            scrub_patterns(
                json!({ "a": "keep\nDROP me\nkeep too", "b": ["drop"], "c": 3 }),
                &patterns,
                "[r]",
                &mut lines
            ),
            json!({ "a": "keep\nkeep too", "b": ["[r]"], "c": 3 })
        );
        assert_eq!(lines, 2);
        let mut lines = 0;
        let patterns = vec!["secret".to_string()];
        let out = scrub_patterns(
            json!({ "backups": { "/Users/secret/a.md": 1, "/Users/secret/b.md": 2, "/other": 3 } }),
            &patterns,
            "[r]",
            &mut lines,
        );
        assert_eq!(
            js::stringify(&out),
            r#"{"backups":{"[r]":1,"[r]-2":2,"/other":3}}"#
        );
        assert_eq!(lines, 2);
    }
}
