//! OpenCode SQLite stores (`opencode-store.ts`): read-only access, legacy and v2
//! schemas, `opencode://<db>#<session>` locators, search, and message loading.

use crate::js;
use crate::paths::{compact_home, count_occurrences, is_windows_absolute, snippet_around};
use crate::reader::{JsStr, LenientFields, deserialize_lenient, parse_json_line};
use crate::types::{
    RecallBlock, RecallMessage, StoreSearchMatch, TranscriptSnippet, TranscriptSource,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension};
use serde::de::{Deserialize, Deserializer, MapAccess};
use std::collections::HashMap;

/// Opens an OpenCode database read-only. A read-only connection to a WAL
/// database cannot work without a `-shm` file, and the bundled SQLite would
/// create one beside the database (Bun's SQLite failed with `SQLITE_CANTOPEN`
/// instead). Such a database, and any open that fails with `SQLITE_CANTOPEN`, is
/// opened as `immutable`, which reads the committed database directly (correct
/// while no OpenCode process writes to it) and creates no files.
pub fn open_opencode_database(database_path: &str) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let open_immutable = || {
        let uri = format!("file:{}?immutable=1", encode_sqlite_uri_path(database_path));
        Connection::open_with_flags(&uri, flags | OpenFlags::SQLITE_OPEN_URI)
            .map_err(|error| sqlite_message(&error, &uri))
    };
    if is_wal_without_shm(database_path) {
        return open_immutable();
    }
    let probe = Connection::open_with_flags(database_path, flags).and_then(|connection| {
        connection
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
            .optional()
            .map(|_| connection)
    });
    match probe {
        Ok(connection) => Ok(connection),
        Err(error) if error.sqlite_error_code() == Some(ErrorCode::CannotOpen) => open_immutable(),
        Err(error) => Err(sqlite_message(&error, database_path)),
    }
}

/// Whether the file header marks a WAL database (format versions 2) and no `-shm` file exists.
fn is_wal_without_shm(database_path: &str) -> bool {
    use std::io::Read;
    let mut header = [0u8; 20];
    let read = std::fs::File::open(database_path).and_then(|mut file| file.read_exact(&mut header));
    read.is_ok()
        && header.starts_with(b"SQLite format 3\0")
        && (header[18] == 2 || header[19] == 2)
        && !std::path::Path::new(&format!("{database_path}-shm")).exists()
}

/// SQLite's own message, as Bun reports it: rusqlite appends `: <path>` to open errors.
fn sqlite_message(error: &rusqlite::Error, path: &str) -> String {
    let message = error.to_string();
    match message
        .strip_suffix(path)
        .and_then(|rest| rest.strip_suffix(": "))
    {
        Some(rest) => rest.to_string(),
        None => message,
    }
}

/// Percent-encodes a path for a `file:` URI, leaving unreserved characters and `/`.
fn encode_sqlite_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Which OpenCode schemas a database holds. Legacy stores keep text in `part`
/// rows under `message` and `session`; v2 stores keep one JSON row per message in
/// `session_message` under `session_v2`. A migrated store can hold both; a session
/// with any `session_message` row is read from v2 only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCodeSchema {
    pub legacy: bool,
    pub v2: bool,
}

/// `openCodeSchema(database)`: errors when neither schema is present.
pub fn opencode_schema(database: &Connection) -> Result<OpenCodeSchema, String> {
    let mut statement = database
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|e| e.to_string())?;
    let tables: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(|rows| rows.collect())
        .map_err(|e| e.to_string())?;
    let has = |names: &[&str]| {
        names
            .iter()
            .all(|name| tables.iter().any(|table| table == name))
    };
    let schema = OpenCodeSchema {
        legacy: has(&["session", "message", "part"]),
        v2: has(&["session_v2", "session_message"]),
    };
    if !schema.legacy && !schema.v2 {
        return Err("unsupported OpenCode schema: expected a part or session_message table".into());
    }
    Ok(schema)
}

