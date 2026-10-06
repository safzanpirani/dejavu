//! The transcript index (`transcript-index.ts`): one SQLite file with an FTS5
//! trigram table over every visible message, refreshed incrementally. The
//! schema, row contents, and refresh bookkeeping (file sizes, `mtimeMs`, the
//! `Bun.hash` of each file's head, OpenCode cursors) match the Bun CLI's, so
//! both binaries can share one index file.

use crate::js;
use crate::opencode::{
    OPENCODE_V2_FROM, OPENCODE_V2_TEXT, OPENCODE_V2_VISIBLE, iso_date_from_millis,
    legacy_only_clause, open_opencode_database, opencode_locator, opencode_schema,
};
use crate::paths::{
    compact_home, date_from_path, js_lower, project_from_transcript_path,
    project_from_transcript_text, snippet_around,
};
use crate::pool::map_pool;
use crate::reader::{extract_visible_message, fs_error};
use crate::scan::jsonl_files;
use crate::sources::home_dir;
use crate::types::{
    StoreDiagnostic, StoreKind, StoreSearchMatch, TranscriptSnippet, TranscriptSource,
    TranscriptStore,
};
use crate::wyhash::bun_hash;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

/// Bump when stored rows change, including how messages are extracted
/// (4: Droid skill notifications are cut from user text; 5: Droid and Codex
/// compaction summaries are indexed).
pub const SCHEMA_VERSION: i64 = 6;
/// Bytes hashed at the start of each JSONL file to detect in-place rewrites versus appends.
const HEAD_BYTES: u64 = 4096;
/// Changed files are parsed in parallel in batches of at most this many files
/// or bytes, and each batch is written in one transaction.
const BATCH_FILES: usize = 64;
const BATCH_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexRefreshResult {
    pub path: String,
    pub files: i64,
    pub messages: i64,
    pub indexed: usize,
    pub removed: usize,
    pub skipped: Vec<StoreDiagnostic>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFileMatch {
    pub store: String,
    pub source: TranscriptSource,
    pub path: String,
    pub count: usize,
}

/// A [`StoreSearchMatch`] with the store it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedStoreMatch {
    pub store: String,
    pub matched: StoreSearchMatch,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptIndexStatus {
    pub path: String,
    pub exists: bool,
    pub schema_version: i64,
    pub files: i64,
    pub messages: i64,
    pub bytes: u64,
}

/// `$DEJAVU_INDEX_PATH`, else `$XDG_CACHE_HOME/dejavu/transcripts.sqlite`, else
/// `~/.cache/dejavu/transcripts.sqlite`. A set variable wins even when empty.
pub fn default_index_path() -> String {
    if let Some(path) = std::env::var_os("DEJAVU_INDEX_PATH") {
        return path.to_string_lossy().into_owned();
    }
    let cache_root = match std::env::var_os("XDG_CACHE_HOME") {
        Some(root) => root.to_string_lossy().into_owned(),
        None => crate::paths::join(&[&home_dir(), ".cache"]),
    };
    crate::paths::join(&[&cache_root, "dejavu", "transcripts.sqlite"])
}

fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

fn stored_schema_version(database: &Connection) -> Result<i64, String> {
    let table: Option<String> = database
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if table.is_none() {
        return Ok(0);
    }
    let value: Option<SqlValue> = database
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    // `Number.parseInt(value, 10) || 0`.
    Ok(match value {
        Some(SqlValue::Integer(number)) => number,
        Some(SqlValue::Real(number)) => number.trunc() as i64,
        Some(SqlValue::Text(text)) => {
            let text = text.trim_start();
            let (sign, digits) = match text.strip_prefix('-') {
                Some(rest) => (-1, rest),
                None => (1, text.strip_prefix('+').unwrap_or(text)),
            };
            let end = digits
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(digits.len());
            digits[..end].parse::<i64>().map_or(0, |n| sign * n)
        }
        _ => 0,
    })
}

fn drop_schema(database: &Connection) -> Result<(), String> {
    for name in [
        "messages",
        "message_rows",
        "files",
        "opencode_cursors",
        "metadata",
    ] {
        database
            .execute(&format!("DROP TABLE IF EXISTS {name}"), [])
            .map_err(sql_error)?;
    }
    Ok(())
}

fn create_schema(database: &Connection) -> Result<(), String> {
    database
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE files (
    path TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    store TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    message_count INTEGER NOT NULL,
    indexed_bytes INTEGER NOT NULL,
    head_bytes INTEGER NOT NULL,
    head TEXT NOT NULL,
    project TEXT NOT NULL
  );
CREATE TABLE opencode_cursors (store TEXT PRIMARY KEY, time_updated INTEGER NOT NULL, part_id TEXT NOT NULL);
CREATE TABLE message_rows (
    id INTEGER PRIMARY KEY,
    store TEXT NOT NULL,
    path TEXT NOT NULL,
    source TEXT NOT NULL,
    role TEXT NOT NULL,
    date TEXT NOT NULL,
    project TEXT NOT NULL,
    key TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL
  );
CREATE INDEX message_rows_path ON message_rows (path);
CREATE INDEX message_rows_store_key ON message_rows (store, key);
CREATE VIRTUAL TABLE messages USING fts5(text, content='message_rows', content_rowid='id', tokenize='trigram');
CREATE TRIGGER message_rows_ai AFTER INSERT ON message_rows BEGIN
    INSERT INTO messages(rowid, text) VALUES (new.id, new.text);
  END;
CREATE TRIGGER message_rows_ad AFTER DELETE ON message_rows BEGIN
    INSERT INTO messages(messages, rowid, text) VALUES ('delete', old.id, old.text);
  END;",
        )
        .map_err(sql_error)?;
    database
        .execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', ?)",
            [SCHEMA_VERSION.to_string()],
        )
        .map_err(sql_error)?;
    Ok(())
}

/// `openIndex(path)`: a writable connection in WAL mode whose schema is
/// recreated when its version differs.
pub fn open_index(path: &str) -> Result<Connection, String> {
    let database = Connection::open(path).map_err(sql_error)?;
    database
        .busy_timeout(std::time::Duration::from_secs(10))
        .map_err(sql_error)?;
    database
        .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .map_err(sql_error)?;
    database
        .execute_batch("PRAGMA synchronous = NORMAL")
        .map_err(sql_error)?;
    if stored_schema_version(&database)? != SCHEMA_VERSION {
        drop_schema(&database)?;
        create_schema(&database)?;
    }
    Ok(database)
}

fn open_index_readonly(path: &str) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let database = Connection::open_with_flags(path, flags).map_err(sql_error)?;
    database
        .busy_timeout(std::time::Duration::from_secs(10))
        .map_err(sql_error)?;
    Ok(database)
}

