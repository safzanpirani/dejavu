//! Agents that keep transcripts in SQLite rather than JSONL files: OpenClaw
//! (`~/.openclaw/agents/<agent>/agent/openclaw-agent.sqlite`) and Hermes
//! (`~/.hermes/state.db`). Each session is addressed as
//! `openclaw://<db>#<session>` or `hermes://<db>#<session>`, encoded like
//! OpenCode locators, and rendered as Pi-format JSONL text. The Pi reader,
//! the index, search, and every view then handle both sources unchanged.
//!
//! OpenClaw already stores Pi entries, one per `transcript_events` row; Node
//! compresses large rows with plain zstd frames. Hermes keeps OpenAI-style
//! chat rows, which are mapped onto Pi `message` entries. Databases open
//! read-only, and only transcript tables are queried: OpenClaw keeps auth
//! profiles in the same file, and those tables are never read.

use crate::opencode::{decode_uri_component, encode_uri_component, open_opencode_database};
use crate::paths::is_windows_absolute;
use crate::types::TranscriptSource;
use crate::view::iso_from_millis;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};
use std::io::Read;

/// One session as the index and the scanners see it. `size` and `mtime_ms`
/// change whenever the session gains events, so an unchanged pair skips work.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualFile {
    pub path: String,
    pub size: u64,
    pub mtime_ms: f64,
}

/// Whether `source` stores transcripts in SQLite sessions rendered here.
pub fn is_virtual_source(source: TranscriptSource) -> bool {
    matches!(
        source,
        TranscriptSource::Openclaw | TranscriptSource::Hermes
    )
}

fn scheme(source: TranscriptSource) -> &'static str {
    match source {
        TranscriptSource::Openclaw => "openclaw",
        _ => "hermes",
    }
}

/// The source an `openclaw://` or `hermes://` locator names.
pub fn source_of(locator: &str) -> Option<TranscriptSource> {
    if locator.starts_with("openclaw://") {
        Some(TranscriptSource::Openclaw)
    } else if locator.starts_with("hermes://") {
        Some(TranscriptSource::Hermes)
    } else {
        None
    }
}

/// Whether `path` is a session locator rather than a file.
pub fn is_virtual_locator(path: &str) -> bool {
    source_of(path).is_some()
}

/// `<scheme>://<db>#<session>`. Both parts are percent-encoded except `/`
/// and `:` in the path, so a `#` in a directory name cannot end the path. A
/// Windows path gets a leading `/`, as a `file:` URL would.
pub fn locator(source: TranscriptSource, database_path: &str, session_id: &str) -> String {
    let slash = if is_windows_absolute(database_path) {
        "/"
    } else {
        ""
    };
    let path = encode_uri_component(database_path)
        .replace("%2F", "/")
        .replace("%3A", ":");
    format!(
        "{}://{slash}{path}#{}",
        scheme(source),
        encode_uri_component(session_id)
    )
}

/// The database path and session ID a locator names.
pub fn parse_locator(locator: &str) -> Result<(TranscriptSource, String, String), String> {
    let invalid = || format!("invalid session locator: {locator}");
    let source = source_of(locator).ok_or_else(invalid)?;
    let rest = &locator[scheme(source).len() + 3..];
    let (path, id) = rest.rsplit_once('#').ok_or_else(invalid)?;
    let path = decode_uri_component(path).map_err(|_| invalid())?;
    let path = match path.strip_prefix('/') {
        Some(windows) if is_windows_absolute(windows) => windows.to_string(),
        _ => path,
    };
    let id = decode_uri_component(id).map_err(|_| invalid())?;
    if id.is_empty() || !(path.starts_with('/') || is_windows_absolute(&path)) {
        return Err(invalid());
    }
    Ok((source, path, id))
}

/// The session ID of a locator, for resume commands.
pub fn session_id(locator: &str) -> Option<String> {
    parse_locator(locator).ok().map(|(_, _, id)| id)
}

