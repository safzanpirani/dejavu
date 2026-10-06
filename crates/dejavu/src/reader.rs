//! Session loading (`session-reader.ts`): the visible user/assistant messages of
//! a transcript, following the active branch of Pi, Claude, and Droid trees.
//!
//! Hot paths (`extract_visible_message`, `load_recall_messages`) deserialize
//! into small lenient structs that skip unused fields (tool output, Claude's
//! `toolUseResult`) without building a `serde_json::Value`. Lenient means a field
//! of an unexpected JSON type reads as absent instead of failing the line, which
//! is what property access on parsed JavaScript objects did.

use crate::js;
use crate::opencode::load_opencode_messages;
use crate::sources::{default_roots, source_from_locator};
use crate::types::{RecallBlock, RecallMessage, TranscriptSource};
use serde::de::{
    DeserializeOwned, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// File and line reading
// ---------------------------------------------------------------------------

/// `Bun.file(path).text()`: the file as UTF-8 (lossy), with a leading BOM
/// removed. Errors read like Bun's (`ENOENT: no such file or directory, open '<path>'`).
pub fn read_text(path: &str) -> Result<String, String> {
    if crate::virtual_store::is_virtual_locator(path) {
        return crate::virtual_store::render(path);
    }
    let bytes = std::fs::read(path).map_err(|error| fs_error(&error, "open", path))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    };
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

/// Formats an I/O error the way Bun and Node do: `CODE: description, syscall '<path>'`.
pub fn fs_error(error: &std::io::Error, syscall: &str, path: &str) -> String {
    use std::io::ErrorKind;
    let (code, description) = match error.kind() {
        ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        ErrorKind::PermissionDenied if error.raw_os_error() == Some(1) => {
            ("EPERM", "operation not permitted")
        }
        // Windows refuses to open a directory with ERROR_ACCESS_DENIED, not EISDIR.
        ErrorKind::PermissionDenied
            if cfg!(windows) && syscall == "open" && std::path::Path::new(path).is_dir() =>
        {
            return "Directories cannot be read like files".to_string();
        }
        ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        ErrorKind::NotADirectory => ("ENOTDIR", "not a directory"),
        ErrorKind::IsADirectory if syscall == "open" => {
            return "Directories cannot be read like files".to_string();
        }
        ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        _ => return format!("{error}, {syscall} '{path}'"),
    };
    format!("{code}: {description}, {syscall} '{path}'")
}

/// `JSON.parse(line)` into `T`, or `None` when the line is not valid JSON.
/// JavaScript accepts lone UTF-16 surrogate escapes that Rust strings cannot
/// hold; such a line is retried with each lone surrogate read as U+FFFD.
pub fn parse_json_line<T: DeserializeOwned>(line: &str) -> Option<T> {
    match serde_json::from_str(line) {
        Ok(value) => Some(value),
        Err(_) if line.contains("\\u") || line.contains("\\U") => {
            let repaired = replace_lone_surrogates(line)?;
            serde_json::from_str(&repaired).ok()
        }
        Err(_) => None,
    }
}

fn hex4(bytes: &[u8], at: usize) -> Option<u16> {
    let digits = std::str::from_utf8(bytes.get(at..at + 4)?).ok()?;
    u16::from_str_radix(digits, 16).ok()
}

/// Rewrites lone `\uD800`-`\uDFFF` escapes as `�`; `None` when there are none.
fn replace_lone_surrogates(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) != Some(&b'u') {
            i += 2;
            continue;
        }
        let Some(unit) = hex4(bytes, i + 2) else {
            i += 2;
            continue;
        };
        if (0xD800..0xDC00).contains(&unit)
            && bytes.get(i + 6) == Some(&b'\\')
            && bytes.get(i + 7) == Some(&b'u')
            && hex4(bytes, i + 8).is_some_and(|low| (0xDC00..0xE000).contains(&low))
        {
            i += 12;
        } else if (0xD800..0xE000).contains(&unit) {
            out.push_str(&line[copied..i]);
            out.push_str("\\ufffd");
            i += 6;
            copied = i;
            changed = true;
        } else {
            i += 6;
        }
    }
    changed.then(|| {
        out.push_str(&line[copied..]);
        out
    })
}

// ---------------------------------------------------------------------------
// Lenient deserialization
// ---------------------------------------------------------------------------

/// A JSON value reduced to what JavaScript truthiness and `Map` keys need.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum JsVal {
    #[default]
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// An object or array: truthy, never equal to another parsed value.
    Other,
}

struct JsValVisitor;