/// Placeholders `?, ?, ...` for `count` values.
fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// `listIndexedSessions`: transcripts whose project contains `project` (home
/// compacted, without a leading `~/`, case-insensitive) with visible activity
/// on or after `since`, newest first, without exposing message bodies.
pub fn list_indexed_sessions(
    project: &str,
    since: Option<&str>,
    limit: usize,
    stores: &[TranscriptStore],
    path: &str,
) -> Result<(Vec<String>, usize), String> {
    if stores.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let database = open_index_readonly(path)?;
    let compacted = compact_home(project);
    let needle = js_lower(compacted.strip_prefix("~/").unwrap_or(compacted)).into_owned();
    let sql = format!(
        "SELECT path, MAX(date) AS latest FROM message_rows
      WHERE instr(lower(project), ?) > 0 AND store IN ({}) GROUP BY path HAVING MAX(date) >= ?
      ORDER BY latest DESC, path ASC",
        placeholders(stores.len())
    );
    let mut values: Vec<SqlValue> = vec![SqlValue::Text(needle)];
    values.extend(stores.iter().map(|s| SqlValue::Text(s.path.clone())));
    values.push(SqlValue::Text(since.unwrap_or("").to_string()));
    let mut statement = database.prepare(&sql).map_err(sql_error)?;
    let rows: Vec<String> = statement
        .query_map(params_from_iter(values), |row| row.get(0))
        .and_then(|rows| rows.collect())
        .map_err(sql_error)?;
    let total = rows.len();
    Ok((rows.into_iter().take(limit).collect(), total))
}

/// Which projects [`recent_sessions`] admits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectFilter {
    Any,
    /// The project contains this text.
    Contains(String),
    /// The project is this directory or one below it.
    Under(String),
}

/// One indexed session with its newest visible-message date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    pub path: String,
    pub source: TranscriptSource,
    pub date: String,
}

/// Indexed sessions with a visible message in a matching project since `since`,
/// newest first. Projects compare case-insensitively and home-compacted.
pub fn recent_sessions(
    filter: &ProjectFilter,
    since: Option<&str>,
    stores: &[TranscriptStore],
    path: &str,
) -> Result<Vec<RecentSession>, String> {
    if stores.is_empty() {
        return Ok(Vec::new());
    }
    let needle = |project: &str| {
        let compacted = compact_home(project);
        js_lower(compacted.strip_prefix("~/").unwrap_or(compacted))
            .trim_end_matches('/')
            .to_string()
    };
    let (clause, mut values) = match filter {
        ProjectFilter::Any => ("1".to_string(), Vec::new()),
        ProjectFilter::Contains(project) => (
            "instr(lower(project), ?) > 0".to_string(),
            vec![SqlValue::Text(needle(project))],
        ),
        ProjectFilter::Under(project) => {
            let under = needle(project);
            (
                "(lower(project) = ? OR substr(lower(project), 1, ?) = ?)".to_string(),
                vec![
                    SqlValue::Text(under.clone()),
                    SqlValue::Integer(under.chars().count() as i64 + 1),
                    SqlValue::Text(format!("{under}/")),
                ],
            )
        }
    };
    let database = open_index_readonly(path)?;
    let sql = format!(
        "SELECT path, source, MAX(date) AS latest FROM message_rows
      WHERE {clause} AND store IN ({}) GROUP BY path HAVING MAX(date) >= ?
      ORDER BY latest DESC, path ASC",
        placeholders(stores.len())
    );
    values.extend(stores.iter().map(|s| SqlValue::Text(s.path.clone())));
    values.push(SqlValue::Text(since.unwrap_or("").to_string()));
    let mut statement = database.prepare(&sql).map_err(sql_error)?;
    let rows: Vec<(String, String, String)> = statement
        .query_map(params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .and_then(|rows| rows.collect())
        .map_err(sql_error)?;
    Ok(rows
        .into_iter()
        .filter_map(|(path, source, date)| {
            Some(RecentSession {
                source: TranscriptSource::from_name(&source)?,
                path,
                date,
            })
        })
        .collect())
}

/// The indexed transcript whose path contains a session id, newest first.
pub fn path_for_session_id(
    id: &str,
    stores: &[TranscriptStore],
    path: &str,
) -> Result<Option<String>, String> {
    if stores.is_empty() || id.is_empty() {
        return Ok(None);
    }
    let database = open_index_readonly(path)?;
    let sql = format!(
        "SELECT path FROM message_rows WHERE instr(lower(path), ?) > 0 AND store IN ({})
      GROUP BY path ORDER BY MAX(date) DESC LIMIT 1",
        placeholders(stores.len())
    );
    let mut values = vec![SqlValue::Text(js_lower(id).into_owned())];
    values.extend(stores.iter().map(|s| SqlValue::Text(s.path.clone())));
    database
        .query_row(&sql, params_from_iter(values), |row| row.get(0))
        .optional()
        .map_err(sql_error)
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

/// `refreshTranscriptIndex(stores, path, rebuild)`: brings the index up to date
/// with every store. Unchanged files are skipped by size and `mtimeMs`; grown
/// files whose head is unchanged index only their new complete lines; other
/// changed files are reparsed. A store that fails is reported in `skipped`.
pub fn refresh_transcript_index(
    stores: &[TranscriptStore],
    path: &str,
    rebuild: bool,
    max_parallel: usize,
) -> Result<IndexRefreshResult, String> {
    let started = Instant::now();
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| fs_error(&error, "mkdir", &parent.to_string_lossy()))?;
    }
    let database = open_index(path)?;
    if rebuild {
        drop_schema(&database)?;
        create_schema(&database)?;
    }
    let mut indexed = 0;
    let mut removed = 0;
    let mut skipped = Vec::new();
    for store in stores {
        let outcome = match store.kind {
            StoreKind::Sqlite => refresh_opencode_store(&database, store),
            StoreKind::Jsonl => refresh_jsonl_store(&database, store, max_parallel),
        };
        match outcome {
            Ok((store_indexed, store_removed)) => {
                indexed += store_indexed;
                removed += store_removed;
            }
            Err(error) => skipped.push(StoreDiagnostic {
                source: store.source,
                path: store.path.clone(),
                error,
            }),
        }
    }
    if rebuild {
        database.execute_batch("VACUUM").map_err(sql_error)?;
    }
    let (files, messages) = totals(&database)?;
    Ok(IndexRefreshResult {
        path: path.to_string(),
        files,
        messages,
        indexed,
        removed,
        skipped,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn totals(database: &Connection) -> Result<(i64, i64), String> {
    database
        .query_row(
            "SELECT COUNT(*) AS files, (SELECT COUNT(*) FROM message_rows) AS messages FROM files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql_error)
}

#[derive(Debug, Clone)]
struct FileRow {
    path: String,
    source: String,
    size: i64,
    mtime_ms: f64,
    indexed_bytes: i64,
    head_bytes: i64,
    head: String,
    project: String,
}

/// `fs.stat(path).mtimeMs` as Bun computes it.
#[cfg(unix)]
fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime() as f64 * 1000.0 + metadata.mtime_nsec() as f64 / 1e6
}

/// `fs.stat(path).mtimeMs` from the modification time the platform reports.
#[cfg(not(unix))]
fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as f64 * 1000.0 + f64::from(d.subsec_nanos()) / 1e6)
        .unwrap_or(0.0)
}

/// A JavaScript number as SQLite stores it: integers as INTEGER, others as REAL.
fn js_number(value: f64) -> SqlValue {
    if value.fract() == 0.0 && value.abs() < 9.2e18 {
        SqlValue::Integer(value as i64)
    } else {
        SqlValue::Real(value)
    }
}

/// One visible message ready to insert.
struct Row {
    role: &'static str,
    date: String,
    project: String,
    text: String,
}

enum Parsed {
    Replace {
        size: u64,
        mtime: f64,
        head: String,
        head_bytes: u64,
        project: String,
        rows: Vec<Row>,
    },
    Append {
        previous: FileRow,
        size: u64,
        mtime: f64,
        consumed: u64,
        rows: Vec<Row>,
    },
}

/// `Bun.file(path).text()` over bytes: lossy UTF-8 without a leading BOM.
fn decode(bytes: Vec<u8>) -> String {
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    };
    match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

fn visible_rows(
    text: &str,
    source: TranscriptSource,
    file_project: &str,
    file_date: &str,
) -> Vec<Row> {
    text.split('\n')
        .filter_map(|line| extract_visible_message(line, source))
        .map(|message| Row {
            role: message.role,
            date: message.date.unwrap_or_else(|| file_date.to_string()),
            project: compact_home(message.project.as_deref().unwrap_or(file_project)).to_string(),
            text: message.text,
        })
        .collect()
}

fn read_range(path: &str, start: u64, limit: Option<u64>) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(path).map_err(|error| fs_error(&error, "open", path))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| fs_error(&error, "read", path))?;
    let mut bytes = Vec::new();
    match limit {
        Some(limit) => file.take(limit).read_to_end(&mut bytes),
        None => file.read_to_end(&mut bytes),
    }
    .map_err(|error| fs_error(&error, "read", path))?;
    Ok(bytes)
}