/// Joins a v2 message row to its visible text items: assistant rows yield one
/// row per `$.content` item (`item`), user rows one row with a null `item`.
pub const OPENCODE_V2_FROM: &str = "session_message sm
  LEFT JOIN json_each(CASE WHEN sm.type = 'assistant' THEN json_extract(sm.data, '$.content') END) item";
/// The visible text of an [`OPENCODE_V2_FROM`] row: user `$.text`, or an assistant item's text.
pub const OPENCODE_V2_TEXT: &str = "CASE WHEN sm.type = 'user' THEN json_extract(sm.data, '$.text')
  WHEN json_extract(item.value, '$.type') = 'text' THEN json_extract(item.value, '$.text') END";
/// Restricts v2 rows to user and assistant messages.
pub const OPENCODE_V2_VISIBLE: &str = "sm.type IN ('user', 'assistant')";

/// `legacyOnlyClause(schema, column)`: in a hybrid store, excludes legacy rows of
/// sessions that have moved to `session_message`.
pub fn legacy_only_clause(schema: OpenCodeSchema, session_column: &str) -> String {
    if schema.v2 {
        format!(
            "AND NOT EXISTS (SELECT 1 FROM session_message v2 WHERE v2.session_id = {session_column})"
        )
    } else {
        String::new()
    }
}

/// `openCodeSessionUsesV2`: whether a session is read from `session_message`.
pub fn opencode_session_uses_v2(
    database: &Connection,
    schema: OpenCodeSchema,
    session_id: &str,
) -> Result<bool, String> {
    if !schema.v2 {
        return Ok(false);
    }
    database
        .query_row(
            "SELECT 1 FROM session_message WHERE session_id = ?1 LIMIT 1",
            [session_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(|e| e.to_string())
}

/// `opencode://<encodeURI(db)>#<encodeURIComponent(session)>`. A Windows path
/// gets a leading `/` (`opencode:///C:%5C...`), as a `file:` URL would, so the
/// drive is not read as a host; its backslashes stay percent-encoded so the
/// path round-trips exactly.
pub fn opencode_locator(database_path: &str, session_id: &str) -> String {
    let slash = if is_windows_absolute(database_path) {
        "/"
    } else {
        ""
    };
    format!(
        "opencode://{slash}{}#{}",
        encode_uri(database_path),
        encode_uri_component(session_id)
    )
}

/// A parsed `opencode://` locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeLocator {
    pub database_path: String,
    pub session_id: String,
}

/// `parseOpenCodeLocator(locator)`, following `new URL()` for the parts the
/// TypeScript read: an empty host, a dot-normalized path without its query, and
/// the fragment. Errors with `invalid OpenCode locator: <locator>`.
pub fn parse_opencode_locator(locator: &str) -> Result<OpenCodeLocator, String> {
    let invalid = || format!("invalid OpenCode locator: {locator}");
    let rest = locator.strip_prefix("opencode://").ok_or_else(invalid)?;
    let (before, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let before = before.split_once('?').map_or(before, |(path, _)| path);
    let slash = before.find('/').unwrap_or(before.len());
    if slash > 0 {
        return Err(invalid());
    }
    let decoded = decode_uri(&normalize_url_path(before))?;
    let database_path = match decoded.strip_prefix('/') {
        Some(windows) if is_windows_absolute(windows) => windows.to_string(),
        _ => decoded,
    };
    let session_id = decode_uri_component(fragment)?;
    if !(database_path.starts_with('/') || is_windows_absolute(&database_path))
        || session_id.is_empty()
    {
        return Err(invalid());
    }
    Ok(OpenCodeLocator {
        database_path,
        session_id,
    })
}

/// Resolves `.` and `..` segments (including `%2e` spellings) as the URL parser does.
fn normalize_url_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let segments: Vec<&str> = path[1..].split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        let lower = segment.to_ascii_lowercase();
        if matches!(lower.as_str(), ".." | ".%2e" | "%2e." | "%2e%2e") {
            out.pop();
            if last {
                out.push("");
            }
        } else if matches!(lower.as_str(), "." | "%2e") {
            if last {
                out.push("");
            }
        } else {
            out.push(segment);
        }
    }
    format!("/{}", out.join("/"))
}