impl<'de> Visitor<'de> for JsValVisitor {
    type Value = JsVal;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<JsVal, E> {
        Ok(JsVal::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<JsVal, E> {
        Ok(JsVal::Num(v as f64))
    }
    fn visit_u64<E>(self, v: u64) -> Result<JsVal, E> {
        Ok(JsVal::Num(v as f64))
    }
    fn visit_f64<E>(self, v: f64) -> Result<JsVal, E> {
        Ok(JsVal::Num(v))
    }
    fn visit_str<E>(self, v: &str) -> Result<JsVal, E> {
        Ok(JsVal::Str(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<JsVal, E> {
        Ok(JsVal::Str(v))
    }
    fn visit_unit<E>(self) -> Result<JsVal, E> {
        Ok(JsVal::Null)
    }
    fn visit_none<E>(self) -> Result<JsVal, E> {
        Ok(JsVal::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JsVal, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(JsVal::Other)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsVal, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(JsVal::Other)
    }
}

impl<'de> Deserialize<'de> for JsVal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<JsVal, D::Error> {
        d.deserialize_any(JsValVisitor)
    }
}

/// `String(value)` for a block `type`: strings as is, arrays joined with commas.
/// Numbers and other values only need to differ from the block type names.
#[derive(Debug, Default)]
struct JsString(String);

struct JsStringVisitor {
    in_array: bool,
}

impl<'de> Visitor<'de> for JsStringVisitor {
    type Value = JsString;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<JsString, E> {
        Ok(JsString(v.to_string()))
    }
    fn visit_i64<E>(self, v: i64) -> Result<JsString, E> {
        Ok(JsString(v.to_string()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<JsString, E> {
        Ok(JsString(v.to_string()))
    }
    fn visit_f64<E>(self, v: f64) -> Result<JsString, E> {
        Ok(JsString(js::number_to_string(v)))
    }
    fn visit_str<E>(self, v: &str) -> Result<JsString, E> {
        Ok(JsString(v.to_string()))
    }
    fn visit_unit<E>(self) -> Result<JsString, E> {
        Ok(JsString(if self.in_array {
            String::new()
        } else {
            "null".into()
        }))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JsString, A::Error> {
        let mut parts = Vec::new();
        while let Some(part) = seq.next_element_seed(JsStringSeed { in_array: true })? {
            parts.push(part.0);
        }
        Ok(JsString(parts.join(",")))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsString, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(JsString("[object Object]".into()))
    }
}

struct JsStringSeed {
    in_array: bool,
}

impl<'de> DeserializeSeed<'de> for JsStringSeed {
    type Value = JsString;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<JsString, D::Error> {
        d.deserialize_any(JsStringVisitor {
            in_array: self.in_array,
        })
    }
}

impl<'de> Deserialize<'de> for JsString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<JsString, D::Error> {
        d.deserialize_any(JsStringVisitor { in_array: false })
    }
}

/// A struct read from a JSON object by field index. Any other JSON type reads
/// as `Default`, and a repeated key overwrites the earlier value (last wins).
pub(crate) trait LenientFields: Default {
    const NAMES: &'static [&'static str];
    fn set<'de, A: MapAccess<'de>>(&mut self, field: usize, map: &mut A) -> Result<(), A::Error>;
}

struct FieldSeed(&'static [&'static str]);

impl<'de> DeserializeSeed<'de> for FieldSeed {
    type Value = Option<usize>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Option<usize>, D::Error> {
        d.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for FieldSeed {
    type Value = Option<usize>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a field name")
    }
    fn visit_str<E>(self, v: &str) -> Result<Option<usize>, E> {
        Ok(self.0.iter().position(|name| *name == v))
    }
}

struct LenientVisitor<T>(std::marker::PhantomData<T>);

impl<'de, T: LenientFields> Visitor<'de> for LenientVisitor<T> {
    type Value = T;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
        let mut out = T::default();
        while let Some(field) = map.next_key_seed(FieldSeed(T::NAMES))? {
            match field {
                Some(index) => out.set(index, &mut map)?,
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(out)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<T, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(T::default())
    }
    fn visit_bool<E>(self, _: bool) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_i64<E>(self, _: i64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_u64<E>(self, _: u64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_f64<E>(self, _: f64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_str<E>(self, _: &str) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_unit<E>(self) -> Result<T, E> {
        Ok(T::default())
    }
}

/// Deserializes a [`LenientFields`] struct from any JSON value.
pub(crate) fn deserialize_lenient<'de, T: LenientFields, D: Deserializer<'de>>(
    d: D,
) -> Result<T, D::Error> {
    d.deserialize_any(LenientVisitor(std::marker::PhantomData))
}

/// Reads `T` when the JSON value is a string; any other value reads as `None`.
#[derive(Debug, Default)]
pub(crate) struct JsStr(pub Option<String>);

impl<'de> Deserialize<'de> for JsStr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<JsStr, D::Error> {
        Ok(JsStr(match JsVal::deserialize(d)? {
            JsVal::Str(value) => Some(value),
            _ => None,
        }))
    }
}

// ---------------------------------------------------------------------------
// Message envelopes
// ---------------------------------------------------------------------------

/// A content block. `FULL` keeps tool-call names and arguments; without it,
/// only text is read (visible-message extraction needs nothing else).
#[derive(Default)]
struct Block<const FULL: bool> {
    kind: Option<JsString>,
    text: Option<JsStr>,
    name: Option<JsStr>,
    arguments: Option<Value>,
    input: Option<Value>,
}

impl<const FULL: bool> LenientFields for Block<FULL> {
    const NAMES: &'static [&'static str] = &["type", "text", "name", "arguments", "input"];
    fn set<'de, A: MapAccess<'de>>(&mut self, field: usize, map: &mut A) -> Result<(), A::Error> {
        match field {
            0 => self.kind = Some(map.next_value()?),
            1 => self.text = Some(map.next_value()?),
            _ if !FULL => {
                map.next_value::<IgnoredAny>()?;
            }
            2 => self.name = Some(map.next_value()?),
            3 => self.arguments = Some(map.next_value()?),
            _ => self.input = Some(map.next_value()?),
        }
        Ok(())
    }
}

impl<'de, const FULL: bool> Deserialize<'de> for Block<FULL> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_lenient(d)
    }
}

impl<const FULL: bool> Block<FULL> {
    /// `normalizeContent`'s per-block rule.
    fn into_recall(self) -> Option<RecallBlock> {
        let kind = self
            .kind
            .as_ref()
            .map_or("undefined", |kind| kind.0.as_str());
        match kind {
            "text" | "input_text" | "output_text" => self
                .text
                .and_then(|text| text.0)
                .map(|text| RecallBlock::Text { text }),
            "toolCall" | "tool_use" => Some(RecallBlock::ToolCall {
                name: self
                    .name
                    .and_then(|name| name.0)
                    .unwrap_or_else(|| "unknown".into()),
                arguments: self
                    .arguments
                    .filter(|value| !value.is_null())
                    .or(self.input.filter(|value| !value.is_null()))
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
            }),
            "image" | "input_image" => Some(RecallBlock::Image),
            _ => None,
        }
    }
}

/// `normalizeContent(content)`: a string is one text block; an array keeps its
/// text, tool-call, and image blocks; anything else is empty.
#[derive(Default)]
struct Content<const FULL: bool>(Vec<RecallBlock>);

struct ContentVisitor<const FULL: bool>;

impl<'de, const FULL: bool> Visitor<'de> for ContentVisitor<FULL> {
    type Value = Content<FULL>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("message content")
    }
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Content(vec![RecallBlock::Text {
            text: v.to_string(),
        }]))
    }
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(Content(vec![RecallBlock::Text { text: v }]))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut blocks = Vec::new();
        while let Some(block) = seq.next_element::<Block<FULL>>()? {
            blocks.extend(block.into_recall());
        }
        Ok(Content(blocks))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Content(Vec::new()))
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Content(Vec::new()))
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Content(Vec::new()))
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Content(Vec::new()))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Content(Vec::new()))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Content(Vec::new()))
    }
}

impl<'de, const FULL: bool> Deserialize<'de> for Content<FULL> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ContentVisitor::<FULL>)
    }
}

/// A Claude/Pi `message` or a Codex `payload`.
#[derive(Default)]
struct Envelope<const FULL: bool> {
    kind: JsStr,
    role: JsStr,
    content: Content<FULL>,
    /// Droid's `visibility`: `llm_only` marks text the harness sent to the model only.
    visibility: JsStr,
    /// Droid's `hookEventName`: set on rows that record a hook run, not a turn.
    hook_event: JsStr,
    /// A Codex `compacted` payload's plaintext summary (`message`).
    summary: JsStr,
}

impl<const FULL: bool> LenientFields for Envelope<FULL> {
    const NAMES: &'static [&'static str] = &[
        "type",
        "role",
        "content",
        "visibility",
        "hookEventName",
        "message",
    ];
    fn set<'de, A: MapAccess<'de>>(&mut self, field: usize, map: &mut A) -> Result<(), A::Error> {
        match field {
            0 => self.kind = map.next_value()?,
            1 => self.role = map.next_value()?,
            2 => self.content = map.next_value()?,
            3 => self.visibility = map.next_value()?,
            4 => self.hook_event = map.next_value()?,
            _ => self.summary = map.next_value()?,
        }
        Ok(())
    }
}

impl<'de, const FULL: bool> Deserialize<'de> for Envelope<FULL> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_lenient(d)
    }
}

impl<const FULL: bool> Envelope<FULL> {
    /// [`Envelope::into_recall`] after dropping Droid's injected user text blocks.
    fn into_recall_from(
        mut self,
        source: TranscriptSource,
    ) -> Option<(&'static str, Vec<RecallBlock>)> {
        if source == TranscriptSource::Droid && is_droid_model_only(self.visibility.0.as_deref()) {
            return None;
        }
        if source == TranscriptSource::Droid && self.role.0.as_deref() == Some("user") {
            self.content.0.retain_mut(|block| match block {
                RecallBlock::Text { text } => match droid_user_text(text).map(str::len) {
                    Some(len) => {
                        text.truncate(len);
                        true
                    }
                    None => false,
                },
                _ => true,
            });
        }
        self.into_recall()
    }

    /// `normalizeEnvelope`: a user or assistant message with at least one kept block.
    fn into_recall(self) -> Option<(&'static str, Vec<RecallBlock>)> {
        let role = match self.role.0.as_deref() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => return None,
        };
        (!self.content.0.is_empty()).then_some((role, self.content.0))
    }
}

/// One JSONL row with the fields session loading reads.
#[derive(Default)]
struct Row<const FULL: bool> {
    kind: JsStr,
    timestamp: JsStr,
    cwd: JsStr,
    links: [JsVal; 6],
    message: Option<Envelope<FULL>>,
    payload: Option<Envelope<FULL>>,
    /// Droid `compaction_state` fields.
    summary_kind: JsStr,
    summary_text: JsStr,
    removed_count: JsVal,
}

const LINK_ID: usize = 0;
const LINK_PARENT_ID: usize = 1;
const LINK_UUID: usize = 2;
const LINK_PARENT_UUID: usize = 3;
const LINK_LOGICAL_PARENT_UUID: usize = 4;
const LINK_LEAF_UUID: usize = 5;
const LINK_NAMES: [&str; 6] = [
    "id",
    "parentId",
    "uuid",
    "parentUuid",
    "logicalParentUuid",
    "leafUuid",
];