fn parse_file(
    path: &str,
    source: TranscriptSource,
    size: u64,
    mtime: f64,
    previous: Option<&FileRow>,
) -> Result<Parsed, String> {
    let file_date = date_from_path(path);
    if crate::virtual_store::is_virtual_locator(path) {
        // A rendered session has no stable byte offsets, so it is reparsed whole.
        let text = crate::virtual_store::render(path)?;
        let project = project_from_transcript_text(path, source, &text);
        let rows = visible_rows(&text, source, &project, &file_date);
        return Ok(Parsed::Replace {
            size,
            mtime,
            head: String::new(),
            head_bytes: 0,
            project,
            rows,
        });
    }
    if let Some(previous) = previous
        && size as i64 >= previous.size
        && previous.indexed_bytes <= size as i64
    {
        let head = if previous.head_bytes > 0 {
            bun_hash(&read_range(path, 0, Some(previous.head_bytes as u64))?)
        } else {
            String::new()
        };
        if head == previous.head {
            let tail = decode_tail(read_range(path, previous.indexed_bytes as u64, None)?);
            // Only consume through the last newline so a half-written trailing line is picked up next time.
            let complete = match tail.rfind('\n') {
                Some(end) => &tail[..=end],
                None => "",
            };
            let rows = visible_rows(complete, source, &previous.project, &file_date);
            return Ok(Parsed::Append {
                previous: previous.clone(),
                size,
                mtime,
                consumed: complete.len() as u64,
                rows,
            });
        }
    }
    let bytes = std::fs::read(path).map_err(|error| fs_error(&error, "open", path))?;
    let head_bytes = HEAD_BYTES.min(size);
    let head = if head_bytes > 0 {
        bun_hash(&bytes[..(head_bytes as usize).min(bytes.len())])
    } else {
        String::new()
    };
    let text = decode(bytes);
    let project = project_from_transcript_text(path, source, &text);
    let rows = visible_rows(&text, source, &project, &file_date);
    Ok(Parsed::Replace {
        size,
        mtime,
        head,
        head_bytes,
        project,
        rows,
    })
}

/// A file slice's `.text()`: lossy UTF-8 (a slice that starts mid-file keeps a BOM-like prefix only at offset 0).
fn decode_tail(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

struct Job {
    path: String,
    size: u64,
    mtime: f64,
    previous: Option<FileRow>,
}

fn refresh_jsonl_store(
    database: &Connection,
    store: &TranscriptStore,
    max_parallel: usize,
) -> Result<(usize, usize), String> {
    // SQLite sessions report their own size and change time; files are stat'ed.
    let virtual_files = if crate::virtual_store::is_virtual_source(store.source) {
        Some(crate::virtual_store::list(store.source, &store.path)?)
    } else {
        None
    };
    let live_paths = match (&virtual_files, store.source) {
        (Some(files), _) => files.iter().map(|file| file.path.clone()).collect(),
        (None, TranscriptSource::Agy) => crate::agy::transcript_files(&store.path)?,
        (None, _) => jsonl_files(&store.path)?,
    };
    let known_rows: Vec<FileRow> = {
        let mut statement = database
            .prepare(
                "SELECT path, source, size, mtime_ms, indexed_bytes, head_bytes, head, project FROM files WHERE store = ?",
            )
            .map_err(sql_error)?;
        statement
            .query_map([&store.path], |row| {
                Ok(FileRow {
                    path: row.get(0)?,
                    source: row.get(1)?,
                    size: row.get(2)?,
                    mtime_ms: row.get(3)?,
                    indexed_bytes: row.get(4)?,
                    head_bytes: row.get(5)?,
                    head: row.get(6)?,
                    project: row.get(7)?,
                })
            })
            .and_then(|rows| rows.collect())
            .map_err(sql_error)?
    };
    let known: HashMap<&str, &FileRow> = known_rows
        .iter()
        .map(|row| (row.path.as_str(), row))
        .collect();

    let mut jobs = Vec::new();
    for (index, file_path) in live_paths.iter().enumerate() {
        let (size, mtime) = match &virtual_files {
            Some(files) => (files[index].size, files[index].mtime_ms),
            None => {
                let metadata = std::fs::metadata(file_path)
                    .map_err(|error| fs_error(&error, "stat", file_path))?;
                (metadata.len(), mtime_ms(&metadata))
            }
        };
        let previous = known.get(file_path.as_str()).copied();
        if let Some(previous) = previous
            && previous.size == size as i64
            && previous.mtime_ms == mtime
        {
            continue;
        }
        jobs.push(Job {
            path: file_path.clone(),
            size,
            mtime,
            previous: previous.cloned(),
        });
    }

    let mut indexed = 0;
    let mut start = 0;
    while start < jobs.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < jobs.len() && end - start < BATCH_FILES && (end == start || bytes < BATCH_BYTES)
        {
            bytes += jobs[end].size;
            end += 1;
        }
        let batch = &jobs[start..end];
        let parsed = map_pool(batch, max_parallel, |job, _| {
            parse_file(
                &job.path,
                store.source,
                job.size,
                job.mtime,
                job.previous.as_ref(),
            )
        })?;
        // Files before a failure are written, as the TypeScript committed each file in turn.
        let failure = parsed.iter().position(Result::is_err);
        let ready = failure.unwrap_or(parsed.len());
        let mut results = parsed.into_iter();
        write_batch(
            database,
            store,
            batch[..ready]
                .iter()
                .zip(results.by_ref().take(ready).map(|r| r.ok().unwrap())),
        )?;
        indexed += ready;
        if failure.is_some() {
            return Err(results.next().and_then(Result::err).unwrap_or_default());
        }
        start = end;
    }

    let live: HashSet<&str> = live_paths.iter().map(String::as_str).collect();
    let gone: Vec<&str> = known_rows
        .iter()
        .map(|row| row.path.as_str())
        .filter(|path| !live.contains(path))
        .collect();
    for chunk in gone.chunks(BATCH_FILES) {
        let transaction = database.unchecked_transaction().map_err(sql_error)?;
        for file_path in chunk {
            transaction
                .execute("DELETE FROM message_rows WHERE path = ?", [file_path])
                .map_err(sql_error)?;
            transaction
                .execute("DELETE FROM files WHERE path = ?", [file_path])
                .map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)?;
    }
    Ok((indexed, gone.len()))
}