/// The OpenClaw session key (`agent:main:telegram:direct:…`) a locator's
/// session belongs to; `openclaw resume` takes the key, not the ID.
pub fn openclaw_session_key(locator: &str) -> Option<String> {
    let (source, database_path, session_id) = parse_locator(locator).ok()?;
    if source != TranscriptSource::Openclaw {
        return None;
    }
    let db = open_opencode_database(&database_path).ok()?;
    db.query_row(
        "SELECT session_key FROM session_windows WHERE session_id = ?",
        [&session_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Every session in one database that has at least one transcript row.
pub fn list(source: TranscriptSource, database_path: &str) -> Result<Vec<VirtualFile>, String> {
    let db = open_opencode_database(database_path)?;
    let sql = match source {
        TranscriptSource::Openclaw => {
            "SELECT session_id,
                    SUM(COALESCE(event_utf8_bytes, LENGTH(event_json), 0)) + COUNT(*),
                    MAX(created_at)
             FROM transcript_events GROUP BY session_id"
        }
        _ => {
            "SELECT session_id,
                    SUM(LENGTH(COALESCE(content, '')) + LENGTH(COALESCE(tool_calls, ''))) + COUNT(*),
                    MAX(timestamp) * 1000
             FROM messages GROUP BY session_id"
        }
    };
    let mut statement = db.prepare(sql).map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let size: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
            let mtime: f64 = row.get::<_, Option<f64>>(2)?.unwrap_or(0.0);
            Ok((id, size, mtime))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|(id, size, mtime)| VirtualFile {
            path: locator(source, database_path, &id),
            size: size.max(0) as u64,
            mtime_ms: mtime.trunc(),
        })
        .collect())
}

/// The session as Pi-format JSONL, one entry per line, newline-terminated.
pub fn render(locator: &str) -> Result<String, String> {
    let (source, database_path, session_id) = parse_locator(locator)?;
    let db = open_opencode_database(&database_path)?;
    let lines = match source {
        TranscriptSource::Openclaw => openclaw_lines(&db, &session_id)?,
        _ => hermes_lines(&db, &session_id)?,
    };
    if lines.is_empty() {
        return Err(format!("ENOENT: no such session, open '{locator}'"));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

fn openclaw_lines(db: &Connection, session_id: &str) -> Result<Vec<String>, String> {
    let mut statement = db
        .prepare("SELECT event_json, event_zstd FROM transcript_events WHERE session_id = ? ORDER BY seq")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
            ))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .filter_map(|(text, compressed)| match (text, compressed) {
            (Some(text), _) => Some(text),
            // A frame that fails to decode drops only its own entry.
            (None, Some(bytes)) => decompress(&bytes),
            (None, None) => None,
        })
        // An entry is one line; JSON text never holds a raw newline.
        .map(|line| line.replace('\n', " "))
        .collect())
}

fn decompress(bytes: &[u8]) -> Option<String> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes).ok()?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    String::from_utf8(out).ok()
}

/// The columns a Hermes table has, so older and newer schemas both read.
fn columns(db: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = db
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .and_then(|rows| rows.collect())
        .map_err(|e| e.to_string())
}