impl<const FULL: bool> LenientFields for Row<FULL> {
    const NAMES: &'static [&'static str] = &[
        "type",
        "timestamp",
        "cwd",
        "message",
        "payload",
        "summaryKind",
        "summaryText",
        "removedCount",
        "id",
        "parentId",
        "uuid",
        "parentUuid",
        "logicalParentUuid",
        "leafUuid",
    ];
    fn set<'de, A: MapAccess<'de>>(&mut self, field: usize, map: &mut A) -> Result<(), A::Error> {
        match field {
            0 => self.kind = map.next_value()?,
            1 => self.timestamp = map.next_value()?,
            2 => self.cwd = map.next_value()?,
            3 => self.message = Some(map.next_value()?),
            4 => self.payload = Some(map.next_value()?),
            5 => self.summary_kind = map.next_value()?,
            6 => self.summary_text = map.next_value()?,
            7 => self.removed_count = map.next_value()?,
            _ if !FULL => {
                map.next_value::<IgnoredAny>()?;
            }
            link => self.links[link - 8] = map.next_value()?,
        }
        Ok(())
    }
}

impl<'de, const FULL: bool> Deserialize<'de> for Row<FULL> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_lenient(d)
    }
}

/// How a compaction summary reads as a user message. `find` treats the prefix
/// as injected, as it treats Claude's "This session is being continued" summary.
pub const COMPACTION_SUMMARY_PREFIX: &str = "[Compaction summary";

/// The user text for a compaction summary, or `None` when the summary is blank.
/// `removed` is how many earlier messages the summary replaced, when known.
pub fn compaction_summary_text(summary: &str, removed: Option<f64>) -> Option<String> {
    if summary.trim_matches(crate::query::js_space).is_empty() {
        return None;
    }
    let header = match removed {
        Some(count) if count >= 1.0 => {
            format!("{COMPACTION_SUMMARY_PREFIX} of {count} earlier messages]")
        }
        _ => format!("{COMPACTION_SUMMARY_PREFIX}]"),
    };
    Some(format!("{header}\n\n{summary}"))
}

impl<const FULL: bool> Row<FULL> {
    /// The summary a compaction row carries in plain text: a Droid
    /// `compaction_state` of kind `llm_summary`, or a Codex `compacted` row with
    /// a `message`. Codex usually encrypts its summary and leaves `message` empty;
    /// Droid's `provider_switch_serialization` kind only re-serializes the turns.
    fn compaction_summary(&self, source: TranscriptSource) -> Option<String> {
        match (source, self.kind.0.as_deref()) {
            (TranscriptSource::Droid, Some("compaction_state"))
                if self.summary_kind.0.as_deref() == Some("llm_summary") =>
            {
                let removed = match self.removed_count {
                    JsVal::Num(count) => Some(count),
                    _ => None,
                };
                compaction_summary_text(self.summary_text.0.as_deref()?, removed)
            }
            (TranscriptSource::Codex, Some("compacted")) => {
                compaction_summary_text(self.payload.as_ref()?.summary.0.as_deref()?, None)
            }
            _ => None,
        }
    }
}

/// Droid marks harness messages the user never saw (continuation prompts,
/// interruption notices, older reminders) with `visibility: "llm_only"`.
pub fn is_droid_model_only(visibility: Option<&str>) -> bool {
    visibility == Some("llm_only")
}

/// Droid writes harness context (tool catalogs, skill lists, system information)
/// into the conversation as user text blocks that begin with `<system-reminder>`,
/// and appends an activated skill to the user's own text as a line-leading
/// `<system-notification>` block. Neither is part of what the user said, so
/// recall, search, and views keep only the text before them. `None` means
/// nothing the user wrote is left.
pub fn droid_user_text(text: &str) -> Option<&str> {
    const NOTIFICATION: &str = "<system-notification>";
    let body = text.trim_start_matches(crate::query::js_space);
    if body.starts_with("<system-reminder>") || body.starts_with(NOTIFICATION) {
        return None;
    }
    let kept = match text.find(&format!("\n{NOTIFICATION}")) {
        Some(end) => text[..end].trim_end_matches(crate::query::js_space),
        None => text,
    };
    (!kept.trim_start_matches(crate::query::js_space).is_empty()).then_some(kept)
}

// ---------------------------------------------------------------------------
// Tree walks
// ---------------------------------------------------------------------------

/// A field value as a JavaScript `Map` key. Strings, numbers, and booleans
/// compare by value; objects never match (they were distinct parsed objects).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key<'a> {
    Str(&'a str),
    Num(u64),
    Bool(bool),
}

#[derive(Clone, Copy)]
enum JsRef<'a> {
    Null,
    Bool(bool),
    Num(f64),
    Str(&'a str),
    Other,
}

impl<'a> JsRef<'a> {
    fn truthy(self) -> bool {
        match self {
            JsRef::Null => false,
            JsRef::Bool(value) => value,
            JsRef::Num(value) => value != 0.0 && !value.is_nan(),
            JsRef::Str(value) => !value.is_empty(),
            JsRef::Other => true,
        }
    }

    fn key(self) -> Option<Key<'a>> {
        match self {
            JsRef::Str(value) => Some(Key::Str(value)),
            JsRef::Num(value) => Some(Key::Num(if value == 0.0 { 0 } else { value.to_bits() })),
            JsRef::Bool(value) => Some(Key::Bool(value)),
            JsRef::Null | JsRef::Other => None,
        }
    }

    fn from_value(value: &'a Value) -> JsRef<'a> {
        match value {
            Value::Null => JsRef::Null,
            Value::Bool(value) => JsRef::Bool(*value),
            Value::Number(number) => JsRef::Num(number.as_f64().unwrap_or(f64::NAN)),
            Value::String(value) => JsRef::Str(value),
            _ => JsRef::Other,
        }
    }

    fn from_js(value: &'a JsVal) -> JsRef<'a> {
        match value {
            JsVal::Null => JsRef::Null,
            JsVal::Bool(value) => JsRef::Bool(*value),
            JsVal::Num(value) => JsRef::Num(*value),
            JsVal::Str(value) => JsRef::Str(value),
            JsVal::Other => JsRef::Other,
        }
    }
}

/// What the branch walks need from a row; `None` is `undefined`.
trait TreeNode {
    fn node_type(&self) -> Option<&str>;
    fn link(&self, link: usize) -> Option<JsRef<'_>>;
    /// Whether the row's message records a Droid hook run (`hookEventName`).
    fn is_hook(&self) -> bool;
}

impl<const FULL: bool> TreeNode for Row<FULL> {
    fn node_type(&self) -> Option<&str> {
        self.kind.0.as_deref()
    }
    fn link(&self, link: usize) -> Option<JsRef<'_>> {
        // A JSON null and a missing field behave alike in every walk below.
        Some(JsRef::from_js(&self.links[link]))
    }
    fn is_hook(&self) -> bool {
        self.message
            .as_ref()
            .is_some_and(|message| message.hook_event.0.is_some())
    }
}

impl TreeNode for TreeEntry {
    fn node_type(&self) -> Option<&str> {
        self.str_field("type")
    }
    fn link(&self, link: usize) -> Option<JsRef<'_>> {
        self.get(LINK_NAMES[link]).map(JsRef::from_value)
    }
    fn is_hook(&self) -> bool {
        self.get("message")
            .and_then(|message| message.get("hookEventName"))
            .is_some_and(|name| !name.is_null())
    }
}

fn truthy_link<N: TreeNode>(node: &N, link: usize) -> Option<JsRef<'_>> {
    node.link(link).filter(|value| value.truthy())
}

/// Follows parent links from `start` and returns the branch root-first.
/// A cycle ends the walk where it would revisit a row (JavaScript looped forever).
fn walk<'a, N: TreeNode>(
    nodes: &'a [N],
    by_id: &HashMap<Key<'a>, usize>,
    start: Option<usize>,
    parent: impl Fn(&'a N) -> Option<JsRef<'a>>,
) -> Vec<usize> {
    let mut seen = vec![false; nodes.len()];
    let mut branch = Vec::new();
    let mut current = start;
    while let Some(index) = current {
        if std::mem::replace(&mut seen[index], true) {
            break;
        }
        branch.push(index);
        current = parent(&nodes[index])
            .filter(|value| value.truthy())
            .and_then(JsRef::key)
            .and_then(|key| by_id.get(&key).copied());
    }
    branch.reverse();
    branch
}