fn write_batch<'a>(
    database: &Connection,
    store: &TranscriptStore,
    items: impl Iterator<Item = (&'a Job, Parsed)>,
) -> Result<(), String> {
    let transaction = database.unchecked_transaction().map_err(sql_error)?;
    {
        let mut insert = transaction
            .prepare_cached(
                "INSERT INTO message_rows (store, path, source, role, date, project, text) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .map_err(sql_error)?;
        let mut upsert = transaction
            .prepare_cached(
                "INSERT OR REPLACE INTO files
    (path, source, store, size, mtime_ms, message_count, indexed_bytes, head_bytes, head, project)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .map_err(sql_error)?;
        let source = store.source.as_str();
        for (job, parsed) in items {
            let mut insert_rows = |rows: &[Row]| -> Result<(), String> {
                for row in rows {
                    insert
                        .execute(params![
                            store.path,
                            job.path,
                            source,
                            row.role,
                            row.date,
                            row.project,
                            row.text
                        ])
                        .map_err(sql_error)?;
                }
                Ok(())
            };
            match parsed {
                Parsed::Replace {
                    size,
                    mtime,
                    head,
                    head_bytes,
                    project,
                    rows,
                } => {
                    transaction
                        .execute("DELETE FROM message_rows WHERE path = ?", [&job.path])
                        .map_err(sql_error)?;
                    insert_rows(&rows)?;
                    upsert
                        .execute(params![
                            job.path,
                            source,
                            store.path,
                            size as i64,
                            js_number(mtime),
                            rows.len() as i64,
                            size as i64,
                            head_bytes as i64,
                            head,
                            project
                        ])
                        .map_err(sql_error)?;
                }
                Parsed::Append {
                    previous,
                    size,
                    mtime,
                    consumed,
                    rows,
                } => {
                    insert_rows(&rows)?;
                    let existing: i64 = transaction
                        .query_row(
                            "SELECT message_count FROM files WHERE path = ?",
                            [&previous.path],
                            |row| row.get(0),
                        )
                        .map_err(sql_error)?;
                    upsert
                        .execute(params![
                            previous.path,
                            previous.source,
                            store.path,
                            size as i64,
                            js_number(mtime),
                            existing + rows.len() as i64,
                            previous.indexed_bytes + consumed as i64,
                            previous.head_bytes,
                            previous.head,
                            previous.project
                        ])
                        .map_err(sql_error)?;
                }
            }
        }
    }
    transaction.commit().map_err(sql_error)
}

/// A column as JavaScript would concatenate it into a string key.
fn js_text(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        ValueRef::Integer(number) => number.to_string(),
        ValueRef::Real(number) => js::number_to_string(number),
        ValueRef::Null => "null".into(),
        ValueRef::Blob(_) => String::new(),
    }
}

fn js_number_of(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Integer(number) => number as f64,
        ValueRef::Real(number) => number,
        ValueRef::Text(text) => std::str::from_utf8(text)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(f64::NAN),
        ValueRef::Null => 0.0,
        ValueRef::Blob(_) => f64::NAN,
    }
}

struct OpenCodePartRow {
    part_id: String,
    time_updated: SqlValue,
    part_id_value: SqlValue,
    session_id: String,
    directory: String,
    title: String,
    session_updated: f64,
    role: String,
    text: String,
}

/// v2 rows are keyed by message id under this prefix. A session that first appears in session_message
/// drops its earlier legacy rows, which carry bare part ids.
const V2_KEY_PREFIX: &str = "v2:";

fn refresh_opencode_store(
    database: &Connection,
    store: &TranscriptStore,
) -> Result<(usize, usize), String> {
    let source = open_opencode_database(&store.path)?;
    let schema = opencode_schema(&source)?;
    let mut indexed = 0;
    if schema.v2 {
        let sql = format!(
            "SELECT sm.id AS part_id, sm.time_updated, s.id AS session_id, s.directory, s.title, s.time_updated AS session_updated,
             sm.type AS role, {OPENCODE_V2_TEXT} AS text
      FROM {OPENCODE_V2_FROM}
      JOIN session_v2 s ON s.id = sm.session_id
      WHERE (sm.time_updated > ?1 OR (sm.time_updated = ?1 AND sm.id > ?2)) AND {OPENCODE_V2_VISIBLE}
      ORDER BY sm.time_updated, sm.id, item.key"
        );
        indexed += refresh_opencode_rows(
            database,
            store,
            &format!("{}#session_message", store.path),
            V2_KEY_PREFIX,
            &source,
            &sql,
        )?;
    }
    // Text parts are re-read whenever OpenCode touches them, so streamed parts converge once they finish.
    if schema.legacy {
        let sql = format!(
            "SELECT p.id AS part_id, p.time_updated, s.id AS session_id, s.directory, s.title, s.time_updated AS session_updated,
             json_extract(m.data, '$.role') AS role, json_extract(p.data, '$.text') AS text
      FROM part p
      JOIN message m ON m.id = p.message_id
      JOIN session s ON s.id = p.session_id
      WHERE (p.time_updated > ?1 OR (p.time_updated = ?1 AND p.id > ?2))
        AND json_extract(p.data, '$.type') = 'text'
        {}
      ORDER BY p.time_updated, p.id",
            legacy_only_clause(schema, "p.session_id")
        );
        indexed += refresh_opencode_rows(database, store, &store.path, "", &source, &sql)?;
    }
    Ok((indexed, 0))
}