fn percent_encode(text: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || keep.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// JavaScript `encodeURI`.
pub fn encode_uri(text: &str) -> String {
    percent_encode(text, b";,/?:@&=+$-_.!~*'()#")
}

/// JavaScript `encodeURIComponent`.
pub fn encode_uri_component(text: &str) -> String {
    percent_encode(text, b"-_.!~*'()")
}

/// JavaScript `decodeURI`: escapes of `;/?:@&=+$,#` stay encoded.
pub fn decode_uri(text: &str) -> Result<String, String> {
    percent_decode(text, b";/?:@&=+$,#")
}

/// JavaScript `decodeURIComponent`.
pub fn decode_uri_component(text: &str) -> Result<String, String> {
    percent_decode(text, b"")
}

fn percent_decode(text: &str, reserved: &[u8]) -> Result<String, String> {
    let malformed = || "URI malformed".to_string();
    let bytes = text.as_bytes();
    let hex = |at: usize| -> Option<u8> {
        let digits = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
        (bytes[at] == b'%')
            .then(|| u8::from_str_radix(digits, 16).ok())
            .flatten()
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            let ch = text[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let first = hex(i).ok_or_else(malformed)?;
        if first < 0x80 {
            if reserved.contains(&first) {
                out.push_str(&text[i..i + 3]);
            } else {
                out.push(first as char);
            }
            i += 3;
            continue;
        }
        let length = match first {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return Err(malformed()),
        };
        let mut sequence = vec![first];
        for k in 1..length {
            let byte = hex(i + 3 * k).ok_or_else(malformed)?;
            if byte & 0xC0 != 0x80 {
                return Err(malformed());
            }
            sequence.push(byte);
        }
        out.push_str(std::str::from_utf8(&sequence).map_err(|_| malformed())?);
        i += 3 * length;
    }
    Ok(out)
}

/// `new Date(ms).toISOString().slice(0, 10)`: the UTC date of an epoch-millisecond
/// time. Errors with `Invalid time value` outside JavaScript's date range.
pub fn iso_date_from_millis(ms: f64) -> Result<String, String> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return Err("Invalid time value".into());
    }
    let days = (ms.trunc() / 86_400_000.0).floor() as i64;
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
    let iso = if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!(
            "{}{:06}-{month:02}-{day:02}",
            if year < 0 { "-" } else { "+" },
            year.abs()
        )
    };
    Ok(iso[..10].to_string())
}

/// A column as JavaScript would see it as text: SQLite TEXT, or a number's digits.
fn text_of(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Integer(number) => Some(number.to_string()),
        ValueRef::Real(number) => Some(js::number_to_string(number)),
        ValueRef::Null | ValueRef::Blob(_) => None,
    }
}