/// Pi's active branch: from the last non-`session` row up through `parentId`.
/// omp also writes a `title` row, which is not a tree node either.
fn pi_branch<N: TreeNode>(nodes: &[N]) -> Vec<usize> {
    let mut by_id = HashMap::new();
    let mut last = None;
    for (index, node) in nodes.iter().enumerate() {
        if matches!(node.node_type(), Some("session" | "title")) {
            continue;
        }
        last = Some(index);
        if let Some(key) = truthy_link(node, LINK_ID).and_then(JsRef::key) {
            by_id.insert(key, index);
        }
    }
    // Rows of type `session` are not tree nodes, so a parentId never resolves to one.
    walk(nodes, &by_id, last, |node| node.link(LINK_PARENT_ID))
}

/// Droid's active branch: from the last non-hook `message` row up through `parentId`.
/// Only `message` rows are tree nodes; `session_start`, `agent_turn_outcome`, and
/// the other bookkeeping rows are neither the leaf nor on the branch. Hook rows
/// never become the leaf, because Droid writes `SessionEnd` last without a parent.
/// Droid appends rows, so a parent always precedes its child: a `parentId` resolves
/// to the newest earlier row with that id. Droid can repeat an id on a later row
/// that names itself as parent, and the last-row-wins lookup would loop there.
fn droid_branch<N: TreeNode>(nodes: &[N]) -> Vec<usize> {
    let mut by_id: HashMap<Key<'_>, Vec<usize>> = HashMap::new();
    let mut last = None;
    for (index, node) in nodes.iter().enumerate() {
        if node.node_type() != Some("message") {
            continue;
        }
        if !node.is_hook() {
            last = Some(index);
        }
        if let Some(key) = truthy_link(node, LINK_ID).and_then(JsRef::key) {
            by_id.entry(key).or_default().push(index);
        }
    }
    let mut branch = Vec::new();
    let mut current = last;
    while let Some(index) = current {
        branch.push(index);
        current = truthy_link(&nodes[index], LINK_PARENT_ID)
            .and_then(JsRef::key)
            .and_then(|key| by_id.get(&key))
            .and_then(|rows| rows.iter().rev().find(|&&row| row < index).copied());
    }
    branch.reverse();
    // A `compaction_state` row is not a tree node. It goes before the first branch
    // message written after it, so a session that holds only a summary still has one.
    for (index, node) in nodes.iter().enumerate() {
        if node.node_type() == Some("compaction_state") {
            let at = branch.partition_point(|&row| row < index);
            branch.insert(at, index);
        }
    }
    branch
}

/// Claude's active branch: from the recorded `last-prompt` leaf (or the last row
/// with a uuid) up through `parentUuid`, crossing compaction boundaries through
/// `logicalParentUuid`.
fn claude_branch<N: TreeNode>(nodes: &[N]) -> Vec<usize> {
    let mut by_id = HashMap::new();
    let mut last_with_uuid = None;
    let mut last_prompt = None;
    for (index, node) in nodes.iter().enumerate() {
        if let Some(uuid) = truthy_link(node, LINK_UUID) {
            last_with_uuid = Some(index);
            if let Some(key) = uuid.key() {
                by_id.insert(key, index);
            }
        }
        if node.node_type() == Some("last-prompt") {
            last_prompt = Some(index);
        }
    }
    let recorded = last_prompt.and_then(|index| truthy_link(&nodes[index], LINK_LEAF_UUID));
    let start = match recorded {
        Some(leaf) => leaf.key().and_then(|key| by_id.get(&key).copied()),
        None => last_with_uuid,
    };
    walk(nodes, &by_id, start, |node| {
        match node.link(LINK_PARENT_UUID) {
            Some(JsRef::Null) | None => node.link(LINK_LOGICAL_PARENT_UUID),
            parent => parent,
        }
    })
}

// ---------------------------------------------------------------------------
// Raw tree entries
// ---------------------------------------------------------------------------

/// One parsed JSONL row (`TreeEntry`): its 1-based line number and the raw
/// JSON value. Non-object rows are kept, as `JSON.parse` returned them.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeEntry {
    /// 1-based line number in the source file.
    pub line: usize,
    pub value: Value,
}

impl TreeEntry {
    /// A top-level field (`entry.<key>`); `None` for missing fields and non-object rows.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.as_object()?.get(key)
    }

    /// A top-level string field.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// `entry.type` when it is a string.
    pub fn kind(&self) -> Option<&str> {
        self.str_field("type")
    }

    /// `entry.timestamp` when it is a string.
    pub fn timestamp(&self) -> Option<&str> {
        self.str_field("timestamp")
    }

    /// `entry.cwd` when it is a string.
    pub fn cwd(&self) -> Option<&str> {
        self.str_field("cwd")
    }

    /// `entry.message` when it is an object.
    pub fn message(&self) -> Option<&serde_json::Map<String, Value>> {
        self.get("message")?.as_object()
    }

    /// `entry.payload` when it is an object.
    pub fn payload(&self) -> Option<&serde_json::Map<String, Value>> {
        self.get("payload")?.as_object()
    }
}

/// `parseJsonl(text)`: every line that parses as JSON, with its line number.
pub fn parse_jsonl(text: &str) -> Vec<TreeEntry> {
    text.split('\n')
        .enumerate()
        .filter_map(|(index, line)| {
            if line.trim().is_empty() {
                return None;
            }
            parse_json_line::<Value>(line).map(|value| TreeEntry {
                line: index + 1,
                value,
            })
        })
        .collect()
}

/// `loadBranchEntries(locator, source)`: the raw rows of a JSONL transcript,
/// reduced to the active branch for Pi, omp, Claude, and Droid.
pub fn load_branch_entries(
    locator: &str,
    source: TranscriptSource,
) -> Result<Vec<TreeEntry>, String> {
    Ok(branch_entries(parse_jsonl(&read_text(locator)?), source))
}

/// The active branch of already-parsed rows (all rows for Codex and OpenCode).
pub fn branch_entries(entries: Vec<TreeEntry>, source: TranscriptSource) -> Vec<TreeEntry> {
    let branch = match source {
        TranscriptSource::Pi
        | TranscriptSource::Omp
        | TranscriptSource::Openclaw
        | TranscriptSource::Hermes => pi_branch(&entries),
        TranscriptSource::Claude => claude_branch(&entries),
        TranscriptSource::Droid => droid_branch(&entries),
        _ => return entries,
    };
    take_indices(entries, &branch)
}

/// Moves the rows at `indices` (ascending or not, without repeats) out of `items`.
fn take_indices<T>(items: Vec<T>, indices: &[usize]) -> Vec<T> {
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    indices
        .iter()
        .filter_map(|&index| slots[index].take())
        .collect()
}

// ---------------------------------------------------------------------------
// Recall messages
// ---------------------------------------------------------------------------

/// `loadRecallMessages(locator, source?)`: the visible user/assistant messages
/// of a transcript, in order. The source comes from the locator when not forced.
pub fn load_recall_messages(
    locator: &str,
    forced_source: Option<TranscriptSource>,
) -> Result<Vec<RecallMessage>, String> {
    let source = match forced_source {
        Some(source) => source,
        None => source_from_locator(locator, default_roots())?,
    };
    if source == TranscriptSource::Opencode {
        return load_opencode_messages(locator);
    }
    Ok(recall_messages_from_text(&read_text(locator)?, source))
}