fn refresh_opencode_rows(
    database: &Connection,
    store: &TranscriptStore,
    cursor_key: &str,
    key_prefix: &str,
    source: &Connection,
    sql: &str,
) -> Result<usize, String> {
    let cursor: (SqlValue, SqlValue) = database
        .query_row(
            "SELECT time_updated, part_id FROM opencode_cursors WHERE store = ?",
            [cursor_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or((SqlValue::Integer(-1), SqlValue::Text(String::new())));
    let rows: Vec<OpenCodePartRow> = {
        let mut statement = source.prepare(sql).map_err(sql_error)?;
        statement
            .query_map(params![cursor.0, cursor.1], |row| {
                let text = |index: usize| -> rusqlite::Result<String> {
                    Ok(match row.get_ref(index)? {
                        ValueRef::Null => String::new(),
                        value => js_text(value),
                    })
                };
                Ok(OpenCodePartRow {
                    part_id: js_text(row.get_ref(0)?),
                    part_id_value: row.get(0)?,
                    time_updated: row.get(1)?,
                    session_id: js_text(row.get_ref(2)?),
                    directory: text(3)?,
                    title: text(4)?,
                    session_updated: js_number_of(row.get_ref(5)?),
                    role: text(6)?,
                    text: text(7)?,
                })
            })
            .and_then(|rows| rows.collect())
            .map_err(sql_error)?
    };
    let Some(last) = rows.last() else {
        return Ok(0);
    };
    let transaction = database.unchecked_transaction().map_err(sql_error)?;
    let mut indexed = 0;
    {
        let mut insert = transaction
            .prepare_cached("INSERT INTO message_rows (store, path, source, role, date, project, key, text) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .map_err(sql_error)?;
        let mut remove = transaction
            .prepare_cached("DELETE FROM message_rows WHERE store = ? AND key = ?")
            .map_err(sql_error)?;
        let mut remove_legacy = transaction
            .prepare_cached(&format!(
                "DELETE FROM message_rows WHERE store = ? AND path = ? AND key NOT LIKE '{V2_KEY_PREFIX}%'"
            ))
            .map_err(sql_error)?;
        let mut cleared: HashSet<String> = HashSet::new();
        let mut sessions: HashSet<String> = HashSet::new();
        for row in &rows {
            let key = format!("{key_prefix}{}", row.part_id);
            let path = opencode_locator(&store.path, &row.session_id);
            // A v2 message spans several rows, one per content item, so its old rows are removed once.
            if cleared.insert(key.clone()) {
                remove
                    .execute(params![store.path, key])
                    .map_err(sql_error)?;
            }
            if !key_prefix.is_empty() && sessions.insert(path.clone()) {
                remove_legacy
                    .execute(params![store.path, path])
                    .map_err(sql_error)?;
            }
            if row.text.is_empty() {
                continue;
            }
            let role = if row.role.is_empty() {
                "unknown"
            } else {
                &row.role
            };
            let project = [row.directory.as_str(), row.title.as_str()]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or("~");
            insert
                .execute(params![
                    store.path,
                    path,
                    "opencode",
                    role,
                    iso_date_from_millis(row.session_updated)?,
                    compact_home(project),
                    key,
                    row.text
                ])
                .map_err(sql_error)?;
            indexed += 1;
        }
        transaction
            .execute(
                "INSERT OR REPLACE INTO opencode_cursors (store, time_updated, part_id) VALUES (?, ?, ?)",
                params![cursor_key, last.time_updated, last.part_id_value],
            )
            .map_err(sql_error)?;
    }
    transaction.commit().map_err(sql_error)?;
    Ok(indexed)
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// `"query"` with doubled quotes: an FTS5 string literal (a case-folded substring for trigrams).
fn fts_literal(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// How `groupedMatches` breaks count ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieBreak {
    Path,
    Date,
}

/// One transcript's aggregate from [`IndexReader::grouped_matches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedRow {
    pub store: String,
    pub path: String,
    pub source: TranscriptSource,
    pub count: usize,
    pub date: String,
    pub project: String,
}

impl GroupedRow {
    /// The match without snippets: an empty date or project falls back to the path's.
    pub fn into_match(self) -> IndexedStoreMatch {
        let date = if self.date.is_empty() {
            date_from_path(&self.path)
        } else {
            self.date
        };
        let project = if self.project.is_empty() {
            project_from_transcript_path(&self.path, self.source)
        } else {
            self.project
        };
        IndexedStoreMatch {
            store: self.store,
            matched: StoreSearchMatch {
                source: self.source,
                path: self.path,
                count: self.count,
                date,
                project,
                snippets: Vec::new(),
            },
        }
    }
}

type MessageVisitor<'a> = dyn FnMut(&str, &str, &str, Option<&str>) + 'a;

/// An open index for searches. Ranking and snippets are separate calls, so a
/// caller fetches snippets only for the transcripts it returns.
pub struct IndexReader {
    database: Connection,
}

impl IndexReader {
    /// Opens the index as `openIndex` did (writable; the schema is reset when stale).
    pub fn open(path: &str) -> Result<IndexReader, String> {
        Ok(IndexReader {
            database: open_index(path)?,
        })
    }

    /// Stream every matching visible row for the selected sessions without a raw-line cap.
    pub fn visit_matching_messages(
        &self,
        query: &str,
        paths: &[&str],
        visit: &mut MessageVisitor<'_>,
    ) -> Result<(), String> {
        let (table, predicate, parameters) = if query.chars().count() >= 3 {
            (
                "messages JOIN message_rows r ON r.id = messages.rowid",
                "messages MATCH ?",
                vec![SqlValue::Text(fts_literal(query))],
            )
        } else if query.is_ascii() {
            (
                "message_rows r",
                "(instr(lower(r.text), ?) > 0 OR length(CAST(r.text AS BLOB)) > length(r.text))",
                vec![SqlValue::Text(js_lower(query).into_owned())],
            )
        } else {
            // SQLite lower() only handles ASCII. The caller verifies Unicode matches.
            ("message_rows r", "1 = 1", Vec::new())
        };
        for paths in paths.chunks(400) {
            let sql = format!(
                "SELECT r.path, r.role, r.text, r.date FROM {table}
                WHERE {predicate} AND r.path IN ({}) ORDER BY r.id",
                placeholders(paths.len())
            );
            let mut values = parameters.clone();
            values.extend(paths.iter().map(|p| SqlValue::Text((*p).to_string())));
            let mut statement = self.database.prepare(&sql).map_err(sql_error)?;
            let mut rows = statement
                .query(params_from_iter(values))
                .map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                let path = js_text(row.get_ref(0).map_err(sql_error)?);
                let role = js_text(row.get_ref(1).map_err(sql_error)?);
                let text = js_text(row.get_ref(2).map_err(sql_error)?);
                let date = js_text(row.get_ref(3).map_err(sql_error)?);
                visit(
                    &path,
                    &role,
                    &text,
                    (!date.is_empty() && date != "unknown").then_some(date.as_str()),
                );
            }
        }
        Ok(())
    }

    /// Latest indexed activity for a bounded set of candidate sessions.
    pub fn last_activity(&self, paths: &[&str]) -> Result<HashMap<String, String>, String> {
        let mut dates = HashMap::new();
        let mut statement = self
            .database
            .prepare_cached("SELECT MAX(date) FROM message_rows WHERE path = ?")
            .map_err(sql_error)?;
        for path in paths {
            let date: Option<String> = statement
                .query_row([path], |row| row.get(0))
                .map_err(sql_error)?;
            if let Some(date) = date.filter(|d| !d.is_empty() && d != "unknown") {
                dates.insert((*path).to_string(), date);
            }
        }
        Ok(dates)
    }

    /// `groupedMatches`: transcripts ranked by literal occurrence count, aggregated
    /// in SQL so only one row per transcript is read. The trigram match already
    /// guarantees a case-folded substring hit, so a count that SQLite's ASCII-only
    /// lower() misses is floored at one instead of being dropped.
    pub fn grouped_matches(
        &self,
        query: &str,
        stores: &[TranscriptStore],
        limit: usize,
        tie_break: TieBreak,
    ) -> Result<Vec<GroupedRow>, String> {
        self.grouped_filtered(query, stores, limit, tie_break, None)
    }

    /// Find applies project/activity eligibility before any candidate cap.
    pub fn find_matches(
        &self,
        query: &str,
        stores: &[TranscriptStore],
        project: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<GroupedRow>, String> {
        self.grouped_filtered(
            query,
            stores,
            usize::MAX,
            TieBreak::Date,
            Some((project, since)),
        )
    }

    fn grouped_filtered(
        &self,
        query: &str,
        stores: &[TranscriptStore],
        limit: usize,
        tie_break: TieBreak,
        filters: Option<(Option<&str>, Option<&str>)>,
    ) -> Result<Vec<GroupedRow>, String> {
        if js::len(query) < 3 || stores.is_empty() {
            return Ok(Vec::new());
        }
        let lowered = js_lower(query).into_owned();
        let tie = match tie_break {
            TieBreak::Path => "path",
            TieBreak::Date => "date",
        };
        let mut conditions = String::new();
        let mut filter_values = Vec::new();
        if let Some((project, since)) = filters {
            if let Some(project) = project.filter(|p| p.is_ascii()) {
                conditions.push_str(" AND (instr(lower(r.project), ?) > 0 OR (r.source IN ('claude','pi','droid') AND instr(replace(lower(r.project), '-', '/'), replace(?, '-', '/')) > 0))");
                let project = js_lower(compact_home(project)).into_owned();
                filter_values.extend([SqlValue::Text(project.clone()), SqlValue::Text(project)]);
            }
            if let Some(since) = since {
                conditions.push_str(" AND EXISTS (SELECT 1 FROM message_rows activity WHERE activity.path = r.path AND (activity.date >= ? OR activity.date = ''))");
                filter_values.push(SqlValue::Text(since.to_string()));
            }
            conditions.push_str(" AND (r.role <> 'user' OR (");
            for (i, prefix) in crate::find::INJECTED_PREFIXES.iter().enumerate() {
                if i > 0 {
                    conditions.push_str(" AND ");
                }
                conditions.push_str(
                    "substr(ltrim(r.text, char(9)||char(10)||char(13)||' '), 1, length(?)) <> ?",
                );
                filter_values.extend([
                    SqlValue::Text((*prefix).to_string()),
                    SqlValue::Text((*prefix).to_string()),
                ]);
            }
            conditions.push_str("))");
        }
        let date_aggregate = if filters.is_some() { "MAX" } else { "MIN" };
        let sql = format!(
            "
    SELECT store, path, source, SUM(MAX(1, occurrences)) AS count, {date_aggregate}(date) AS date, MIN(project) AS project
    FROM (
      SELECT r.store, r.path, r.source, r.date, r.project,
             (length(lower(r.text)) - length(replace(lower(r.text), ?, ''))) / length(?) AS occurrences
      FROM messages
      JOIN message_rows r ON r.id = messages.rowid
      WHERE messages MATCH ? AND r.store IN ({}) {conditions}
    )
    GROUP BY path
    ORDER BY count DESC, {tie} DESC
    LIMIT ?
  ",
            placeholders(stores.len())
        );
        let mut values: Vec<SqlValue> = vec![
            SqlValue::Text(lowered.clone()),
            SqlValue::Text(lowered),
            SqlValue::Text(fts_literal(query)),
        ];
        values.extend(stores.iter().map(|s| SqlValue::Text(s.path.clone())));
        values.extend(filter_values);
        values.push(SqlValue::Integer(limit.min(i64::MAX as usize) as i64));
        let mut statement = self.database.prepare(&sql).map_err(sql_error)?;
        let rows = statement
            .query_map(params_from_iter(values), |row| {
                let source: String = row.get(2)?;
                Ok(GroupedRow {
                    store: js_text(row.get_ref(0)?),
                    path: js_text(row.get_ref(1)?),
                    source: TranscriptSource::from_name(&source)
                        .unwrap_or(TranscriptSource::Opencode),
                    count: row.get::<_, i64>(3)?.max(0) as usize,
                    date: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    project: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                })
            })
            .and_then(|rows| rows.collect())
            .map_err(sql_error)?;
        Ok(rows)
    }

    /// The first `limit` matching messages of each transcript in `paths`, in
    /// index order, as snippets around the query (the TypeScript's per-path
    /// `MATCH ... AND r.path = ? ORDER BY r.id LIMIT ?`). The full-text lookup
    /// runs once for all paths: each lookup decodes every trigram's doclist, so
    /// one per path would repeat that work.
    pub fn snippets(
        &self,
        query: &str,
        paths: &[&str],
        limit: usize,
    ) -> Result<HashMap<String, Vec<TranscriptSnippet>>, String> {
        let mut out: HashMap<String, Vec<TranscriptSnippet>> = HashMap::new();
        if paths.is_empty() || limit == 0 {
            return Ok(out);
        }
        let sql = format!(
            "WITH hits AS MATERIALIZED (SELECT rowid AS id FROM messages WHERE messages MATCH ?)
      SELECT r.path, r.id FROM hits JOIN message_rows r ON r.id = hits.id WHERE r.path IN ({})",
            placeholders(paths.len())
        );
        let mut values: Vec<SqlValue> = vec![SqlValue::Text(fts_literal(query))];
        values.extend(paths.iter().map(|p| SqlValue::Text(p.to_string())));
        let mut ids: HashMap<String, Vec<i64>> = HashMap::new();
        {
            let mut statement = self.database.prepare(&sql).map_err(sql_error)?;
            let mut rows = statement
                .query(params_from_iter(values))
                .map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                ids.entry(js_text(row.get_ref(0).map_err(sql_error)?))
                    .or_default()
                    .push(row.get(1).map_err(sql_error)?);
            }
        }
        let mut fetch = self
            .database
            .prepare_cached("SELECT role, text FROM message_rows WHERE id = ?")
            .map_err(sql_error)?;
        for (path, mut row_ids) in ids {
            row_ids.sort_unstable();
            row_ids.truncate(limit);
            let mut snippets = Vec::with_capacity(row_ids.len());
            for id in row_ids {
                snippets.push(
                    fetch
                        .query_row([id], |row| {
                            let text = js_text(row.get_ref(1)?);
                            Ok(TranscriptSnippet {
                                role: js_text(row.get_ref(0)?),
                                text: snippet_around(&text, query),
                            })
                        })
                        .map_err(sql_error)?,
                );
            }
            out.insert(path, snippets);
        }
        Ok(out)
    }
}

/// `searchTranscriptIndex(query, stores, path, limit)`: per-transcript counts, ties broken by path.
pub fn search_transcript_index(
    query: &str,
    stores: &[TranscriptStore],
    path: &str,
    limit: usize,
) -> Result<Vec<IndexedFileMatch>, String> {
    if js::len(query) < 3 || stores.is_empty() {
        return Ok(Vec::new());
    }
    let reader = IndexReader::open(path)?;
    Ok(reader
        .grouped_matches(query, stores, limit, TieBreak::Path)?
        .into_iter()
        .map(|row| IndexedFileMatch {
            store: row.store,
            source: row.source,
            path: row.path,
            count: row.count,
        })
        .collect())
}

/// `searchTranscriptIndexMatches(query, stores, path, limit, snippetLimit)`:
/// ranked matches with up to `snippet_limit` snippets each, ties broken by date.
pub fn search_transcript_index_matches(
    query: &str,
    stores: &[TranscriptStore],
    path: &str,
    limit: usize,
    snippet_limit: usize,
) -> Result<Vec<IndexedStoreMatch>, String> {
    if js::len(query) < 3 || stores.is_empty() {
        return Ok(Vec::new());
    }
    let reader = IndexReader::open(path)?;
    let mut found: Vec<IndexedStoreMatch> = reader
        .grouped_matches(query, stores, limit, TieBreak::Date)?
        .into_iter()
        .map(GroupedRow::into_match)
        .collect();
    let paths: Vec<&str> = found.iter().map(|m| m.matched.path.as_str()).collect();
    let mut snippets = reader.snippets(query, &paths, snippet_limit)?;
    for item in &mut found {
        item.matched.snippets = snippets.remove(&item.matched.path).unwrap_or_default();
    }
    Ok(found)
}

/// `transcriptIndexStatus(path)`: counts of an existing index, or `exists: false`.
pub fn transcript_index_status(path: &str) -> TranscriptIndexStatus {
    let missing = || TranscriptIndexStatus {
        path: path.to_string(),
        exists: false,
        schema_version: 0,
        files: 0,
        messages: 0,
        bytes: 0,
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return missing();
    };
    let status = || -> Result<TranscriptIndexStatus, String> {
        let database = open_index_readonly(path)?;
        let schema_version = stored_schema_version(&database)?;
        let (files, messages) = if schema_version == SCHEMA_VERSION {
            totals(&database)?
        } else {
            (0, 0)
        };
        Ok(TranscriptIndexStatus {
            path: path.to_string(),
            exists: true,
            schema_version,
            files,
            messages,
            bytes: metadata.len(),
        })
    };
    status().unwrap_or_else(|_| missing())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("dejavu-index-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn claude_line(role: &str, text: &str) -> String {
        serde_json::json!({
            "type": role,
            "timestamp": "2026-09-02T10:00:00Z",
            "cwd": "/work/dejavu",
            "message": { "role": role, "content": [{ "type": "text", "text": text }] },
        })
        .to_string()
    }

    fn store(source: TranscriptSource, kind: StoreKind, path: &str) -> TranscriptStore {
        TranscriptStore {
            source,
            kind,
            path: path.to_string(),
        }
    }

    fn refresh(stores: &[TranscriptStore], index: &str) -> IndexRefreshResult {
        refresh_transcript_index(stores, index, false, 4).unwrap()
    }

    fn counts(query: &str, stores: &[TranscriptStore], index: &str) -> Vec<usize> {
        search_transcript_index(query, stores, index, 200)
            .unwrap()
            .into_iter()
            .map(|m| m.count)
            .collect()
    }

    #[test]
    fn incrementally_replaces_changed_files_and_removes_deleted_files() {
        let root = temp_root("incremental");
        let store_path = format!("{root}/claude");
        let transcript = format!("{store_path}/session.jsonl");
        let index = format!("{root}/cache/index.sqlite");
        let stores = [store(
            TranscriptSource::Claude,
            StoreKind::Jsonl,
            &store_path,
        )];
        std::fs::create_dir_all(&store_path).unwrap();
        std::fs::write(
            &transcript,
            claude_line("user", "Needle phrase appears twice: needle phrase"),
        )
        .unwrap();

        let initial = refresh(&stores, &index);
        assert_eq!(
            (
                initial.files,
                initial.messages,
                initial.indexed,
                initial.removed
            ),
            (1, 1, 1, 0)
        );
        assert_eq!(
            search_transcript_index("needle phrase", &stores, &index, 200).unwrap(),
            [IndexedFileMatch {
                store: store_path.clone(),
                source: TranscriptSource::Claude,
                path: transcript.clone(),
                count: 2,
            }]
        );
        let found = search_transcript_index_matches("needle phrase", &stores, &index, 10, 1)
            .unwrap()
            .remove(0);
        assert_eq!(found.store, store_path);
        assert_eq!(found.matched.count, 2);
        assert_eq!(found.matched.date, "2026-09-02");
        assert_eq!(found.matched.project, "/work/dejavu");
        assert_eq!(found.matched.snippets.len(), 1);
        assert_eq!(found.matched.snippets[0].role, "user");

        assert_eq!(refresh(&stores, &index).indexed, 0);

        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &transcript,
            claude_line("assistant", "Replacement text only"),
        )
        .unwrap();
        assert_eq!(refresh(&stores, &index).indexed, 1);
        assert!(counts("needle phrase", &stores, &index).is_empty());
        assert_eq!(counts("replacement text", &stores, &index), [1]);

        std::fs::remove_file(&transcript).unwrap();
        let deleted = refresh(&stores, &index);
        assert_eq!(
            (deleted.files, deleted.messages, deleted.removed),
            (0, 0, 1)
        );
        let status = transcript_index_status(&index);
        assert!(status.exists);
        assert_eq!((status.files, status.messages), (0, 0));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn verifies_exact_literal_text_after_trigram_candidate_lookup() {
        let root = temp_root("literal");
        let store_path = format!("{root}/codex");
        let index = format!("{root}/index.sqlite");
        std::fs::create_dir_all(&store_path).unwrap();
        std::fs::write(
            format!("{store_path}/session.jsonl"),
            serde_json::json!({
                "type": "response_item",
                "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Alpha beta and alpha-beta" }] },
            })
            .to_string(),
        )
        .unwrap();
        let stores = [store(
            TranscriptSource::Codex,
            StoreKind::Jsonl,
            &store_path,
        )];
        refresh(&stores, &index);
        assert_eq!(counts("alpha beta", &stores, &index), [1]);
        assert!(counts("alpha  beta", &stores, &index).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn appends_only_the_new_tail_of_a_growing_jsonl_file() {
        use std::io::Write;
        let root = temp_root("append");
        let store_path = format!("{root}/claude");
        let transcript = format!("{store_path}/grow.jsonl");
        let index = format!("{root}/index.sqlite");
        let stores = [store(
            TranscriptSource::Claude,
            StoreKind::Jsonl,
            &store_path,
        )];
        std::fs::create_dir_all(&store_path).unwrap();
        std::fs::write(
            &transcript,
            format!("{}\n", claude_line("user", "first message")),
        )
        .unwrap();
        let append = |text: &str| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        refresh(&stores, &index);
        // A half-written trailing line must wait for its newline.
        append(&format!(
            "{}\n{{\"type\":\"user\",\"mess",
            claude_line("assistant", "second message")
        ));
        let grown = refresh(&stores, &index);
        assert_eq!((grown.indexed, grown.messages), (1, 2));
        assert_eq!(counts("second message", &stores, &index), [1]);
        append(
            "age\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"third message\"}]}}\n",
        );
        assert_eq!(refresh(&stores, &index).messages, 3);
        assert_eq!(counts("third message", &stores, &index), [1]);
        assert_eq!(counts("first message", &stores, &index), [1]);
        // A rewrite that changes the head falls back to a full reparse.
        std::fs::write(
            &transcript,
            format!("{}\n", claude_line("user", "rewritten only")),
        )
        .unwrap();
        assert_eq!(refresh(&stores, &index).messages, 1);
        assert!(counts("first message", &stores, &index).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn legacy_opencode(path: &str) -> Connection {
        let database = Connection::open(path).unwrap();
        database
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
            )
            .unwrap();
        database
    }

    // Date.UTC(2026, 7, 4) and Date.UTC(2026, 8, 1).
    const AUG_4: i64 = 1_785_801_600_000;
    const SEP_1: i64 = 1_788_220_800_000;

    #[test]
    fn indexes_opencode_text_parts_incrementally_by_cursor_and_reports_unreadable_stores() {
        let root = temp_root("opencode");
        let database_path = format!("{root}/opencode.db");
        let index = format!("{root}/index.sqlite");
        let database = legacy_opencode(&database_path);
        database
            .execute(
                "INSERT INTO session VALUES (?, ?, ?, ?)",
                params!["ses_1", "/work/demo", "Demo", AUG_4],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO message VALUES (?, ?, ?, ?)",
                params!["msg_1", "ses_1", 1, r#"{"role":"user"}"#],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    "part_1",
                    "msg_1",
                    "ses_1",
                    1,
                    1,
                    r#"{"type":"text","text":"Needle question"}"#
                ],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    "part_2",
                    "msg_1",
                    "ses_1",
                    2,
                    2,
                    r#"{"type":"reasoning","text":"Needle hidden"}"#
                ],
            )
            .unwrap();
        let stores = [
            store(
                TranscriptSource::Opencode,
                StoreKind::Sqlite,
                &database_path,
            ),
            store(
                TranscriptSource::Opencode,
                StoreKind::Sqlite,
                &format!("{root}/missing.db"),
            ),
        ];
        let initial = refresh(&stores, &index);
        assert_eq!(initial.indexed, 1);
        assert_eq!(initial.skipped.len(), 1);
        assert_eq!(initial.skipped[0].path, format!("{root}/missing.db"));
        let found = search_transcript_index_matches("needle", &stores[..1], &index, 40, 3)
            .unwrap()
            .remove(0)
            .matched;
        assert_eq!(found.source, TranscriptSource::Opencode);
        assert_eq!(found.count, 1);
        assert_eq!(found.project, "/work/demo");
        assert_eq!(found.date, "2026-08-04");
        assert_eq!(
            found.snippets,
            [TranscriptSnippet {
                role: "user".into(),
                text: "Needle question".into()
            }]
        );
        let locator = crate::opencode::parse_opencode_locator(&found.path).unwrap();
        assert_eq!(locator.database_path, database_path);
        assert_eq!(locator.session_id, "ses_1");

        // A streamed part is re-read when OpenCode bumps its time_updated.
        database
            .execute(
                "UPDATE part SET time_updated = 3, data = ? WHERE id = 'part_1'",
                [r#"{"type":"text","text":"Needle question needle again"}"#],
            )
            .unwrap();
        assert_eq!(refresh(&stores[..1], &index).indexed, 1);
        let again = search_transcript_index_matches("needle", &stores[..1], &index, 40, 3).unwrap();
        assert_eq!(again[0].matched.count, 2);
        assert_eq!(refresh(&stores[..1], &index).indexed, 0);
        drop(database);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn indexes_v2_session_message_rows_and_drops_legacy_rows_once_a_session_moves_to_v2() {
        let root = temp_root("opencode-v2");
        let database_path = format!("{root}/opencode.db");
        let index = format!("{root}/index.sqlite");
        let database = legacy_opencode(&database_path);
        database
            .execute_batch(
                "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT, time_updated INTEGER NOT NULL);
                 CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO session VALUES (?, ?, ?, ?)",
                params!["ses_1", "/work/demo", "Demo", AUG_4],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO message VALUES (?, ?, ?, ?)",
                params!["msg_1", "ses_1", 1, r#"{"role":"user"}"#],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    "part_1",
                    "msg_1",
                    "ses_1",
                    1,
                    1,
                    r#"{"type":"text","text":"Needle legacy"}"#
                ],
            )
            .unwrap();
        let stores = [store(
            TranscriptSource::Opencode,
            StoreKind::Sqlite,
            &database_path,
        )];
        let insert = |id: &str, kind: &str, seq: i64, created: i64, updated: i64, data: &str| {
            database
                .execute(
                    "INSERT OR REPLACE INTO session_message VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![id, "ses_1", kind, seq, created, updated, data],
                )
                .unwrap();
        };
        let first = |limit: usize| {
            search_transcript_index_matches("needle", &stores, &index, 40, limit).unwrap()
        };
        assert_eq!(refresh(&stores, &index).indexed, 1);
        assert_eq!(first(3)[0].matched.count, 1);

        database
            .execute(
                "INSERT INTO session_v2 VALUES (?, ?, ?, ?)",
                params!["ses_1", "/work/demo", "Demo", SEP_1],
            )
            .unwrap();
        insert("msg_v1", "user", 0, 5, 5, r#"{"text":"Needle question"}"#);
        insert(
            "msg_v2",
            "assistant",
            1,
            6,
            6,
            r#"{"content":[{"type":"text","text":"Needle answer"},{"type":"reasoning","text":"Needle hidden"},{"type":"text","text":"Needle again"}]}"#,
        );
        insert(
            "msg_v3",
            "synthetic",
            2,
            7,
            7,
            r#"{"text":"Needle synthetic"}"#,
        );
        assert_eq!(refresh(&stores, &index).indexed, 3);
        let matches = first(5);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].matched.count, 3);
        assert_eq!(matches[0].matched.date, "2026-09-01");
        let mut texts: Vec<&str> = matches[0]
            .matched
            .snippets
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        texts.sort();
        assert_eq!(texts, ["Needle again", "Needle answer", "Needle question"]);

        // A streamed assistant message is re-read whole when its time_updated moves.
        insert(
            "msg_v2",
            "assistant",
            1,
            6,
            8,
            r#"{"content":[{"type":"text","text":"Needle final"}]}"#,
        );
        assert_eq!(refresh(&stores, &index).indexed, 1);
        assert_eq!(first(3)[0].matched.count, 2);
        assert_eq!(refresh(&stores, &index).indexed, 0);
        drop(database);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rebuilds_an_index_whose_schema_version_is_stale() {
        let root = temp_root("schema");
        let index = format!("{root}/index.sqlite");
        {
            let stale = Connection::open(&index).unwrap();
            stale
                .execute_batch(
                    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO metadata VALUES ('schema_version', '1');
                     CREATE TABLE files (path TEXT PRIMARY KEY);",
                )
                .unwrap();
        }
        assert_eq!(transcript_index_status(&index).schema_version, 1);
        refresh(&[], &index);
        let status = transcript_index_status(&index);
        assert!(status.exists);
        assert_eq!(
            (status.schema_version, status.files, status.messages),
            (SCHEMA_VERSION, 0, 0)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