/// `value || fallback` for a text column.
fn text_or(value: ValueRef<'_>, fallback: &str) -> String {
    text_of(value)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

struct SearchRow {
    session_id: String,
    directory: String,
    title: String,
    time_updated: f64,
    role: Option<String>,
    text: String,
}

fn search_rows(database: &Connection, sql: &str, query: &str) -> Result<Vec<SearchRow>, String> {
    let mut statement = database.prepare(sql).map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([query], |row| {
            Ok(SearchRow {
                session_id: text_of(row.get_ref(0)?).unwrap_or_default(),
                directory: text_of(row.get_ref(1)?).unwrap_or_default(),
                title: text_of(row.get_ref(2)?).unwrap_or_default(),
                time_updated: match row.get_ref(3)? {
                    ValueRef::Integer(number) => number as f64,
                    ValueRef::Real(number) if !number.is_nan() => number,
                    ValueRef::Text(text) => std::str::from_utf8(text)
                        .ok()
                        .and_then(|t| t.trim().parse().ok())
                        .unwrap_or(f64::NAN),
                    _ => 0.0,
                },
                role: text_of(row.get_ref(4)?),
                text: text_of(row.get_ref(5)?).unwrap_or_default(),
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

/// `searchOpenCodeStore(query, databasePath, limit, snippetsPerSession)`: case-insensitive
/// literal search over visible text, one match per session ranked by count then date.
pub fn search_opencode_store(
    query: &str,
    database_path: &str,
    limit: usize,
    snippets_per_session: usize,
) -> Result<Vec<StoreSearchMatch>, String> {
    let database = open_opencode_database(database_path)?;
    let schema = opencode_schema(&database)?;
    let mut rows = Vec::new();
    if schema.v2 {
        let sql = format!(
            "SELECT * FROM (
          SELECT s.id AS session_id, s.directory, s.title, s.time_updated, sm.type AS role, {OPENCODE_V2_TEXT} AS text
          FROM {OPENCODE_V2_FROM}
          JOIN session_v2 s ON s.id = sm.session_id
          WHERE {OPENCODE_V2_VISIBLE}
        ) WHERE instr(lower(text), lower(?1)) > 0"
        );
        rows.extend(search_rows(&database, &sql, query)?);
    }
    if schema.legacy {
        let sql = format!(
            "SELECT s.id AS session_id, s.directory, s.title, s.time_updated,
               json_extract(m.data, '$.role') AS role,
               json_extract(p.data, '$.text') AS text
        FROM part p
        JOIN message m ON m.id = p.message_id
        JOIN session s ON s.id = p.session_id
        WHERE json_extract(p.data, '$.type') = 'text'
          AND instr(lower(json_extract(p.data, '$.text')), lower(?1)) > 0
          {}",
            legacy_only_clause(schema, "p.session_id")
        );
        rows.extend(search_rows(&database, &sql, query)?);
    }
    // JavaScript's `b - a` comparator; a NaN time sorts as 0 so the order stays total.
    let time = |row: &SearchRow| {
        if row.time_updated.is_nan() {
            0.0
        } else {
            row.time_updated
        }
    };
    rows.sort_by(|a, b| time(b).total_cmp(&time(a)));
    let mut matches: Vec<StoreSearchMatch> = Vec::new();
    let mut by_session: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let index = match by_session.get(&row.session_id) {
            Some(&index) => index,
            None => {
                let project = [row.directory.as_str(), row.title.as_str()]
                    .into_iter()
                    .find(|text| !text.is_empty())
                    .unwrap_or("~");
                matches.push(StoreSearchMatch {
                    source: TranscriptSource::Opencode,
                    path: opencode_locator(database_path, &row.session_id),
                    count: 0,
                    date: iso_date_from_millis(row.time_updated)?,
                    project: compact_home(project).to_string(),
                    snippets: Vec::new(),
                });
                by_session.insert(row.session_id.clone(), matches.len() - 1);
                matches.len() - 1
            }
        };
        let entry = &mut matches[index];
        entry.count += count_occurrences(&row.text, query);
        if entry.snippets.len() < snippets_per_session {
            let role = row
                .role
                .filter(|role| !role.is_empty())
                .unwrap_or_else(|| "unknown".into());
            entry.snippets.push(TranscriptSnippet {
                role,
                text: snippet_around(&row.text, query),
            });
        }
    }
    matches.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| b.date.cmp(&a.date)));
    matches.truncate(limit);
    Ok(matches)
}

/// A legacy `part.data` row: only `type` and `text` matter.
#[derive(Default)]
struct PartData {
    kind: JsStr,
    text: JsStr,
}

impl LenientFields for PartData {
    const NAMES: &'static [&'static str] = &["type", "text"];
    fn set<'de, A: MapAccess<'de>>(&mut self, field: usize, map: &mut A) -> Result<(), A::Error> {
        match field {
            0 => self.kind = map.next_value()?,
            _ => self.text = map.next_value()?,
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for PartData {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_lenient(d)
    }
}

/// `loadOpenCodeMessages(locator)`: a session's user and assistant text in order,
/// one message per OpenCode message id.
pub fn load_opencode_messages(locator: &str) -> Result<Vec<RecallMessage>, String> {
    let OpenCodeLocator {
        database_path,
        session_id,
    } = parse_opencode_locator(locator)?;
    let database = open_opencode_database(&database_path)?;
    let schema = opencode_schema(&database)?;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    if opencode_session_uses_v2(&database, schema, &session_id)? {
        let sql = format!(
            "SELECT sm.id AS message_id, sm.type AS role, {OPENCODE_V2_TEXT} AS text
        FROM {OPENCODE_V2_FROM}
        WHERE sm.session_id = ?1 AND {OPENCODE_V2_VISIBLE}
        ORDER BY sm.seq, item.key"
        );
        let mut statement = database.prepare(&sql).map_err(|e| e.to_string())?;
        let mapped = statement
            .query_map([&session_id], |row| {
                let text = match row.get_ref(2)? {
                    ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                    _ => None,
                };
                let (id, role) = (
                    text_or(row.get_ref(0)?, "null"),
                    text_or(row.get_ref(1)?, "unknown"),
                );
                Ok(text.map(|text| (id, role, text)))
            })
            .map_err(|e| e.to_string())?;
        for row in mapped {
            rows.extend(row.map_err(|e| e.to_string())?);
        }
    } else if schema.legacy {
        let mut statement = database
            .prepare(
                "SELECT m.id AS message_id, json_extract(m.data, '$.role') AS role, p.data AS part_data
      FROM message m
      JOIN part p ON p.message_id = m.id
      WHERE m.session_id = ?1
      ORDER BY m.time_created, m.id, p.time_created, p.id",
            )
            .map_err(|e| e.to_string())?;
        let mapped = statement
            .query_map([&session_id], |row| {
                let part =
                    text_of(row.get_ref(2)?).and_then(|data| parse_json_line::<PartData>(&data));
                let text = part
                    .filter(|part| part.kind.0.as_deref() == Some("text"))
                    .and_then(|part| part.text.0);
                let (id, role) = (
                    text_or(row.get_ref(0)?, "null"),
                    text_or(row.get_ref(1)?, "unknown"),
                );
                Ok(text.map(|text| (id, role, text)))
            })
            .map_err(|e| e.to_string())?;
        for row in mapped {
            rows.extend(row.map_err(|e| e.to_string())?);
        }
    }
    Ok(group_messages(rows))
}

/// `groupMessages`: one message per id in first-seen order; the first row's role wins.
fn group_messages(rows: Vec<(String, String, String)>) -> Vec<RecallMessage> {
    let mut messages: Vec<RecallMessage> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (id, role, text) in rows {
        let index = *by_id.entry(id).or_insert_with(|| {
            messages.push(RecallMessage {
                role,
                content: Vec::new(),
            });
            messages.len() - 1
        });
        messages[index].content.push(RecallBlock::Text { text });
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_db(name: &str) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("dejavu-opencode-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("store.db").to_str().unwrap().to_string()
    }

    fn cleanup(path: &str) {
        let _ = std::fs::remove_dir_all(std::path::Path::new(path).parent().unwrap());
    }

    const AUG_4: i64 = 1_785_801_600_000; // Date.UTC(2026, 7, 4)
    const SEP_1: i64 = 1_788_220_800_000; // Date.UTC(2026, 8, 1)

    fn create_legacy_tables(db: &Connection) {
        db.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
    }

    fn create_v2_tables(db: &Connection) {
        db.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT, time_updated INTEGER NOT NULL);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
    }

    fn insert_v2_messages(db: &Connection, session_id: &str) {
        let rows = [
            (
                "msg_v1",
                "user",
                json!({ "text": "Needle question", "files": [] }),
            ),
            (
                "msg_v2",
                "assistant",
                json!({ "content": [
                { "type": "reasoning", "text": "Needle hidden" },
                { "type": "text", "text": "Needle answer" },
                { "type": "tool", "tool": "bash", "state": { "output": "needle tool" } },
                { "type": "text", "text": "Needle follow-up" },
            ] }),
            ),
            ("msg_v3", "synthetic", json!({ "text": "Needle synthetic" })),
            (
                "msg_v4",
                "compaction",
                json!({ "summary": "Needle summary" }),
            ),
        ];
        for (seq, (id, kind, data)) in rows.iter().enumerate() {
            db.execute(
                "INSERT INTO session_message VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    id,
                    session_id,
                    kind,
                    seq as i64,
                    seq as i64,
                    seq as i64,
                    data.to_string()
                ],
            )
            .unwrap();
        }
    }

    fn legacy_store() -> String {
        let path = temp_db("legacy");
        let db = Connection::open(&path).unwrap();
        create_legacy_tables(&db);
        db.execute(
            "INSERT INTO session VALUES (?, ?, ?, ?)",
            rusqlite::params!["ses_1", "/work/demo", "Demo", AUG_4],
        )
        .unwrap();
        for (id, time, role) in [("msg_1", 1, "user"), ("msg_2", 2, "assistant")] {
            db.execute(
                "INSERT INTO message VALUES (?, ?, ?, ?)",
                rusqlite::params![id, "ses_1", time, json!({ "role": role }).to_string()],
            )
            .unwrap();
        }
        for (id, message, time, kind, text) in [
            ("part_1", "msg_1", 1, "text", "Needle question"),
            ("part_2", "msg_2", 2, "text", "Needle answer"),
            ("part_3", "msg_2", 3, "reasoning", "Needle hidden"),
        ] {
            db.execute(
                "INSERT INTO part VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    id,
                    message,
                    "ses_1",
                    time,
                    json!({ "type": kind, "text": text }).to_string()
                ],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn searches_visible_text_parts_and_returns_a_queryable_locator() {
        let path = legacy_store();
        let matches = search_opencode_store("needle", &path, 10, 3).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            (
                matches[0].source,
                matches[0].count,
                matches[0].project.as_str()
            ),
            (TranscriptSource::Opencode, 2, "/work/demo")
        );
        assert_eq!(matches[0].date, "2026-08-04");
        assert_eq!(
            parse_opencode_locator(&matches[0].path).unwrap(),
            OpenCodeLocator {
                database_path: path.clone(),
                session_id: "ses_1".into()
            }
        );
        let messages = load_opencode_messages(&opencode_locator(&path, "ses_1")).unwrap();
        let pairs: Vec<(&str, &str)> = messages
            .iter()
            .map(|m| (m.role.as_str(), m.content[0].text().unwrap()))
            .collect();
        assert_eq!(
            pairs,
            [("user", "Needle question"), ("assistant", "Needle answer")]
        );
        cleanup(&path);
    }

    #[test]
    fn searches_and_loads_visible_text_from_a_v2_store() {
        let path = temp_db("v2");
        let db = Connection::open(&path).unwrap();
        create_v2_tables(&db);
        db.execute(
            "INSERT INTO session_v2 VALUES (?, ?, ?, ?)",
            rusqlite::params!["ses_v2", "/work/next", "Next", SEP_1],
        )
        .unwrap();
        insert_v2_messages(&db, "ses_v2");
        drop(db);
        let matches = search_opencode_store("needle", &path, 10, 5).unwrap();
        assert_eq!(matches.len(), 1);
        let found = &matches[0];
        assert_eq!(
            (found.count, found.project.as_str(), found.date.as_str()),
            (3, "/work/next", "2026-09-01")
        );
        let mut snippets: Vec<(&str, &str)> = found
            .snippets
            .iter()
            .map(|s| (s.role.as_str(), s.text.as_str()))
            .collect();
        snippets.sort();
        assert_eq!(
            snippets,
            [
                ("assistant", "Needle answer"),
                ("assistant", "Needle follow-up"),
                ("user", "Needle question")
            ]
        );
        assert_eq!(
            parse_opencode_locator(&found.path).unwrap().session_id,
            "ses_v2"
        );
        let messages = load_opencode_messages(&found.path).unwrap();
        let shape: Vec<(&str, Vec<&str>)> = messages
            .iter()
            .map(|m| {
                (
                    m.role.as_str(),
                    m.content.iter().filter_map(RecallBlock::text).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("user", vec!["Needle question"]),
                ("assistant", vec!["Needle answer", "Needle follow-up"])
            ]
        );
        cleanup(&path);
    }

    #[test]
    fn reads_each_session_of_a_hybrid_store_from_one_schema_only() {
        let path = temp_db("hybrid");
        let db = Connection::open(&path).unwrap();
        create_legacy_tables(&db);
        create_v2_tables(&db);
        for id in ["ses_old", "ses_moved"] {
            db.execute(
                "INSERT INTO session VALUES (?, ?, ?, ?)",
                rusqlite::params![id, format!("/work/{id}"), id, AUG_4],
            )
            .unwrap();
            db.execute(
                "INSERT INTO message VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    format!("msg_{id}"),
                    id,
                    1,
                    json!({ "role": "user" }).to_string()
                ],
            )
            .unwrap();
            db.execute(
                "INSERT INTO part VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    format!("part_{id}"),
                    format!("msg_{id}"),
                    id,
                    1,
                    json!({ "type": "text", "text": "Needle legacy" }).to_string()
                ],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO session_v2 VALUES (?, ?, ?, ?)",
            rusqlite::params!["ses_moved", "/work/ses_moved", "Moved", SEP_1],
        )
        .unwrap();
        insert_v2_messages(&db, "ses_moved");
        drop(db);
        let matches = search_opencode_store("needle", &path, 10, 5).unwrap();
        let mut by_session: Vec<(String, usize)> = matches
            .iter()
            .map(|m| (parse_opencode_locator(&m.path).unwrap().session_id, m.count))
            .collect();
        by_session.sort();
        assert_eq!(
            by_session,
            [("ses_moved".to_string(), 3), ("ses_old".to_string(), 1)]
        );
        let old = load_opencode_messages(&opencode_locator(&path, "ses_old")).unwrap();
        assert_eq!(
            old.iter()
                .map(|m| m.content[0].text().unwrap())
                .collect::<Vec<_>>(),
            ["Needle legacy"]
        );
        assert_eq!(
            load_opencode_messages(&opencode_locator(&path, "ses_moved"))
                .unwrap()
                .len(),
            2
        );
        cleanup(&path);
    }

    #[test]
    fn names_an_unsupported_opencode_schema() {
        let path = temp_db("empty");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE other (x)")
            .unwrap();
        let error = search_opencode_store("needle", &path, 10, 3).unwrap_err();
        assert!(error.contains("unsupported OpenCode schema"), "{error}");
        cleanup(&path);
    }

    #[test]
    fn reads_a_wal_store_that_has_no_shm_file_without_changing_it() {
        let path = legacy_store();
        {
            let db = Connection::open(&path).unwrap();
            let mode: String = db
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        let shm = format!("{path}-shm");
        assert!(!std::path::Path::new(&shm).exists());
        let before = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let matches = search_opencode_store("needle", &path, 10, 3).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(!std::path::Path::new(&shm).exists());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        cleanup(&path);
    }

    #[test]
    fn reads_wal_rows_a_live_writer_has_not_checkpointed() {
        let path = legacy_store();
        let writer = Connection::open(&path).unwrap();
        writer
            .query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
            .unwrap();
        writer
            .execute(
                "UPDATE part SET data = ? WHERE id = 'part_1'",
                [json!({ "type": "text", "text": "fresh needle" }).to_string()],
            )
            .unwrap();
        assert!(std::path::Path::new(&format!("{path}-shm")).exists());
        let messages = load_opencode_messages(&opencode_locator(&path, "ses_1")).unwrap();
        assert_eq!(messages[0].content[0].text(), Some("fresh needle"));
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn reports_a_missing_database() {
        let error =
            search_opencode_store("x", "/nonexistent-dejavu/opencode.db", 10, 3).unwrap_err();
        assert_eq!(error, "unable to open database file");
    }

    #[test]
    fn locators_round_trip_like_url_parsing() {
        let path = "/Users/dev/My Data/ü/opencode.db";
        let locator = opencode_locator(path, "ses/1 a");
        assert_eq!(
            locator,
            "opencode:///Users/dev/My%20Data/%C3%BC/opencode.db#ses%2F1%20a"
        );
        assert_eq!(
            parse_opencode_locator(&locator).unwrap(),
            OpenCodeLocator {
                database_path: path.into(),
                session_id: "ses/1 a".into()
            }
        );
        let parsed = parse_opencode_locator("opencode:///a/./b/../c.db?x=1#s").unwrap();
        assert_eq!(parsed.database_path, "/a/c.db");
        assert_eq!(
            parse_opencode_locator("opencode:///a/x%23y.db#s")
                .unwrap()
                .database_path,
            "/a/x%23y.db"
        );
        for bad in [
            "opencode:///a.db",
            "opencode:///a.db#",
            "opencode://host/a.db#s",
            "/a.db#s",
        ] {
            assert_eq!(
                parse_opencode_locator(bad).unwrap_err(),
                format!("invalid OpenCode locator: {bad}")
            );
        }
        assert_eq!(
            parse_opencode_locator("opencode:///a%ZZ.db#s").unwrap_err(),
            "URI malformed"
        );
        assert_eq!(decode_uri_component("%F0%9F%98%80").unwrap(), "😀");
        assert!(decode_uri_component("%ED%A0%80").is_err());
    }

    #[test]
    fn windows_database_paths_round_trip_through_locators() {
        for path in [
            r"C:\Users\dev\AppData\Local\Temp\x/opencode.db",
            "D:/data/opencode/opencode.db",
            r"\\nas\share\opencode.db",
        ] {
            let locator = opencode_locator(path, "ses_1");
            assert!(locator.starts_with("opencode:///"), "{locator}");
            assert_eq!(
                parse_opencode_locator(&locator).unwrap(),
                OpenCodeLocator {
                    database_path: path.into(),
                    session_id: "ses_1".into()
                }
            );
        }
        assert_eq!(
            opencode_locator(r"C:\a b\o.db", "s"),
            "opencode:///C:%5Ca%20b%5Co.db#s"
        );
        assert!(parse_opencode_locator(r"opencode://C:%5Ca.db#s").is_err());
    }

    #[test]
    fn formats_utc_dates_like_to_iso_string() {
        assert_eq!(iso_date_from_millis(AUG_4 as f64).unwrap(), "2026-08-04");
        assert_eq!(iso_date_from_millis(0.0).unwrap(), "1970-01-01");
        assert_eq!(iso_date_from_millis(-1.0).unwrap(), "1969-12-31");
        assert_eq!(
            iso_date_from_millis(951_782_400_000.0).unwrap(),
            "2000-02-29"
        );
        assert_eq!(iso_date_from_millis(8.64e15).unwrap(), "+275760-09");
        assert_eq!(
            iso_date_from_millis(f64::NAN).unwrap_err(),
            "Invalid time value"
        );
    }
}