/// The recall messages of JSONL transcript text (`source` must not be OpenCode).
pub fn recall_messages_from_text(text: &str, source: TranscriptSource) -> Vec<RecallMessage> {
    if source == TranscriptSource::Agy {
        return crate::agy::recall_messages(text);
    }
    let rows = text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .filter_map(parse_json_line::<Row<true>>);
    let summary_message = |row: &Row<true>| {
        row.compaction_summary(source).map(|text| RecallMessage {
            role: "user".to_string(),
            content: vec![RecallBlock::Text { text }],
        })
    };
    let to_message = |envelope: Option<Envelope<true>>| {
        let (role, mut content) = envelope?.into_recall_from(source)?;
        // Codex sends AGENTS.md and environment context as user text blocks.
        // Search still indexes them; show and query drop them, as find does.
        if source == TranscriptSource::Codex && role == "user" {
            content.retain(|block| match block {
                RecallBlock::Text { text } => crate::find::is_real_user_prompt(text),
                _ => true,
            });
            if content.is_empty() {
                return None;
            }
        }
        Some(RecallMessage {
            role: role.to_string(),
            content,
        })
    };
    match source {
        TranscriptSource::Pi
        | TranscriptSource::Omp
        | TranscriptSource::Openclaw
        | TranscriptSource::Hermes
        | TranscriptSource::Claude
        | TranscriptSource::Droid => {
            let rows: Vec<Row<true>> = rows.collect();
            let branch = match source {
                TranscriptSource::Pi
                | TranscriptSource::Omp
                | TranscriptSource::Openclaw
                | TranscriptSource::Hermes => pi_branch(&rows),
                TranscriptSource::Droid => droid_branch(&rows),
                _ => claude_branch(&rows),
            };
            take_indices(rows, &branch)
                .into_iter()
                .filter_map(|row| summary_message(&row).or_else(|| to_message(row.message)))
                .collect()
        }
        _ => rows
            .filter_map(|row| {
                if let Some(message) = summary_message(&row) {
                    return Some(message);
                }
                let payload = row
                    .payload
                    .filter(|_| row.kind.0.as_deref() == Some("response_item"))?;
                to_message(
                    Some(payload).filter(|payload| payload.kind.0.as_deref() == Some("message")),
                )
            })
            .collect(),
    }
}

/// `serializeRecallMessages(messages)`: `[role]` headers over block text, tool
/// calls as `[tool call: name]` plus compact JSON arguments, images as `[image omitted]`.
pub fn serialize_recall_messages(messages: &[RecallMessage]) -> String {
    let mut out = String::new();
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push('[');
        out.push_str(&message.role);
        out.push_str("]\n");
        for (block_index, block) in message.content.iter().enumerate() {
            if block_index > 0 {
                out.push('\n');
            }
            match block {
                RecallBlock::Text { text } => out.push_str(text),
                RecallBlock::ToolCall { name, arguments } => {
                    out.push_str("[tool call: ");
                    out.push_str(name);
                    out.push_str("]\n");
                    out.push_str(&js::stringify(arguments));
                }
                RecallBlock::Image => out.push_str("[image omitted]"),
            }
        }
    }
    out
}

/// The result of [`extract_visible_message`]. `date` and `project` are omitted
/// from JSON when the row has no string `timestamp` / `cwd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisibleMessage {
    pub role: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `extractVisibleMessage(line, source)`: one JSONL row's visible user or