fn select_list(have: &[String], wanted: &[&str]) -> String {
    wanted
        .iter()
        .map(|name| {
            if have.iter().any(|c| c == name) {
                (*name).to_string()
            } else {
                format!("NULL AS {name}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn iso(seconds: Option<f64>) -> Value {
    seconds
        .and_then(|s| iso_from_millis(s * 1000.0).ok())
        .map_or(Value::Null, Value::String)
}

fn hermes_lines(db: &Connection, session_id: &str) -> Result<Vec<String>, String> {
    let session_columns = columns(db, "sessions")?;
    let session = db
        .query_row(
            &format!(
                "SELECT {} FROM sessions WHERE id = ?",
                select_list(&session_columns, &["started_at", "cwd", "title"])
            ),
            [session_id],
            |row| {
                Ok((
                    row.get::<_, Option<f64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    if let Some((started, cwd, title)) = &session {
        let mut header = json!({ "type": "session", "version": 3, "id": session_id, "timestamp": iso(*started) });
        if let Some(cwd) = cwd.as_deref().filter(|c| !c.is_empty()) {
            header["cwd"] = json!(cwd);
        }
        lines.push(header.to_string());
        if let Some(title) = title.as_deref().filter(|t| !t.is_empty()) {
            lines.push(json!({ "type": "title", "title": title }).to_string());
        }
    }
    let message_columns = columns(db, "messages")?;
    let wanted = [
        "id",
        "role",
        "content",
        "tool_calls",
        "tool_call_id",
        "tool_name",
        "timestamp",
        "reasoning",
        "reasoning_content",
        "active",
    ];
    let mut statement = db
        .prepare(&format!(
            "SELECT {} FROM messages WHERE session_id = ? ORDER BY id",
            select_list(&message_columns, &wanted)
        ))
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok(HermesRow {
                id: row.get(0)?,
                role: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                content: row.get(2)?,
                tool_calls: row.get(3)?,
                tool_call_id: row.get(4)?,
                tool_name: row.get(5)?,
                timestamp: row.get(6)?,
                reasoning: row
                    .get::<_, Option<String>>(7)?
                    .or(row.get::<_, Option<String>>(8)?),
                active: row.get::<_, Option<i64>>(9)?.unwrap_or(1) != 0,
            })
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| e.to_string())?;
    let mut parent: Option<String> = None;
    for row in rows.into_iter().filter(|row| row.active) {
        let Some(message) = hermes_message(&row) else {
            continue;
        };
        let id = format!("m{}", row.id);
        lines.push(
            json!({
                "type": "message", "id": id, "parentId": parent,
                "timestamp": iso(row.timestamp), "message": message,
            })
            .to_string(),
        );
        parent = Some(id);
    }
    Ok(lines)
}

struct HermesRow {
    id: i64,
    role: String,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    reasoning: Option<String>,
    active: bool,
}

/// A Hermes row as a Pi message: user text, assistant thinking + text +
/// tool calls, or a tool result. Session metadata rows are skipped.
fn hermes_message(row: &HermesRow) -> Option<Value> {
    let text = row.content.as_deref().unwrap_or("");
    match row.role.as_str() {
        "user" => Some(json!({ "role": "user", "content": [{ "type": "text", "text": text }] })),
        "assistant" => {
            let mut content = Vec::new();
            if let Some(reasoning) = row.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                content.push(json!({ "type": "thinking", "thinking": reasoning }));
            }
            if !text.is_empty() {
                content.push(json!({ "type": "text", "text": text }));
            }
            content.extend(tool_calls(row.tool_calls.as_deref()));
            Some(json!({ "role": "assistant", "content": content }))
        }
        "tool" => Some(json!({
            "role": "toolResult",
            "toolCallId": row.tool_call_id,
            "toolName": row.tool_name,
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        })),
        _ => None,
    }
}

/// OpenAI `tool_calls` (`[{id, function: {name, arguments}}]`) as Pi
/// `toolCall` blocks with parsed arguments.
fn tool_calls(raw: Option<&str>) -> Vec<Value> {
    let Some(Value::Array(calls)) = raw.and_then(|r| serde_json::from_str::<Value>(r).ok()) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(Value::as_object)
        .map(|call| {
            let function = call.get("function").and_then(Value::as_object);
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let arguments = match function.and_then(|f| f.get("arguments")) {
                Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(json!({ "input": text })),
                Some(other) => other.clone(),
                None => Value::Object(Map::new()),
            };
            json!({ "type": "toolCall", "id": call.get("id"), "name": name, "arguments": arguments })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str, schema: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("dejavu-virtual-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.sqlite").to_string_lossy().into_owned();
        let db = Connection::open(&path).unwrap();
        db.execute_batch(schema).unwrap();
        path
    }

    #[test]
    fn locators_round_trip_with_spaces_and_hashes() {
        let path = "/home/u/my store#1/openclaw-agent.sqlite";
        let made = locator(TranscriptSource::Openclaw, path, "s 1");
        assert!(made.starts_with("openclaw://"));
        assert_eq!(
            parse_locator(&made).unwrap(),
            (
                TranscriptSource::Openclaw,
                path.to_string(),
                "s 1".to_string()
            )
        );
        assert_eq!(source_of("hermes:///x#y"), Some(TranscriptSource::Hermes));
        assert!(parse_locator("hermes://").is_err());
        let windows = locator(
            TranscriptSource::Hermes,
            r"C:\Users\me\.hermes\state.db",
            "h1",
        );
        assert!(windows.starts_with("hermes:///C:"));
        assert_eq!(
            parse_locator(&windows).unwrap().1,
            r"C:\Users\me\.hermes\state.db"
        );
        assert!(!is_virtual_locator("/x/a.jsonl"));
    }

    #[test]
    fn openclaw_sessions_render_their_events_in_order_and_decode_zstd_rows() {
        let path = temp_db(
            "openclaw",
            "CREATE TABLE transcript_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER,
                event_zstd BLOB, event_utf8_bytes INTEGER, navigation_json TEXT);",
        );
        let db = Connection::open(&path).unwrap();
        let header = r#"{"type":"session","version":4,"id":"s1","cwd":"/w"}"#;
        let message = r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        // A zstd frame of `message` made with `zstd -c`, stored raw (no compression), so the
        // test needs no encoder: magic, frame header, one raw last block.
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, message.len() as u8];
        let block_header = ((message.len() as u32) << 3) | 1;
        frame.extend_from_slice(&block_header.to_le_bytes()[..3]);
        frame.extend_from_slice(message.as_bytes());
        db.execute(
            "INSERT INTO transcript_events VALUES ('s1', 1, NULL, 2000, ?, ?, '{}')",
            rusqlite::params![frame, message.len() as i64],
        )
        .unwrap();
        db.execute(
            "INSERT INTO transcript_events VALUES ('s1', 0, ?, 1000, NULL, NULL, NULL)",
            [header],
        )
        .unwrap();
        drop(db);
        let files = list(TranscriptSource::Openclaw, &path).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mtime_ms, 2000.0);
        assert_eq!(
            render(&files[0].path).unwrap(),
            format!("{header}\n{message}\n")
        );
        assert!(render(&locator(TranscriptSource::Openclaw, &path, "missing")).is_err());
    }

    #[test]
    fn hermes_rows_become_a_linear_pi_transcript() {
        let path = temp_db(
            "hermes",
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, started_at REAL, cwd TEXT, title TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT,
                tool_calls TEXT, tool_call_id TEXT, tool_name TEXT, timestamp REAL, reasoning TEXT);
             INSERT INTO sessions VALUES ('h1', 1791188690.5, '/home/u', 'Greeting');
             INSERT INTO messages VALUES (1, 'h1', 'session_meta', '{}', NULL, NULL, NULL, 1791188690, NULL);
             INSERT INTO messages VALUES (2, 'h1', 'user', 'read notes', NULL, NULL, NULL, 1791188691, NULL);
             INSERT INTO messages VALUES (3, 'h1', 'assistant', '', '[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"n.txt\\\"}\"}}]', NULL, NULL, 1791188692, 'look first');
             INSERT INTO messages VALUES (4, 'h1', 'tool', 'beta', NULL, 'c1', 'read_file', 1791188693, NULL);
             INSERT INTO messages VALUES (5, 'h1', 'assistant', 'It says beta.', NULL, NULL, NULL, 1791188694, NULL);",
        );
        let files = list(TranscriptSource::Hermes, &path).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mtime_ms, 1791188694000.0);
        let text = render(&files[0].path).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows[0]["type"], "session");
        assert_eq!(rows[0]["cwd"], "/home/u");
        assert_eq!(rows[1], json!({ "type": "title", "title": "Greeting" }));
        assert_eq!(rows.len(), 6, "session_meta is skipped");
        assert_eq!(rows[2]["parentId"], Value::Null);
        assert_eq!(rows[3]["parentId"], "m2");
        assert_eq!(
            rows[3]["message"]["content"],
            json!([
                { "type": "thinking", "thinking": "look first" },
                { "type": "toolCall", "id": "c1", "name": "read_file", "arguments": { "path": "n.txt" } }
            ])
        );
        assert_eq!(rows[4]["message"]["role"], "toolResult");
        assert_eq!(rows[4]["message"]["toolCallId"], "c1");
        assert_eq!(rows[5]["message"]["content"][0]["text"], "It says beta.");
    }
}