/// assistant text (text blocks joined by a space), its date (`timestamp`'s
/// first 10 units), and its `cwd`. `None` for other rows and empty text.
/// Droid's injected `<system-reminder>` user blocks do not count as text.
pub fn extract_visible_message(line: &str, source: TranscriptSource) -> Option<VisibleMessage> {
    if source == TranscriptSource::Agy {
        let (role, text, date) = crate::agy::visible_text(line)?;
        return Some(VisibleMessage {
            role,
            text,
            date,
            project: None,
        });
    }
    let row = parse_json_line::<Row<false>>(line)?;
    if let Some(text) = row.compaction_summary(source) {
        return Some(VisibleMessage {
            role: "user",
            text,
            date: row
                .timestamp
                .0
                .map(|timestamp| js::prefix(&timestamp, 10).to_string()),
            project: row.cwd.0,
        });
    }
    let envelope = if source == TranscriptSource::Codex {
        if row.kind.0.as_deref() != Some("response_item") {
            return None;
        }
        let payload = row.payload?;
        if payload.kind.0.as_deref() != Some("message") {
            return None;
        }
        payload
    } else {
        row.message?
    };
    let (role, content) = envelope.into_recall_from(source)?;
    let mut text = String::new();
    let mut first = true;
    for block in &content {
        if let RecallBlock::Text { text: part } = block {
            if !first {
                text.push(' ');
            }
            text.push_str(part);
            first = false;
        }
    }
    if text.is_empty() {
        return None;
    }
    Some(VisibleMessage {
        role,
        text,
        date: row
            .timestamp
            .0
            .map(|timestamp| js::prefix(&timestamp, 10).to_string()),
        project: row.cwd.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(values: &[Value]) -> String {
        values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn message(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
        json!({ "type": "message", "id": id, "parentId": parent, "message": { "role": role, "content": [{ "type": "text", "text": text }] } })
    }

    fn claude(uuid: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
        json!({ "type": role, "uuid": uuid, "parentUuid": parent, "message": { "role": role, "content": [{ "type": "text", "text": text }] } })
    }

    fn texts(messages: &[RecallMessage]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.content[0].text().unwrap())
            .collect()
    }

    /// Loads through both paths (typed rows and raw `TreeEntry` values) and checks they agree.
    fn load(values: &[Value], source: TranscriptSource) -> Vec<RecallMessage> {
        let text = rows(values);
        let typed = recall_messages_from_text(&text, source);
        let entries = branch_entries(parse_jsonl(&text), source);
        let via_entries: Vec<RecallMessage> = entries
            .iter()
            .filter(|entry| {
                source != TranscriptSource::Codex || entry.kind() == Some("response_item")
            })
            .filter_map(|entry| {
                let key = if source == TranscriptSource::Codex {
                    "payload"
                } else {
                    "message"
                };
                let envelope = entry.get(key)?;
                if source == TranscriptSource::Codex
                    && envelope.get("type")?.as_str() != Some("message")
                {
                    return None;
                }
                let envelope: Envelope<true> = serde_json::from_value(envelope.clone()).ok()?;
                envelope.into_recall().map(|(role, content)| RecallMessage {
                    role: role.into(),
                    content,
                })
            })
            .collect();
        assert_eq!(typed, via_entries);
        typed
    }

    #[test]
    fn reads_pi_messages_without_thinking_or_tool_output() {
        let line = json!({ "type": "message", "message": { "role": "assistant", "content": [
            { "type": "thinking", "thinking": "hidden" }, { "type": "text", "text": "visible" },
        ] } })
        .to_string();
        let message = extract_visible_message(&line, TranscriptSource::Pi).unwrap();
        assert_eq!(
            (message.role, message.text.as_str()),
            ("assistant", "visible")
        );
    }

    #[test]
    fn reads_claude_user_and_assistant_envelopes() {
        let line = json!({ "type": "user", "cwd": "/work", "timestamp": "2026-09-20T10:00:00Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Claude text" }] } })
        .to_string();
        let message = extract_visible_message(&line, TranscriptSource::Claude).unwrap();
        assert_eq!(message.role, "user");
        assert_eq!(message.text, "Claude text");
        assert_eq!(message.project.as_deref(), Some("/work"));
        assert_eq!(message.date.as_deref(), Some("2026-09-20"));
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","text":"Claude text","date":"2026-09-20","project":"/work"}"#
        );
    }

    #[test]
    fn reads_codex_response_messages_but_ignores_developer_context() {
        let user = json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Codex text" }] } });
        let developer = json!({ "type": "response_item", "payload": { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "rules" }] } });
        let message = extract_visible_message(&user.to_string(), TranscriptSource::Codex).unwrap();
        assert_eq!(
            (message.role, message.text.as_str()),
            ("user", "Codex text")
        );
        assert!(extract_visible_message(&developer.to_string(), TranscriptSource::Codex).is_none());
        // Codex rows need both type checks; other sources read `message`, not `payload`.
        assert!(extract_visible_message(&user.to_string(), TranscriptSource::Claude).is_none());
        let wrong = json!({ "type": "event_msg", "payload": { "type": "message", "role": "user", "content": "x" } });
        assert!(extract_visible_message(&wrong.to_string(), TranscriptSource::Codex).is_none());
    }

    #[test]
    fn extraction_edge_cases() {
        let pi = TranscriptSource::Pi;
        // String content, joined text blocks, and empty-string blocks that still join to " ".
        let line = json!({ "message": { "role": "user", "content": "plain" } }).to_string();
        assert_eq!(extract_visible_message(&line, pi).unwrap().text, "plain");
        let line = json!({ "message": { "role": "user", "content": [{ "type": "text", "text": "a" }, { "type": "image" }, { "type": "output_text", "text": "b" }] } }).to_string();
        assert_eq!(extract_visible_message(&line, pi).unwrap().text, "a b");
        let line = json!({ "message": { "role": "user", "content": [{ "type": "text", "text": "" }, { "type": "text", "text": "" }] } }).to_string();
        assert_eq!(extract_visible_message(&line, pi).unwrap().text, " ");
        // Tool-only and image-only messages have no visible text.
        let line = json!({ "message": { "role": "assistant", "content": [{ "type": "tool_use", "name": "x", "input": {} }] } }).to_string();
        assert!(extract_visible_message(&line, pi).is_none());
        // String(["text"]) is "text" in JavaScript.
        let line = json!({ "message": { "role": "user", "content": [{ "type": ["text"], "text": "arr" }] } }).to_string();
        assert_eq!(extract_visible_message(&line, pi).unwrap().text, "arr");
        // Fields of the wrong type read as missing; non-object rows and invalid JSON are skipped.
        let line = json!({ "timestamp": 5, "cwd": ["x"], "message": { "role": "user", "content": [{ "type": "text", "text": 5 }, 7, null, { "type": "text", "text": "ok" }] } }).to_string();
        let message = extract_visible_message(&line, pi).unwrap();
        assert_eq!(
            (message.text.as_str(), message.date, message.project),
            ("ok", None, None)
        );
        assert!(extract_visible_message("[1,2]", pi).is_none());
        assert!(extract_visible_message("\"s\"", pi).is_none());
        assert!(extract_visible_message("{oops", pi).is_none());
        assert!(extract_visible_message(&json!({ "message": "user" }).to_string(), pi).is_none());
        // Last duplicate key wins, as in JSON.parse.
        let line = r#"{"message":{"role":"assistant","role":"user","content":"dup"}}"#;
        assert_eq!(extract_visible_message(line, pi).unwrap().role, "user");
        // A lone surrogate escape still parses, read as U+FFFD.
        let line = r#"{"message":{"role":"user","content":"a\ud83d b 😀"}}"#;
        assert_eq!(
            extract_visible_message(line, pi).unwrap().text,
            "a\u{fffd} b 😀"
        );
        // The date is the first ten UTF-16 units of the timestamp.
        let line = json!({ "timestamp": "2026", "message": { "role": "user", "content": "x" } })
            .to_string();
        assert_eq!(
            extract_visible_message(&line, pi).unwrap().date.as_deref(),
            Some("2026")
        );
    }

    #[test]
    fn loads_the_active_pi_branch() {
        let values = [
            json!({ "type": "session", "version": 3, "id": "session", "cwd": "/tmp" }),
            message("a", None, "user", "root"),
            message("old", Some("a"), "assistant", "abandoned"),
            message("new", Some("a"), "assistant", "active"),
        ];
        assert_eq!(
            texts(&load(&values, TranscriptSource::Pi)),
            ["root", "active"]
        );
    }

    #[test]
    fn pi_branch_skips_session_rows_and_ends_at_unknown_parents() {
        let values = [
            message("a", None, "user", "lost"),
            message("b", Some("missing"), "user", "orphan"),
            message("c", Some("b"), "assistant", "tip"),
            json!({ "type": "session", "id": "s" }),
        ];
        assert_eq!(
            texts(&load(&values, TranscriptSource::Pi)),
            ["orphan", "tip"]
        );
        // A cycle stops instead of looping.
        let values = [
            message("a", Some("b"), "user", "one"),
            message("b", Some("a"), "assistant", "two"),
        ];
        assert_eq!(texts(&load(&values, TranscriptSource::Pi)), ["one", "two"]);
        // The last row decides the leaf even when it is not a message.
        let values = [
            message("a", None, "user", "root"),
            json!({ "type": "model_change", "id": "m", "parentId": "a" }),
        ];
        assert_eq!(texts(&load(&values, TranscriptSource::Pi)), ["root"]);
        // omp writes a title row first and may rewrite it in place; it is
        // never the leaf.
        let values = [
            message("a", None, "user", "root"),
            message("b", Some("a"), "assistant", "tip"),
            json!({ "type": "title", "v": 1, "title": "Named" }),
        ];
        assert_eq!(
            texts(&load(&values, TranscriptSource::Omp)),
            ["root", "tip"]
        );
    }

    #[test]
    fn loads_claudes_recorded_leaf_branch() {
        let values = [
            claude("a", None, "user", "root"),
            claude("old", Some("a"), "assistant", "abandoned"),
            claude("new", Some("a"), "assistant", "active"),
            json!({ "type": "last-prompt", "leafUuid": "new" }),
        ];
        assert_eq!(
            texts(&load(&values, TranscriptSource::Claude)),
            ["root", "active"]
        );
    }

    #[test]
    fn claude_without_a_recorded_leaf_uses_the_last_uuid_row() {
        let values = [
            claude("a", None, "user", "root"),
            claude("new", Some("a"), "assistant", "active"),
            claude("old", Some("a"), "assistant", "latest"),
            json!({ "type": "summary", "summary": "s" }),
        ];
        assert_eq!(
            texts(&load(&values, TranscriptSource::Claude)),
            ["root", "latest"]
        );
        // A recorded leaf that does not resolve yields nothing, as in TypeScript.
        let mut values = values.to_vec();
        values.push(json!({ "type": "last-prompt", "leafUuid": "gone" }));
        assert!(load(&values, TranscriptSource::Claude).is_empty());
        // The last last-prompt row decides, and one without a leafUuid falls back.
        values.push(json!({ "type": "last-prompt" }));
        assert_eq!(
            texts(&load(&values, TranscriptSource::Claude)),
            ["root", "latest"]
        );
    }

    #[test]
    fn follows_claudes_branch_across_compaction_boundaries() {
        let values = [
            claude("a", None, "user", "before compaction"),
            claude("b", Some("a"), "assistant", "early answer"),
            json!({ "type": "system", "subtype": "compact_boundary", "uuid": "c", "parentUuid": null, "logicalParentUuid": "b" }),
            claude("d", Some("c"), "user", "after compaction"),
        ];
        assert_eq!(
            texts(&load(&values, TranscriptSource::Claude)),
            ["before compaction", "early answer", "after compaction"]
        );
        // An empty parentUuid is not nullish, so logicalParentUuid is not consulted.
        let values = [
            claude("a", None, "user", "before"),
            json!({ "type": "system", "uuid": "c", "parentUuid": "", "logicalParentUuid": "a" }),
            claude("d", Some("c"), "user", "after"),
        ];
        assert_eq!(texts(&load(&values, TranscriptSource::Claude)), ["after"]);
    }

    #[test]
    fn loads_only_user_and_assistant_codex_response_items() {
        let values = [
            json!({ "type": "response_item", "payload": { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "rules" }] } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "# AGENTS.md instructions for /work" }, { "type": "input_text", "text": "<environment_context>x</environment_context>" }] } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "<recommended_plugins>x</recommended_plugins>" }, { "type": "input_text", "text": "question" }] } }),
            json!({ "type": "response_item", "payload": { "type": "reasoning", "summary": [] } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "answer" }] } }),
        ];
        let text: String = values.iter().map(|value| format!("{value}\n")).collect();
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Codex)),
            ["question", "answer"]
        );
    }

    #[test]
    fn normalizes_tool_calls_and_images() {
        let values = [
            json!({ "type": "assistant", "uuid": "a", "message": { "role": "assistant", "content": [
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } },
            { "type": "toolCall", "name": 5, "arguments": null, "input": null },
            { "type": "toolCall", "name": "edit", "arguments": { "path": "x" }, "input": { "ignored": true } },
            { "type": "input_image" },
            { "type": "tool_result", "content": "big" },
        ] } }),
        ];
        let messages = load(&values, TranscriptSource::Claude);
        assert_eq!(
            messages[0].content,
            vec![
                RecallBlock::ToolCall {
                    name: "Bash".into(),
                    arguments: json!({ "command": "ls" })
                },
                RecallBlock::ToolCall {
                    name: "unknown".into(),
                    arguments: json!({})
                },
                RecallBlock::ToolCall {
                    name: "edit".into(),
                    arguments: json!({ "path": "x" })
                },
                RecallBlock::Image,
            ]
        );
        assert_eq!(
            serialize_recall_messages(&messages),
            "[assistant]\n[tool call: Bash]\n{\"command\":\"ls\"}\n[tool call: unknown]\n{}\n[tool call: edit]\n{\"path\":\"x\"}\n[image omitted]"
        );
        let two = [
            RecallMessage {
                role: "user".into(),
                content: vec![RecallBlock::Text { text: "q".into() }],
            },
            RecallMessage {
                role: "assistant".into(),
                content: vec![
                    RecallBlock::Text { text: "a".into() },
                    RecallBlock::Text { text: "b".into() },
                ],
            },
        ];
        assert_eq!(
            serialize_recall_messages(&two),
            "[user]\nq\n\n[assistant]\na\nb"
        );
    }

    fn droid_rows() -> Vec<Value> {
        let reminder = |text: &str| json!({ "type": "text", "text": format!("<system-reminder>{text}</system-reminder>") });
        vec![
            json!({ "type": "session_start", "id": "sess", "title": "t", "cwd": "/work/app", "owner": "o", "version": 2 }),
            json!({ "type": "message", "id": "m0", "timestamp": "2026-09-01T10:00:00Z", "message": { "role": "user", "visibility": "llm_only", "content": [reminder("tools"), reminder("skills")] } }),
            json!({ "type": "message", "id": "m1", "parentId": "m0", "timestamp": "2026-09-01T10:00:01Z", "message": { "role": "user", "content": [reminder("system info"), { "type": "text", "text": "fix the build" }] } }),
            json!({ "type": "message", "id": "m2", "parentId": "m1", "message": { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "plan" },
                { "type": "text", "text": "abandoned answer" },
            ] } }),
            json!({ "type": "agent_turn_outcome", "turnId": "x", "reason": "done" }),
            json!({ "type": "message", "id": "m3", "parentId": "m1", "message": { "role": "assistant", "content": [
                { "type": "text", "text": "running" },
                { "type": "tool_use", "id": "t1", "name": "Execute", "input": { "command": "make" } },
            ] } }),
            json!({ "type": "message", "id": "m4", "parentId": "m3", "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false }] } }),
            json!({ "type": "message", "id": "m5", "parentId": "m4", "message": { "role": "assistant", "content": [{ "type": "text", "text": "built" }] } }),
            json!({ "type": "agent_turn_outcome", "turnId": "y", "reason": "done" }),
        ]
    }

    #[test]
    fn droid_follows_the_last_message_branch_and_drops_injected_reminders() {
        let text = rows(&droid_rows());
        let messages = recall_messages_from_text(&text, TranscriptSource::Droid);
        assert_eq!(
            serialize_recall_messages(&messages),
            "[user]\nfix the build\n\n[assistant]\nrunning\n[tool call: Execute]\n{\"command\":\"make\"}\n\n[assistant]\nbuilt"
        );
        // The branch holds message rows only: session_start and agent_turn_outcome are
        // neither the leaf nor on the walk, and the abandoned fork (line 4) is skipped.
        let lines: Vec<usize> = branch_entries(parse_jsonl(&text), TranscriptSource::Droid)
            .iter()
            .map(|entry| entry.line)
            .collect();
        assert_eq!(lines, [2, 3, 6, 7, 8]);
        // Claude keeps reminder text as written.
        let claude = rows(&[claude("u1", None, "user", "<system-reminder>kept")]);
        assert_eq!(
            texts(&recall_messages_from_text(
                &claude,
                TranscriptSource::Claude
            )),
            ["<system-reminder>kept"]
        );
    }

    #[test]
    fn droid_drops_model_only_messages() {
        let hidden = json!({ "type": "message", "id": "c", "parentId": "a", "message": {
            "role": "user", "visibility": "llm_only",
            "content": [{ "type": "text", "text": "Continue where you left off." }] } });
        let shown = json!({ "type": "message", "id": "d", "parentId": "c", "message": {
            "role": "user", "visibility": "user_only",
            "content": [{ "type": "text", "text": "Request interrupted" }] } });
        let text = rows(&[message("a", None, "user", "ask"), hidden.clone(), shown]);
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Droid)),
            ["ask", "Request interrupted"]
        );
        assert!(extract_visible_message(&hidden.to_string(), TranscriptSource::Droid).is_none());
        // Other sources ignore the field.
        let claude = json!({ "type": "user", "uuid": "u", "message": {
            "role": "user", "visibility": "llm_only", "content": [{ "type": "text", "text": "kept" }] } });
        assert!(extract_visible_message(&claude.to_string(), TranscriptSource::Claude).is_some());
    }

    #[test]
    fn droid_visible_messages_skip_reminders_and_bookkeeping_rows() {
        let droid = TranscriptSource::Droid;
        let rows = droid_rows();
        let visible: Vec<Option<String>> = rows
            .iter()
            .map(|row| extract_visible_message(&row.to_string(), droid).map(|message| message.text))
            .collect();
        assert_eq!(
            visible,
            [
                None,
                None,
                Some("fix the build".into()),
                Some("abandoned answer".into()),
                None,
                Some("running".into()),
                None,
                Some("built".into()),
                None,
            ]
        );
        let message = extract_visible_message(&rows[2].to_string(), droid).unwrap();
        assert_eq!(
            (message.role, message.date.as_deref(), message.project),
            ("user", Some("2026-09-01"), None)
        );
        // Leading whitespace before the tag still marks an injected block; assistant
        // text and Claude rows are never filtered.
        let padded = json!({ "type": "message", "message": { "role": "user", "content": [{ "type": "text", "text": "\n  <system-reminder>x" }] } });
        assert!(extract_visible_message(&padded.to_string(), droid).is_none());
        assert!(extract_visible_message(&padded.to_string(), TranscriptSource::Claude).is_some());
        let assistant = json!({ "type": "message", "message": { "role": "assistant", "content": "<system-reminder>quoted" } });
        assert!(extract_visible_message(&assistant.to_string(), droid).is_some());
    }

    #[test]
    fn droid_hook_rows_never_become_the_leaf() {
        // Droid records hook runs as user rows; SessionEnd comes last with no parent.
        let hook = |id: &str, parent: Option<&str>, event: &str| {
            json!({ "type": "message", "id": id, "parentId": parent, "message": {
                "role": "user", "content": [], "visibility": "user_only", "hookEventName": event } })
        };
        let mut rows_with_hooks = droid_rows();
        rows_with_hooks.insert(1, hook("h0", None, "SessionStart"));
        rows_with_hooks.push(hook("h1", Some("m4"), "PostToolUse"));
        rows_with_hooks.push(hook("h2", None, "SessionEnd"));
        let text = rows(&rows_with_hooks);
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Droid)),
            ["fix the build", "running", "built"]
        );
        let lines: Vec<usize> = branch_entries(parse_jsonl(&text), TranscriptSource::Droid)
            .iter()
            .map(|entry| entry.line)
            .collect();
        assert_eq!(lines, [3, 4, 7, 8, 9]);
    }

    #[test]
    fn droid_parent_links_resolve_to_earlier_rows_when_an_id_repeats() {
        // Droid reused m4's id for a self-parented row (a cancelled tool result).
        let mut repeated = droid_rows();
        repeated.push(
            json!({ "type": "message", "id": "m4", "parentId": "m4", "message": {
            "role": "user", "visibility": "user_only",
            "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "ok" }] } }),
        );
        repeated.push(message("m6", Some("m4"), "user", "next"));
        let text = rows(&repeated);
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Droid)),
            ["fix the build", "running", "next"]
        );
        let lines: Vec<usize> = branch_entries(parse_jsonl(&text), TranscriptSource::Droid)
            .iter()
            .map(|entry| entry.line)
            .collect();
        assert_eq!(lines, [2, 3, 6, 7, 10, 11]);
    }

    #[test]
    fn droid_user_text_drops_appended_skill_notifications() {
        let notification = "<system-notification>\nSkills provide...\n<skill filePath=\"/s/SKILL.md\">body</skill>\n</system-notification>";
        let prompt = format!("fix the build\n\n{notification}");
        assert_eq!(droid_user_text(&prompt), Some("fix the build"));
        assert_eq!(droid_user_text(notification), None);
        assert_eq!(droid_user_text("  <system-reminder>x"), None);
        assert_eq!(droid_user_text("\n\n"), None);
        // Only a line-leading tag is injected; a mention inside a sentence is the user's.
        assert_eq!(
            droid_user_text("what is <system-notification> for?"),
            Some("what is <system-notification> for?")
        );
        let row = json!({ "type": "message", "id": "a", "message": { "role": "user",
            "content": [{ "type": "text", "text": prompt }] } });
        let text = rows(std::slice::from_ref(&row));
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Droid)),
            ["fix the build"]
        );
        let visible = extract_visible_message(&row.to_string(), TranscriptSource::Droid).unwrap();
        assert_eq!(visible.text, "fix the build");
        // Claude keeps the text as written.
        assert!(
            extract_visible_message(&row.to_string(), TranscriptSource::Claude)
                .is_some_and(|message| message.text.contains("<skill"))
        );
    }

    #[test]
    fn compaction_summaries_read_as_user_messages_where_they_happened() {
        let summary = |kind: &str| {
            json!({ "type": "compaction_state", "id": "c", "summaryKind": kind,
                "summaryText": "earlier work", "removedCount": 12, "timestamp": "2026-09-01T09:00:00Z" })
        };
        // A Droid session that holds only a summary still recalls it.
        let only = rows(&[
            json!({ "type": "session_start", "id": "sess", "cwd": "/w" }),
            summary("llm_summary"),
        ]);
        assert_eq!(
            texts(&recall_messages_from_text(&only, TranscriptSource::Droid)),
            ["[Compaction summary of 12 earlier messages]\n\nearlier work"]
        );
        // In place, the summary sits before the first message written after it;
        // provider-switch serializations repeat the turns and are skipped.
        let mut in_place = droid_rows();
        in_place.insert(6, summary("llm_summary"));
        in_place.insert(1, summary("provider_switch_serialization"));
        let text = rows(&in_place);
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Droid)),
            [
                "fix the build",
                "running",
                "[Compaction summary of 12 earlier messages]\n\nearlier work",
                "built"
            ]
        );
        let summary_line = summary("llm_summary").to_string();
        let visible = extract_visible_message(&summary_line, TranscriptSource::Droid).unwrap();
        assert_eq!(
            (visible.role, visible.date.as_deref()),
            ("user", Some("2026-09-01"))
        );
        assert!(extract_visible_message(&summary_line, TranscriptSource::Claude).is_none());
        // Codex keeps a plaintext summary only in `message`; encrypted ones leave it empty.
        let codex = |message: &str| json!({ "type": "compacted", "payload": { "message": message, "replacement_history": [] } });
        let user = json!({ "type": "response_item", "payload": { "type": "message", "role": "user",
            "content": [{ "type": "input_text", "text": "next" }] } });
        let text = rows(&[codex("handoff notes"), codex(""), user]);
        assert_eq!(
            texts(&recall_messages_from_text(&text, TranscriptSource::Codex)),
            ["[Compaction summary]\n\nhandoff notes", "next"]
        );
    }

    #[test]
    fn droid_without_messages_has_an_empty_branch() {
        let text = rows(&[
            json!({ "type": "session_start", "id": "sess", "cwd": "/w" }),
            json!({ "type": "agent_turn_outcome" }),
        ]);
        assert!(recall_messages_from_text(&text, TranscriptSource::Droid).is_empty());
        assert!(branch_entries(parse_jsonl(&text), TranscriptSource::Droid).is_empty());
    }

    #[test]
    fn parses_jsonl_with_line_numbers() {
        let entries = parse_jsonl("{\"a\":1}\n\n  \nnot json\n5\n{\"b\":2}\r\n");
        assert_eq!(
            entries.iter().map(|entry| entry.line).collect::<Vec<_>>(),
            [1, 5, 6]
        );
        assert_eq!(entries[1].value, json!(5));
        assert_eq!(entries[1].get("a"), None);
    }

    #[test]
    fn loads_files_and_reports_bun_style_errors() {
        let dir = std::env::temp_dir().join(format!("dejavu-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let body = format!(
            "\u{feff}{}",
            rows(&[message("a", None, "user", "bom first line")])
        );
        std::fs::write(&path, body).unwrap();
        let path = path.to_str().unwrap();
        let messages = load_recall_messages(path, Some(TranscriptSource::Pi)).unwrap();
        assert_eq!(texts(&messages), ["bom first line"]);
        assert_eq!(
            load_branch_entries(path, TranscriptSource::Pi).unwrap()[0].line,
            1
        );
        let missing = dir.join("missing.jsonl");
        let missing = missing.to_str().unwrap();
        assert_eq!(
            load_recall_messages(missing, Some(TranscriptSource::Claude)).unwrap_err(),
            format!("ENOENT: no such file or directory, open '{missing}'")
        );
        assert_eq!(
            read_text(dir.to_str().unwrap()).unwrap_err(),
            "Directories cannot be read like files"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod real_data {
    //! Throwaway parity probe (not committed): prints only counts and hashes.
    use super::*;
    use sha2::{Digest, Sha256};

    fn newest_and_largest(root: &std::path::Path) -> Vec<String> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    let meta = entry.metadata().unwrap();
                    files.push((
                        meta.modified().unwrap(),
                        meta.len(),
                        path.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        let mut picks = Vec::new();
        files.sort_by_key(|f| f.0);
        picks.extend(files.iter().rev().take(3).map(|f| f.2.clone()));
        files.sort_by_key(|f| f.1);
        picks.extend(files.iter().rev().take(2).map(|f| f.2.clone()));
        picks
    }

    fn hash(text: &str) -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    #[ignore]
    fn probe() {
        let out = std::env::var("PROBE_OUT").unwrap();
        let mut locators = Vec::new();
        for store in crate::sources::discover_stores(crate::types::SourceSelector::All) {
            if store.kind == crate::types::StoreKind::Sqlite {
                let db = crate::opencode::open_opencode_database(&store.path).unwrap();
                let schema = crate::opencode::opencode_schema(&db).unwrap();
                let table = if schema.v2 { "session_v2" } else { "session" };
                let ids: Vec<String> = db
                    .prepare(&format!(
                        "SELECT id FROM {table} ORDER BY time_updated DESC LIMIT 3"
                    ))
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                locators.extend(
                    ids.iter()
                        .map(|id| crate::opencode::opencode_locator(&store.path, id)),
                );
            } else {
                locators.extend(newest_and_largest(std::path::Path::new(&store.path)));
            }
        }
        let mut lines = Vec::new();
        for locator in &locators {
            let source = source_from_locator(locator, default_roots()).unwrap();
            let start = std::time::Instant::now();
            let messages = load_recall_messages(locator, None).unwrap_or_default();
            let load_ms = start.elapsed().as_millis();
            let users = messages.iter().filter(|m| m.role == "user").count();
            let blocks = messages.iter().map(|m| m.content.len()).sum::<usize>();
            let serialized = hash(&serialize_recall_messages(&messages));
            let (mut visible, mut branch) = (String::from("-"), 0);
            if source != TranscriptSource::Opencode {
                let text = read_text(locator).unwrap();
                let all: Vec<String> = text
                    .split('\n')
                    .filter_map(|line| extract_visible_message(line, source))
                    .map(|m| js::stringify(&m))
                    .collect();
                visible = format!("{}:{}", all.len(), hash(&all.join("\n")));
                branch = load_branch_entries(locator, source).unwrap().len();
            }
            lines.push(format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}ms",
                source,
                messages.len(),
                users,
                blocks,
                serialized,
                visible,
                branch,
                load_ms
            ));
        }
        std::fs::write(format!("{out}/locators.txt"), locators.join("\n")).unwrap();
        std::fs::write(format!("{out}/rust.tsv"), lines.join("\n")).unwrap();
    }
}
