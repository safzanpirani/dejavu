//! Semantic search over Claude memory: chunks of each memory file, their
//! embedding vectors, and the queue that keeps the two in step.
//!
//! The chunk table is the queue. Freshness is decided by content hash: an
//! edited passage becomes a new `pending` row and its old row (with the old
//! vector and text) is deleted in the same update, so stale wording can never
//! be returned. Vectors live in the transcript index file, in tables outside
//! its versioned schema, and are searched by exact brute force.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::embed::{self, BatchPoll, EmbedError, Embedder};
use crate::memory::{self, MemoryMatch, MemoryStore, SearchOptions};

/// A chunk grows to about this many characters before the next one starts.
const TARGET_CHARS: usize = 1600;
/// No single chunk exceeds this, far below any embedding model's input limit.
const MAX_CHARS: usize = 4000;
/// Chunks and text bytes per batch job (Gemini caps inline batches at 20 MB).
const BATCH_CHUNKS: usize = 1000;
const BATCH_BYTES: usize = 8_000_000;
const STANDARD_GROUP: usize = 100;
const RATE_LIMIT_RETRIES: usize = 3;
/// A batch still running after this long is re-embedded through the standard endpoint.
const BATCH_PATIENCE_MS: i64 = 24 * 60 * 60 * 1000;
/// `memory search` talks to the batch API at most this often.
pub const ADVANCE_INTERVAL_MS: i64 = 10 * 60 * 1000;
/// Chunks a search may embed through the standard endpoint on the way; a
/// larger backlog waits for `dejavu memory embed`.
const SEARCH_STANDARD_CHUNKS: usize = 200;
const RRF_K: f64 = 60.0;
/// A file is a semantic result when its closest passage scores at least this
/// and within `SIMILARITY_MARGIN` of the best file. Measured on
/// gemini-embedding-2 at 768 dimensions with task prefixes, where scores
/// crowd together: the best passage for an off-topic query reaches about 0.60,
/// the right note for a paraphrase 0.72-0.79, and loosely related notes sit
/// 0.05-0.10 under it.
pub const MIN_SIMILARITY: f32 = 0.65;
const SIMILARITY_MARGIN: f32 = 0.06;

fn sql(error: rusqlite::Error) -> String {
    error.to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// File stem, project and frontmatter description: context every chunk carries.
    pub title: String,
    pub text: String,
    /// SHA-256 of the title and text, the chunk's identity within its file.
    pub sha: String,
    /// The text looks like it holds a credential and is never sent or stored.
    pub withheld: bool,
}

/// Why a whole file is never chunked or sent: Claude's `MEMORY.md` index
/// (pointers to the other files, and the only place gated titles appear), and
/// gated files (`DO NOT READ UNLESS …` in the file name, frontmatter name, or
/// description). The frontmatter is scanned line by line, so a marker in a
/// YAML block scalar (`description: >`) or a continuation line still counts.
fn skip_reason(file_name: &str, lines: &[&str]) -> Option<&'static str> {
    const MARKER: &str = "do not read unless";
    if file_name == "MEMORY.md" {
        return Some("index");
    }
    if file_name.to_lowercase().contains(MARKER) {
        return Some("gated");
    }
    if lines.first().map(|line| line.trim()) != Some("---") {
        return None;
    }
    let end = lines.iter().skip(1).position(|line| line.trim() == "---")?;
    // A top-level `key:` line starts a field; indented lines continue it.
    let mut in_field = false;
    for line in &lines[1..=end] {
        if !line.starts_with([' ', '\t'])
            && let Some((key, _)) = line.split_once(':')
        {
            in_field = matches!(key.trim(), "name" | "description");
        }
        if in_field && line.to_lowercase().contains(MARKER) {
            return Some("gated");
        }
    }
    None
}

/// True when text holds something shaped like a credential: a private-key
/// block or a token with a well-known prefix.
pub fn looks_secret(text: &str) -> bool {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return true;
    }
    const PREFIXES: [(&str, usize); 14] = [
        ("sk-", 24),
        ("AKIA", 20),
        ("ASIA", 20),
        ("AIza", 35),
        ("ghp_", 30),
        ("gho_", 30),
        ("ghs_", 30),
        ("github_pat_", 30),
        ("glpat-", 20),
        ("xoxb-", 20),
        ("xoxp-", 20),
        ("hf_", 30),
        ("eyJ", 40),
        ("tvly-", 20),
    ];
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .any(|token| {
            PREFIXES
                .iter()
                .any(|(prefix, min)| token.len() >= *min && token.starts_with(prefix))
        })
}

/// Splits one oversized block at line ends, and an oversized line at characters.
fn split_long(block: &str) -> Vec<String> {
    if block.chars().count() <= MAX_CHARS {
        return vec![block.to_string()];
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut size = 0;
    for line in block.lines() {
        let mut rest: Vec<char> = line.chars().collect();
        if size > 0 && size + 1 + rest.len() > MAX_CHARS {
            parts.push(std::mem::take(&mut current));
            size = 0;
        }
        if size > 0 {
            current.push('\n');
            size += 1;
        }
        while size + rest.len() > MAX_CHARS {
            let tail = rest.split_off(MAX_CHARS - size);
            current.extend(rest);
            parts.push(std::mem::take(&mut current));
            size = 0;
            rest = tail;
        }
        size += rest.len();
        current.extend(rest);
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// A block that looks like it holds a credential stays whole, so the check
/// on its chunk sees all of it and every part is withheld.
fn flush<'a>(block: &mut Vec<&'a str>, sections: &mut [(Option<&'a str>, Vec<String>)]) {
    if !block.is_empty() {
        let joined = block.join("\n");
        if let Some(section) = sections.last_mut() {
            if looks_secret(&joined) {
                section.1.push(joined);
            } else {
                section.1.extend(split_long(&joined));
            }
        }
        block.clear();
    }
}

/// The chunks of one memory file, or the reason the file is skipped.
/// The body splits at Markdown headings and blank lines, never at fixed
/// offsets, so an edit changes only the chunks of its own section.
pub fn chunk_file(project: &str, file_name: &str, text: &str) -> Result<Vec<Chunk>, &'static str> {
    let lines = memory::lines_of(text);
    let front = memory::frontmatter(&lines);
    if let Some(reason) = skip_reason(file_name, &lines) {
        return Err(reason);
    }
    let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
    let mut title = format!("{stem} [{project}]");
    if let Some(description) = &front.description {
        title.push_str(": ");
        title.push_str(description);
    }

    // Sections: a heading line (or none, before the first) and its blocks.
    let mut sections: Vec<(Option<&str>, Vec<String>)> = vec![(None, Vec::new())];
    let mut block: Vec<&str> = Vec::new();
    let mut fenced = false;
    for line in lines[front.body_start.min(lines.len())..].iter().copied() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            block.push(line);
        } else if fenced {
            block.push(line);
        } else if line.starts_with('#') {
            flush(&mut block, &mut sections);
            sections.push((Some(line), Vec::new()));
        } else if line.trim().is_empty() {
            flush(&mut block, &mut sections);
        } else {
            block.push(line);
        }
    }
    flush(&mut block, &mut sections);

    let mut texts: Vec<String> = Vec::new();
    for (heading, blocks) in sections {
        let mut current = String::new();
        let mut emit = |current: &mut String| {
            if !current.is_empty() {
                texts.push(match heading {
                    Some(heading) => format!("{heading}\n{current}"),
                    None => std::mem::take(current),
                });
                current.clear();
            }
        };
        for block in blocks {
            if !current.is_empty() && current.len() + block.len() + 2 > TARGET_CHARS {
                emit(&mut current);
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&block);
        }
        emit(&mut current);
    }
    if texts.is_empty() && front.description.is_some() {
        texts.push(String::new());
    }

    let mut seen = HashSet::new();
    let mut chunks = Vec::new();
    for text in texts {
        let sha = hex(&Sha256::digest(format!("{title}\n{text}").as_bytes()));
        if seen.insert(sha.clone()) {
            let withheld = looks_secret(&text) || looks_secret(&title);
            chunks.push(Chunk {
                title: title.clone(),
                text,
                sha,
                withheld,
            });
        }
    }
    Ok(chunks)
}

/// Opens the index file for the memory tables without touching the transcript
/// schema, which another dejavu version may own.
pub fn open_store(path: &str) -> Result<Connection, String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{error}: {}", parent.display()))?;
    }
    let database = Connection::open(path).map_err(sql)?;
    database
        .busy_timeout(Duration::from_secs(10))
        .map_err(sql)?;
    database
        .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .map_err(sql)?;
    database
        .execute_batch(
            "PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS memory_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS memory_files (
    path TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    skipped TEXT
  );
CREATE TABLE IF NOT EXISTS memory_chunks (
    id INTEGER PRIMARY KEY,
    root TEXT NOT NULL,
    path TEXT NOT NULL,
    chunk_no INTEGER NOT NULL,
    content_sha TEXT NOT NULL,
    title TEXT NOT NULL,
    text TEXT NOT NULL,
    status TEXT NOT NULL,
    batch_id TEXT,
    batch_pos INTEGER,
    model TEXT,
    dims INTEGER,
    vec BLOB,
    updated_at INTEGER NOT NULL,
    UNIQUE (path, content_sha)
  );
CREATE INDEX IF NOT EXISTS memory_chunks_status ON memory_chunks (status);
CREATE TABLE IF NOT EXISTS memory_embed_batches (
    batch_id TEXT PRIMARY KEY,
    submitted_at INTEGER NOT NULL,
    chunk_count INTEGER NOT NULL,
    model TEXT NOT NULL,
    dims INTEGER NOT NULL
  );",
        )
        .map_err(sql)?;
    Ok(database)
}

pub fn meta_get(database: &Connection, key: &str) -> Result<Option<String>, String> {
    database
        .query_row(
            "SELECT value FROM memory_meta WHERE key = ?",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)
}

pub fn meta_set(database: &Connection, key: &str, value: &str) -> Result<(), String> {
    database
        .execute(
            "INSERT INTO memory_meta (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )
        .map(|_| ())
        .map_err(sql)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStats {
    pub files: usize,
    pub changed_files: usize,
    pub removed_files: usize,
    pub chunks_added: usize,
    pub chunks_removed: usize,
}

/// Brings the chunk table in line with the memory files under `root`. Local
/// only: nothing is sent. Unchanged files (same size and mtime) are not read.
pub fn sync(
    database: &mut Connection,
    root: &str,
    store: &dyn MemoryStore,
    now_ms: i64,
) -> Result<SyncStats, String> {
    let files = memory::memory_files(root, store)?;
    let mut known: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut statement = database
            .prepare("SELECT path, size, mtime_ns FROM memory_files WHERE root = ?")
            .map_err(sql)?;
        let rows = statement
            .query_map([root], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
            .map_err(sql)?;
        for row in rows {
            let (path, stat): (String, (i64, i64)) = row.map_err(sql)?;
            known.insert(path, stat);
        }
    }
    let mut stats = SyncStats {
        files: files.len(),
        ..SyncStats::default()
    };
    let mut seen = HashSet::new();
    let mut changes = Vec::new();
    for file in &files {
        seen.insert(file.path.as_str());
        let stat = store
            .stat(&file.path)
            .map(|(size, mtime)| (i64::try_from(size).unwrap_or(i64::MAX), mtime));
        if stat.is_some() && stat == known.get(&file.path).copied() {
            continue;
        }
        // An unreadable file keeps its rows until it reads again or disappears.
        let Ok(text) = store.read(&file.path) else {
            continue;
        };
        changes.push((file, stat, chunk_file(&file.project, &file.name, &text)));
    }
    let removed: Vec<&String> = known
        .keys()
        .filter(|path| !seen.contains(path.as_str()))
        .collect();
    if changes.is_empty() && removed.is_empty() {
        return Ok(stats);
    }

    let transaction = database.transaction().map_err(sql)?;
    for path in removed {
        stats.chunks_removed += transaction
            .execute("DELETE FROM memory_chunks WHERE path = ?", [path])
            .map_err(sql)?;
        transaction
            .execute("DELETE FROM memory_files WHERE path = ?", [path])
            .map_err(sql)?;
        stats.removed_files += 1;
    }
    for (file, stat, chunks) in changes {
        let mut existing: HashMap<String, i64> = HashMap::new();
        {
            let mut statement = transaction
                .prepare("SELECT content_sha, id FROM memory_chunks WHERE path = ?")
                .map_err(sql)?;
            let rows = statement
                .query_map([&file.path], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql)?;
            for row in rows {
                let (sha, id): (String, i64) = row.map_err(sql)?;
                existing.insert(sha, id);
            }
        }
        let mut touched = false;
        for (number, chunk) in chunks.iter().flatten().enumerate() {
            match existing.remove(&chunk.sha) {
                Some(id) => {
                    transaction
                        .execute(
                            "UPDATE memory_chunks SET chunk_no = ? WHERE id = ?",
                            params![number as i64, id],
                        )
                        .map_err(sql)?;
                }
                None => {
                    let (status, text) = if chunk.withheld {
                        ("withheld", "")
                    } else {
                        ("pending", chunk.text.as_str())
                    };
                    transaction
                        .execute(
                            "INSERT INTO memory_chunks
                             (root, path, chunk_no, content_sha, title, text, status, updated_at)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                            params![
                                root,
                                file.path,
                                number as i64,
                                chunk.sha,
                                if chunk.withheld {
                                    ""
                                } else {
                                    chunk.title.as_str()
                                },
                                text,
                                status,
                                now_ms
                            ],
                        )
                        .map_err(sql)?;
                    stats.chunks_added += 1;
                    touched = true;
                }
            }
        }
        for id in existing.into_values() {
            transaction
                .execute("DELETE FROM memory_chunks WHERE id = ?", [id])
                .map_err(sql)?;
            stats.chunks_removed += 1;
            touched = true;
        }
        stats.changed_files += usize::from(touched);
        let (size, mtime) = stat.unwrap_or((-1, -1));
        transaction
            .execute(
                "INSERT INTO memory_files (path, root, size, mtime_ns, skipped) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT (path) DO UPDATE SET root = excluded.root, size = excluded.size,
                 mtime_ns = excluded.mtime_ns, skipped = excluded.skipped",
                params![file.path, root, size, mtime, chunks.as_ref().err()],
            )
            .map_err(sql)?;
    }
    transaction.commit().map_err(sql)?;
    Ok(stats)
}

fn blob(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Stores a vector on the row that still holds the text it was made from:
/// same id, same `content_sha`, and the status it had when it was read. A
/// sync in another process may delete the row meanwhile and reuse its id.
/// Returns false when the row no longer matches and nothing was stored.
fn store_vector(
    database: &Connection,
    row: &RowKey,
    vector: &[f32],
    embedder: &dyn Embedder,
    now_ms: i64,
) -> Result<bool, String> {
    database
        .execute(
            "UPDATE memory_chunks SET status = 'done', vec = ?, model = ?, dims = ?,
             batch_id = NULL, batch_pos = NULL, updated_at = ?
             WHERE id = ? AND content_sha = ? AND status = ?",
            params![
                blob(vector),
                embedder.model(),
                embedder.dims() as i64,
                now_ms,
                row.id,
                row.sha,
                row.status
            ],
        )
        .map(|changed| changed == 1)
        .map_err(sql)
}

pub struct AdvanceOptions<'a> {
    pub now_ms: i64,
    /// Embed everything through the standard endpoint now instead of batching.
    pub immediate: bool,
    pub timeout: Duration,
    /// Most chunks to embed through the standard endpoint in this run; a
    /// search uses it to stay quick. `None` embeds everything queued for it.
    pub max_standard: Option<usize>,
    /// How long to wait before retrying a rate-limited (HTTP 429) standard
    /// request, up to `RATE_LIMIT_RETRIES` times in a row. `None` stops at the
    /// first one, as a search must.
    pub rate_limit_wait: Option<Duration>,
    /// Called with (embedded so far, total) during standard-endpoint work.
    pub progress: Option<&'a dyn Fn(usize, usize)>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvanceStats {
    /// Vectors stored from finished batches.
    pub collected: usize,
    /// Vectors stored from the standard endpoint.
    pub embedded: usize,
    /// Chunks queued on the batch API in this run.
    pub submitted: usize,
    /// Results dropped because their chunk was edited or removed meanwhile.
    pub discarded: usize,
    /// Chunks the provider rejected; they stay searchable by keyword.
    pub failed: usize,
    pub notes: Vec<String>,
}

/// What identifies a queued chunk's text: a write lands only while all three match.
struct RowKey {
    id: i64,
    sha: String,
    status: String,
}

struct Row {
    key: RowKey,
    title: String,
    text: String,
}

fn rows_with_status(database: &Connection, statuses: &[&str]) -> Result<Vec<Row>, String> {
    let marks = vec!["?"; statuses.len()].join(", ");
    let mut statement = database
        .prepare(&format!(
            "SELECT id, content_sha, status, title, text FROM memory_chunks
             WHERE status IN ({marks}) ORDER BY id"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(statuses), |row| {
            Ok(Row {
                key: RowKey {
                    id: row.get(0)?,
                    sha: row.get(1)?,
                    status: row.get(2)?,
                },
                title: row.get(3)?,
                text: row.get(4)?,
            })
        })
        .map_err(sql)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql)
}

fn mark_failed(database: &Connection, row: &RowKey, embedder: &dyn Embedder) -> Result<(), String> {
    database
        .execute(
            "UPDATE memory_chunks SET status = 'failed', vec = NULL, model = ?, dims = ?,
             batch_id = NULL, batch_pos = NULL WHERE id = ? AND content_sha = ? AND status = ?",
            params![
                embedder.model(),
                embedder.dims() as i64,
                row.id,
                row.sha,
                row.status
            ],
        )
        .map(|_| ())
        .map_err(sql)
}

/// Embeds rows through the standard endpoint. Returns false when a transient
/// or account-level error stopped the run; the remaining rows keep their state.
fn embed_rows(
    database: &Connection,
    embedder: &dyn Embedder,
    rows: &[Row],
    options: &AdvanceOptions,
    stats: &mut AdvanceStats,
) -> Result<bool, String> {
    let document = |row: &Row| embed::format_document(embedder.model(), &row.title, &row.text);
    let room = options
        .max_standard
        .map_or(rows.len(), |max| max.saturating_sub(stats.embedded));
    let complete = room >= rows.len();
    let rows = &rows[..room.min(rows.len())];
    let total = rows.len();
    let mut finished = 0;
    for group in rows.chunks(STANDARD_GROUP) {
        if !take_lease(database)? {
            stats.notes.push(LEASE_LOST.into());
            return Ok(false);
        }
        let texts: Vec<String> = group.iter().map(document).collect();
        let mut outcome = embedder.embed(&texts, options.timeout);
        for _ in 0..RATE_LIMIT_RETRIES {
            match (&outcome, options.rate_limit_wait) {
                (Err(error), Some(wait)) if error.status == Some(429) => {
                    std::thread::sleep(wait);
                    outcome = embedder.embed(&texts, options.timeout);
                }
                _ => break,
            }
        }
        match outcome {
            Ok(vectors) => {
                for (row, vector) in group.iter().zip(&vectors) {
                    if store_vector(database, &row.key, vector, embedder, options.now_ms)? {
                        stats.embedded += 1;
                    } else {
                        stats.discarded += 1;
                    }
                }
            }
            // A rejected request may be one bad text: find it, keep the rest.
            Err(EmbedError {
                retryable: false,
                status: Some(400),
                ..
            }) if group.len() > 1 => {
                for row in group {
                    match embedder.embed(&[document(row)], options.timeout) {
                        Ok(vectors) => {
                            if store_vector(
                                database,
                                &row.key,
                                &vectors[0],
                                embedder,
                                options.now_ms,
                            )? {
                                stats.embedded += 1;
                            } else {
                                stats.discarded += 1;
                            }
                        }
                        Err(EmbedError {
                            retryable: false,
                            status: Some(400),
                            ..
                        }) => {
                            mark_failed(database, &row.key, embedder)?;
                            stats.failed += 1;
                        }
                        Err(error) => {
                            stats.notes.push(error.message);
                            return Ok(false);
                        }
                    }
                }
            }
            Err(EmbedError {
                retryable: false,
                status: Some(400),
                ..
            }) => {
                mark_failed(database, &group[0].key, embedder)?;
                stats.failed += 1;
            }
            Err(error) => {
                stats.notes.push(error.message);
                return Ok(false);
            }
        }
        finished += group.len();
        if let Some(progress) = options.progress {
            progress(finished, total);
        }
    }
    Ok(complete)
}

fn release_batch(database: &Connection, batch: &str, status: &str) -> Result<(), String> {
    database
        .execute(
            "UPDATE memory_chunks SET status = ?, batch_id = NULL, batch_pos = NULL
             WHERE batch_id = ? AND status = 'submitted'",
            [status, batch],
        )
        .map_err(sql)?;
    database
        .execute(
            "DELETE FROM memory_embed_batches WHERE batch_id = ?",
            [batch],
        )
        .map(|_| ())
        .map_err(sql)
}

/// One process at a time moves the queue: the holder of this `memory_meta`
/// row, whose value is `<expiry ms>:<pid>`. The holder renews it before each
/// provider call; a crashed holder's lease simply expires.
const LEASE_KEY: &str = "advance_lease";
const LEASE_MS: i64 = 15 * 60 * 1000;
const LEASE_HELD: &str = "another dejavu process is embedding memory; skipped";
const LEASE_LOST: &str = "another dejavu process took over embedding; stopped";

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// Takes or renews the advance lease. False while another process holds a
/// lease that has not expired. Wall-clock time, not `AdvanceOptions::now_ms`.
fn take_lease(database: &Connection) -> Result<bool, String> {
    let now = wall_ms();
    let owner = std::process::id().to_string();
    database
        .execute(
            "INSERT INTO memory_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value
             WHERE CAST(memory_meta.value AS INTEGER) < ?3
                OR substr(memory_meta.value, instr(memory_meta.value, ':') + 1) = ?4",
            params![LEASE_KEY, format!("{}:{owner}", now + LEASE_MS), now, owner],
        )
        .map(|changed| changed == 1)
        .map_err(sql)
}

fn release_lease(database: &Connection) -> Result<(), String> {
    database
        .execute(
            "DELETE FROM memory_meta
             WHERE key = ?1 AND substr(value, instr(value, ':') + 1) = ?2",
            params![LEASE_KEY, std::process::id().to_string()],
        )
        .map(|_| ())
        .map_err(sql)
}

/// A changed model or size makes every stored vector incomparable: cancel
/// batches made for another model and queue every chunk embedded by one.
fn invalidate(database: &Connection, embedder: &dyn Embedder) -> Result<(), String> {
    let model = embedder.model();
    let dims = embedder.dims() as i64;
    let mut stale: Vec<String> = Vec::new();
    {
        let mut statement = database
            .prepare(
                "SELECT batch_id FROM memory_embed_batches WHERE model IS NOT ? OR dims IS NOT ?",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map(params![model, dims], |row| row.get(0))
            .map_err(sql)?;
        for row in rows {
            stale.push(row.map_err(sql)?);
        }
    }
    for batch in stale {
        embedder.cancel_batch(&batch);
        release_batch(database, &batch, "pending")?;
    }
    database
        .execute(
            "UPDATE memory_chunks SET status = 'pending', vec = NULL, model = NULL, dims = NULL
             WHERE status IN ('done', 'failed') AND (model IS NOT ? OR dims IS NOT ?)",
            params![model, dims],
        )
        .map(|_| ())
        .map_err(sql)
}

/// Moves the queue forward: collects finished batches, re-embeds what a batch
/// failed to deliver, and submits pending chunks. Every provider error is
/// reported in `notes` and leaves the affected rows queued. While another
/// process is doing this, it returns at once with a note.
pub fn advance(
    database: &Connection,
    embedder: &dyn Embedder,
    options: &AdvanceOptions,
) -> Result<AdvanceStats, String> {
    if !take_lease(database)? {
        return Ok(AdvanceStats {
            notes: vec![LEASE_HELD.into()],
            ..AdvanceStats::default()
        });
    }
    let advanced = advance_leased(database, embedder, options);
    let released = release_lease(database);
    let stats = advanced?;
    released?;
    Ok(stats)
}

fn advance_leased(
    database: &Connection,
    embedder: &dyn Embedder,
    options: &AdvanceOptions,
) -> Result<AdvanceStats, String> {
    let mut stats = AdvanceStats::default();
    let model = embedder.model();
    let dims = embedder.dims() as i64;
    invalidate(database, embedder)?;

    let mut batches: Vec<(String, i64)> = Vec::new();
    {
        let mut statement = database
            .prepare(
                "SELECT batch_id, submitted_at FROM memory_embed_batches ORDER BY submitted_at",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sql)?;
        for row in rows {
            batches.push(row.map_err(sql)?);
        }
    }
    for (batch, submitted_at) in batches {
        let late = options.now_ms - submitted_at > BATCH_PATIENCE_MS;
        match embedder.poll_batch(&batch) {
            // Still running, or unreadable for now: wait, but not past a day.
            Ok(BatchPoll::Running)
            | Err(EmbedError {
                retryable: true, ..
            }) if late => {
                embedder.cancel_batch(&batch);
                release_batch(database, &batch, "retry")?;
                stats.notes.push(format!(
                    "{batch} was not done after 24 h; re-embedding its chunks now"
                ));
            }
            Ok(BatchPoll::Running) => {}
            Ok(BatchPoll::Done(results)) => {
                for (position, result) in results.iter().enumerate() {
                    // The row is gone when its text changed while the batch was out.
                    let key: Option<RowKey> = database
                        .query_row(
                            "SELECT id, content_sha FROM memory_chunks
                             WHERE batch_id = ? AND batch_pos = ? AND status = 'submitted'",
                            params![batch, position as i64],
                            |row| {
                                Ok(RowKey {
                                    id: row.get(0)?,
                                    sha: row.get(1)?,
                                    status: "submitted".into(),
                                })
                            },
                        )
                        .optional()
                        .map_err(sql)?;
                    match (key, result) {
                        (Some(key), Ok(vector)) => {
                            if store_vector(database, &key, vector, embedder, options.now_ms)? {
                                stats.collected += 1;
                            } else {
                                stats.discarded += 1;
                            }
                        }
                        (Some(_), Err(_)) => {}
                        (None, _) => stats.discarded += 1,
                    }
                }
                // Rows the batch answered with an error, or not at all.
                release_batch(database, &batch, "retry")?;
            }
            Ok(BatchPoll::Failed(reason)) => {
                release_batch(database, &batch, "retry")?;
                stats.notes.push(format!(
                    "{batch} failed ({reason}); re-embedding its chunks now"
                ));
            }
            // A batch that can never be read again (deleted, 404): give up on it.
            Err(error) if !error.retryable => {
                release_batch(database, &batch, "retry")?;
                stats.notes.push(format!(
                    "{batch} could not be read ({}); re-embedding its chunks now",
                    error
                        .status
                        .map_or(error.message, |status| format!("HTTP {status}"))
                ));
            }
            Err(error) => stats.notes.push(error.message),
        }
    }

    let batching = embedder.batch_supported() && !options.immediate;
    let standard = rows_with_status(
        database,
        if batching {
            &["retry"]
        } else {
            &["retry", "pending"]
        },
    )?;
    if !embed_rows(database, embedder, &standard, options, &mut stats)? || !batching {
        return Ok(stats);
    }

    let pending = rows_with_status(database, &["pending"])?;
    let mut start = 0;
    while start < pending.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < pending.len() && end - start < BATCH_CHUNKS {
            bytes += pending[end].title.len() + pending[end].text.len();
            if end > start && bytes > BATCH_BYTES {
                break;
            }
            end += 1;
        }
        let first = start;
        let group = &pending[start..end];
        start = end;
        let texts: Vec<String> = group
            .iter()
            .map(|row| embed::format_document(model, &row.title, &row.text))
            .collect();
        if !take_lease(database)? {
            stats.notes.push(LEASE_LOST.into());
            break;
        }
        match embedder.submit_batch(&texts) {
            Ok(batch) => {
                let transaction = database.unchecked_transaction().map_err(sql)?;
                transaction
                    .execute(
                        "INSERT INTO memory_embed_batches (batch_id, submitted_at, chunk_count, model, dims)
                         VALUES (?, ?, ?, ?, ?)",
                        params![batch, options.now_ms, group.len() as i64, model, dims],
                    )
                    .map_err(sql)?;
                for (position, row) in group.iter().enumerate() {
                    transaction
                        .execute(
                            "UPDATE memory_chunks SET status = 'submitted', batch_id = ?, batch_pos = ?
                             WHERE id = ? AND content_sha = ? AND status = 'pending'",
                            params![batch, position as i64, row.key.id, row.key.sha],
                        )
                        .map_err(sql)?;
                }
                transaction.commit().map_err(sql)?;
                stats.submitted += group.len();
            }
            Err(error) if error.retryable => {
                stats.notes.push(error.message);
                break;
            }
            // The key or model has no batch API (Gemini's free tier, for one):
            // the rest of this run uses the standard endpoint.
            Err(error) => {
                stats.notes.push(format!(
                    "batch API unavailable ({}); used the standard endpoint",
                    error
                        .status
                        .map_or(error.message, |status| format!("HTTP {status}"))
                ));
                embed_rows(database, embedder, &pending[first..], options, &mut stats)?;
                break;
            }
        }
    }
    Ok(stats)
}

/// Gives chunks the provider rejected another try.
pub fn requeue_failed(database: &Connection) -> Result<usize, String> {
    database
        .execute(
            "UPDATE memory_chunks SET status = 'pending', model = NULL, dims = NULL
             WHERE status = 'failed'",
            [],
        )
        .map_err(sql)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedStatus {
    pub files: usize,
    /// Gated files and `MEMORY.md` indexes, which are never sent.
    pub skipped_files: usize,
    pub chunks: usize,
    pub done: usize,
    pub pending: usize,
    pub submitted: usize,
    pub retry: usize,
    pub failed: usize,
    /// Chunks that look like they hold a credential; never sent.
    pub withheld: usize,
    pub batches: usize,
}

pub fn status(database: &Connection, root: &str) -> Result<EmbedStatus, String> {
    let count = |query: &str| -> Result<usize, String> {
        database
            .query_row(query, [root], |row| row.get::<_, i64>(0))
            .map(|n| n as usize)
            .map_err(sql)
    };
    let mut status = EmbedStatus {
        files: count("SELECT count(*) FROM memory_files WHERE root = ?")?,
        skipped_files: count(
            "SELECT count(*) FROM memory_files WHERE root = ? AND skipped IS NOT NULL",
        )?,
        batches: database
            .query_row("SELECT count(*) FROM memory_embed_batches", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(sql)? as usize,
        ..EmbedStatus::default()
    };
    let mut statement = database
        .prepare("SELECT status, count(*) FROM memory_chunks WHERE root = ? GROUP BY status")
        .map_err(sql)?;
    let rows = statement
        .query_map([root], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(sql)?;
    for row in rows {
        let (name, n) = row.map_err(sql)?;
        let n = n as usize;
        status.chunks += n;
        match name.as_str() {
            "done" => status.done = n,
            "pending" => status.pending = n,
            "submitted" => status.submitted = n,
            "retry" => status.retry = n,
            "failed" => status.failed = n,
            "withheld" => status.withheld = n,
            _ => {}
        }
    }
    Ok(status)
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    pub path: String,
    pub similarity: f32,
    /// The text of the file's closest chunk.
    pub passage: String,
}

/// The files whose best chunk is close enough to the query vector: exact
/// cosine over every stored vector of this model, best chunk per file.
pub fn nearest(
    database: &Connection,
    root: &str,
    embedder: &dyn Embedder,
    query: &[f32],
    limit: usize,
    keep: &dyn Fn(&str) -> bool,
) -> Result<Vec<SemanticHit>, String> {
    let mut best: HashMap<String, (f32, i64)> = HashMap::new();
    {
        let mut statement = database
            .prepare(
                "SELECT id, path, vec FROM memory_chunks
                 WHERE root = ? AND status = 'done' AND model = ? AND dims = ?",
            )
            .map_err(sql)?;
        let mut rows = statement
            .query(params![root, embedder.model(), embedder.dims() as i64])
            .map_err(sql)?;
        while let Some(row) = rows.next().map_err(sql)? {
            let path = row
                .get_ref(1)
                .map_err(sql)?
                .as_str()
                .map_err(|e| e.to_string())?;
            if !keep(path) {
                continue;
            }
            let bytes = row
                .get_ref(2)
                .map_err(sql)?
                .as_blob()
                .map_err(|e| e.to_string())?;
            if bytes.len() != query.len() * 4 {
                continue;
            }
            let similarity: f32 = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .zip(query)
                .map(|(raw, q)| f32::from_le_bytes(*raw) * q)
                .sum();
            let id: i64 = row.get(0).map_err(sql)?;
            match best.get_mut(path) {
                Some(entry) if entry.0 >= similarity => {}
                Some(entry) => *entry = (similarity, id),
                None => {
                    best.insert(path.to_string(), (similarity, id));
                }
            }
        }
    }
    let mut ranked: Vec<(String, f32, i64)> = best
        .into_iter()
        .map(|(path, (similarity, id))| (path, similarity, id))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let floor = ranked.first().map_or(MIN_SIMILARITY, |best| {
        MIN_SIMILARITY.max(best.1 - SIMILARITY_MARGIN)
    });
    ranked.retain(|hit| hit.1 >= floor);
    ranked.truncate(limit);
    ranked
        .into_iter()
        .map(|(path, similarity, id)| {
            let passage: String = database
                .query_row("SELECT text FROM memory_chunks WHERE id = ?", [id], |row| {
                    row.get(0)
                })
                .map_err(sql)?;
            Ok(SemanticHit {
                path,
                similarity,
                passage,
            })
        })
        .collect()
}

/// One search result: how it matched, and how close its best passage is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedMatch {
    #[serde(flatten)]
    pub found: MemoryMatch,
    /// `keyword`, `semantic`, or `both`.
    #[serde(rename = "match")]
    pub matched: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
}

pub fn keyword_only(keyword: Vec<MemoryMatch>) -> Vec<RankedMatch> {
    keyword
        .into_iter()
        .map(|found| RankedMatch {
            found,
            matched: "keyword",
            similarity: None,
        })
        .collect()
}

/// Reciprocal-rank fusion of the keyword and semantic rankings. A file in
/// both lists keeps its keyword counts and snippets.
pub fn fuse(
    keyword: Vec<MemoryMatch>,
    semantic: Vec<(MemoryMatch, f32)>,
    limit: usize,
) -> Vec<RankedMatch> {
    let mut merged: Vec<(f64, RankedMatch)> = keyword_only(keyword)
        .into_iter()
        .enumerate()
        .map(|(rank, found)| (1.0 / (RRF_K + rank as f64 + 1.0), found))
        .collect();
    for (rank, (found, similarity)) in semantic.into_iter().enumerate() {
        let score = 1.0 / (RRF_K + rank as f64 + 1.0);
        let similarity = Some((f64::from(similarity) * 1000.0).round() / 1000.0);
        match merged
            .iter_mut()
            .find(|(_, existing)| existing.found.path == found.path)
        {
            Some((total, existing)) => {
                *total += score;
                existing.matched = "both";
                existing.similarity = similarity;
            }
            None => merged.push((
                score,
                RankedMatch {
                    found,
                    matched: "semantic",
                    similarity,
                },
            )),
        }
    }
    // Stable: equal scores keep keyword hits ahead of semantic-only ones.
    merged.sort_by(|a, b| b.0.total_cmp(&a.0));
    merged
        .into_iter()
        .take(limit)
        .map(|(_, found)| found)
        .collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SemanticInfo {
    /// Chunks with a vector, and chunks still waiting for one.
    pub vectors: usize,
    pub waiting: usize,
    pub notes: Vec<String>,
}

/// The semantic leg of `memory search`: sync the chunk table, move the queue
/// when it is due, embed the query, and rank files by their closest chunk.
#[allow(clippy::too_many_arguments)]
pub fn semantic_search(
    query: &str,
    root: &str,
    options: &SearchOptions,
    store: &dyn MemoryStore,
    index_path: &str,
    embedder: &dyn Embedder,
    now_ms: i64,
    limit: usize,
) -> Result<(Vec<(MemoryMatch, f32)>, SemanticInfo), String> {
    let mut database = open_store(index_path)?;
    sync(&mut database, root, store, now_ms)?;
    // Before the pending-work gate: a model or size change with nothing
    // queued must still requeue the old vectors.
    invalidate(&database, embedder)?;
    let mut info = SemanticInfo::default();
    let before = status(&database, root)?;
    let due = meta_get(&database, "last_advance_ms")?
        .and_then(|value| value.parse::<i64>().ok())
        .is_none_or(|last| now_ms - last >= ADVANCE_INTERVAL_MS);
    if due && before.pending + before.retry + before.submitted > 0 {
        meta_set(&database, "last_advance_ms", &now_ms.to_string())?;
        let advanced = advance(
            &database,
            embedder,
            &AdvanceOptions {
                now_ms,
                immediate: false,
                timeout: Duration::from_secs(20),
                max_standard: Some(SEARCH_STANDARD_CHUNKS),
                rate_limit_wait: None,
                progress: None,
            },
        )?;
        info.notes = advanced.notes;
    }
    let vector = embedder
        .embed(
            &[embed::format_query(embedder.model(), query)],
            Duration::from_secs(6),
        )
        .map_err(|error| error.message)?
        .pop()
        .ok_or("the embedding provider returned no query vector")?;
    // Both filters run before `nearest` applies its cut-off and limit.
    let project = options.project.as_deref().map(str::to_lowercase);
    let kinds: std::cell::RefCell<HashMap<String, bool>> = Default::default();
    let keep = |path: &str| {
        project.as_deref().is_none_or(|wanted| {
            memory::project_from_path(path)
                .to_lowercase()
                .contains(wanted)
        }) && options.kind.as_deref().is_none_or(|wanted| {
            *kinds
                .borrow_mut()
                .entry(path.to_string())
                .or_insert_with(|| {
                    store.read(path).is_ok_and(|text| {
                        memory::frontmatter(&memory::lines_of(&text))
                            .kind
                            .is_some_and(|kind| kind.eq_ignore_ascii_case(wanted))
                    })
                })
        })
    };
    let hits = nearest(&database, root, embedder, &vector, limit, &keep)?;
    let after = status(&database, root)?;
    info.vectors = after.done;
    info.waiting = after.pending + after.retry + after.submitted;
    Ok((
        hits.into_iter()
            .filter_map(|hit| {
                memory::semantic_match(&hit.path, &hit.passage, options, store)
                    .map(|found| (found, hit.similarity))
            })
            .collect(),
        info,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_client::TempDir;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// Files in memory, with a version counter standing in for mtime.
    #[derive(Default)]
    struct Files(RefCell<BTreeMap<String, (String, i64)>>);

    impl Files {
        fn put(&self, name: &str, text: &str) {
            let path = format!("/mem/-proj/memory/{name}");
            let mut files = self.0.borrow_mut();
            let version = files.get(&path).map_or(1, |(_, v)| v + 1);
            files.insert(path, (text.to_string(), version));
        }
        fn remove(&self, name: &str) {
            self.0
                .borrow_mut()
                .remove(&format!("/mem/-proj/memory/{name}"));
        }
    }

    impl MemoryStore for Files {
        fn glob(&self, _root: &str) -> Result<Vec<String>, String> {
            Ok(self.0.borrow().keys().cloned().collect())
        }
        fn read(&self, path: &str) -> Result<String, String> {
            self.0
                .borrow()
                .get(path)
                .map(|(text, _)| text.clone())
                .ok_or_else(|| "missing".into())
        }
        fn stat(&self, path: &str) -> Option<(u64, i64)> {
            self.0
                .borrow()
                .get(path)
                .map(|(text, version)| (text.len() as u64, *version))
        }
    }

    /// Vectors by topic word, so "meaning" is testable: texts that mention a
    /// topic or one of its synonyms point the same way.
    struct Fake {
        model: String,
        batch: bool,
        sent: RefCell<Vec<String>>,
        batches: RefCell<Vec<(Vec<String>, bool)>>,
        reject: Option<&'static str>,
        /// Standard calls that answer HTTP 429 before one succeeds.
        limited: std::cell::Cell<usize>,
        /// What every poll answers instead of the batch state.
        poll_error: Option<EmbedError>,
        /// What every batch submission answers, and how many were tried.
        submit_error: Option<EmbedError>,
        submits: std::cell::Cell<usize>,
        /// Runs inside each standard call, while the provider "works".
        on_embed: Option<Box<dyn Fn()>>,
    }

    impl Fake {
        fn new(batch: bool) -> Self {
            Fake {
                model: "fake-1".into(),
                batch,
                sent: RefCell::new(Vec::new()),
                batches: RefCell::new(Vec::new()),
                reject: None,
                limited: std::cell::Cell::new(0),
                poll_error: None,
                submit_error: None,
                submits: std::cell::Cell::new(0),
                on_embed: None,
            }
        }
        fn vector(text: &str) -> Vec<f32> {
            let lower = text.to_lowercase();
            let topics: [&[&str]; 3] = [
                &["disk", "storage", "partition", "space"],
                &["deploy", "release", "ship"],
                &["pancake", "breakfast"],
            ];
            let mut vector: Vec<f32> = topics
                .iter()
                .map(|words| {
                    if words.iter().any(|word| lower.contains(word)) {
                        1.0
                    } else {
                        0.0
                    }
                })
                .collect();
            vector.push(0.05);
            embed::normalize(&mut vector);
            vector
        }
        fn finish_batches(&self) {
            for batch in self.batches.borrow_mut().iter_mut() {
                batch.1 = true;
            }
        }
    }

    impl Embedder for Fake {
        fn model(&self) -> &str {
            &self.model
        }
        fn dims(&self) -> usize {
            4
        }
        fn embed(&self, texts: &[String], _: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
            if self.limited.get() > 0 {
                self.limited.set(self.limited.get() - 1);
                return Err(EmbedError {
                    retryable: true,
                    status: Some(429),
                    message: "slow down".into(),
                });
            }
            if let Some(bad) = self.reject
                && texts.iter().any(|text| text.contains(bad))
            {
                return Err(EmbedError {
                    retryable: false,
                    status: Some(400),
                    message: "rejected".into(),
                });
            }
            if let Some(hook) = &self.on_embed {
                hook();
            }
            self.sent.borrow_mut().extend(texts.iter().cloned());
            Ok(texts.iter().map(|text| Fake::vector(text)).collect())
        }
        fn batch_supported(&self) -> bool {
            self.batch
        }
        fn submit_batch(&self, texts: &[String]) -> Result<String, EmbedError> {
            self.submits.set(self.submits.get() + 1);
            if let Some(error) = &self.submit_error {
                return Err(error.clone());
            }
            let mut batches = self.batches.borrow_mut();
            batches.push((texts.to_vec(), false));
            self.sent.borrow_mut().extend(texts.iter().cloned());
            Ok(format!("batches/{}", batches.len() - 1))
        }
        fn poll_batch(&self, name: &str) -> Result<BatchPoll, EmbedError> {
            if let Some(error) = &self.poll_error {
                return Err(error.clone());
            }
            let index: usize = name.trim_start_matches("batches/").parse().unwrap();
            let (texts, done) = &self.batches.borrow()[index];
            Ok(if *done {
                BatchPoll::Done(texts.iter().map(|text| Ok(Fake::vector(text))).collect())
            } else {
                BatchPoll::Running
            })
        }
        fn cancel_batch(&self, _: &str) {}
    }

    fn note(description: &str, body: &str) -> String {
        format!(
            "---\nname: n\ndescription: {description}\nmetadata:\n  type: project\n---\n\n{body}"
        )
    }

    fn open(dir: &TempDir) -> Connection {
        open_store(dir.path().join("index.sqlite").to_str().unwrap()).unwrap()
    }

    fn options(now_ms: i64, immediate: bool) -> AdvanceOptions<'static> {
        AdvanceOptions {
            now_ms,
            immediate,
            timeout: Duration::from_secs(1),
            max_standard: None,
            rate_limit_wait: Some(Duration::ZERO),
            progress: None,
        }
    }

    fn paths(database: &Connection, fake: &Fake, query: &str) -> Vec<String> {
        nearest(database, "/mem", fake, &Fake::vector(query), 10, &|_| true)
            .unwrap()
            .into_iter()
            .map(|hit| hit.path.rsplit('/').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn chunks_split_at_headings_and_carry_the_file_context() {
        let text = note(
            "Disk notes",
            "Intro line.\n\n## Cleanup\nStep one.\n\nStep two.\n\n## Other\nTail.",
        );
        let chunks = chunk_file("-proj", "disk.md", &text).unwrap();
        let texts: Vec<&str> = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Intro line.",
                "## Cleanup\nStep one.\n\nStep two.",
                "## Other\nTail."
            ]
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.title == "disk [-proj]: Disk notes")
        );

        // Editing one section changes only that section's hash.
        let edited = chunk_file("-proj", "disk.md", &text.replace("Tail.", "New tail.")).unwrap();
        assert_eq!(chunks[0].sha, edited[0].sha);
        assert_eq!(chunks[1].sha, edited[1].sha);
        assert_ne!(chunks[2].sha, edited[2].sha);
    }

    #[test]
    fn long_text_stays_under_the_chunk_cap_without_losing_words() {
        let paragraph = |word: &str| format!("{word} ").repeat(300);
        let digits: String = (0..3000).map(|n| n.to_string()).collect();
        assert!(digits.len() > MAX_CHARS * 2);
        let body = format!(
            "{}\n\n{}\n\n{digits}",
            paragraph("alpha"),
            paragraph("beta")
        );
        let chunks = chunk_file("-proj", "long.md", &body).unwrap();
        assert_eq!(chunks.len(), 5);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.text.chars().count() <= MAX_CHARS)
        );
        let rejoined: String = chunks[2..]
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect();
        assert_eq!(rejoined, digits);
    }

    #[test]
    fn gated_files_indexes_and_credentials_are_never_sent() {
        let files = Files::default();
        files.put("MEMORY.md", "- [Gated thing](x.md) disk storage");
        files.put("DO NOT READ UNLESS X MENTIONED.md", "disk secret plan");
        files.put(
            "quiet.md",
            &note(
                "Do not read unless the vendor is named",
                "disk vendor notes",
            ),
        );
        files.put(
            "keys.md",
            "disk key sk-abcdefghijklmnopqrstuvwxyz012345\n\n## Safe\ndisk cleanup steps",
        );
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 1).unwrap();
        let fake = Fake::new(false);
        advance(&database, &fake, &options(1, true)).unwrap();
        let sent = fake.sent.borrow().join("\n");
        assert!(sent.contains("disk cleanup steps"));
        for hidden in ["Gated thing", "secret plan", "vendor notes", "sk-abc"] {
            assert!(!sent.contains(hidden), "{hidden} was sent");
        }
        let stored: String = database
            .query_row(
                "SELECT group_concat(text || title) FROM memory_chunks",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains("sk-abc"));
        let state = status(&database, "/mem").unwrap();
        assert_eq!((state.skipped_files, state.withheld, state.done), (3, 1, 1));
    }

    #[test]
    fn an_edit_replaces_its_chunk_and_a_stale_batch_result_is_discarded() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.\n\n## Deploy\nShip on Friday.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 1).unwrap();
        let fake = Fake::new(true);
        let first = advance(&database, &fake, &options(1, false)).unwrap();
        assert_eq!((first.submitted, first.embedded), (2, 0));
        // Until the batch returns, nothing is searchable by meaning.
        assert!(paths(&database, &fake, "storage").is_empty());

        // The disk passage changes topic while the batch is out.
        files.put(
            "a.md",
            "Pancakes for breakfast.\n\n## Deploy\nShip on Friday.",
        );
        let synced = sync(&mut database, "/mem", &files, 2).unwrap();
        assert_eq!((synced.chunks_added, synced.chunks_removed), (1, 1));

        fake.finish_batches();
        let second = advance(&database, &fake, &options(2, false)).unwrap();
        assert_eq!(
            (second.collected, second.discarded, second.submitted),
            (1, 1, 1)
        );
        // The old wording's vector never lands: a disk query finds nothing.
        assert!(paths(&database, &fake, "storage").is_empty());
        assert_eq!(paths(&database, &fake, "release"), ["a.md"]);

        fake.finish_batches();
        advance(&database, &fake, &options(3, false)).unwrap();
        assert_eq!(paths(&database, &fake, "breakfast"), ["a.md"]);

        files.remove("a.md");
        sync(&mut database, "/mem", &files, 4).unwrap();
        assert!(paths(&database, &fake, "breakfast").is_empty());
        assert_eq!(status(&database, "/mem").unwrap().chunks, 0);
    }

    #[test]
    fn unchanged_files_are_not_sent_twice_and_a_model_change_requeues_everything() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.");
        files.put("b.md", "Deploy on Friday.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        let mut fake = Fake::new(false);
        for round in 1..=2 {
            sync(&mut database, "/mem", &files, round).unwrap();
            advance(&database, &fake, &options(round, false)).unwrap();
        }
        assert_eq!(fake.sent.borrow().len(), 2);

        fake.model = "fake-2".into();
        // Old vectors belong to another model and must not rank.
        assert!(paths(&database, &fake, "storage").is_empty());
        advance(&database, &fake, &options(3, false)).unwrap();
        assert_eq!(fake.sent.borrow().len(), 4);
        assert_eq!(paths(&database, &fake, "storage"), ["a.md"]);
    }

    #[test]
    fn a_batch_out_for_a_day_and_a_rejected_text_fall_back_cleanly() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.");
        files.put("b.md", "POISON deploy text.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 0).unwrap();
        let mut fake = Fake::new(true);
        fake.reject = Some("POISON");
        assert_eq!(
            advance(&database, &fake, &options(0, false))
                .unwrap()
                .submitted,
            2
        );
        // A day later the batch is still running: its chunks go standard.
        let late = advance(&database, &fake, &options(BATCH_PATIENCE_MS + 1, false)).unwrap();
        assert_eq!((late.embedded, late.failed), (1, 1));
        assert_eq!(paths(&database, &fake, "storage"), ["a.md"]);
        let state = status(&database, "/mem").unwrap();
        assert_eq!((state.done, state.failed, state.batches), (1, 1, 0));
        // The rejected chunk is not retried on every run.
        let again = advance(&database, &fake, &options(BATCH_PATIENCE_MS + 2, false)).unwrap();
        assert_eq!((again.embedded, again.failed, again.submitted), (0, 0, 0));
    }

    #[test]
    fn a_rate_limit_is_waited_out_by_embed_and_stops_a_search_without_losing_the_queue() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 1).unwrap();
        let fake = Fake::new(false);
        fake.limited.set(1);
        let impatient = AdvanceOptions {
            rate_limit_wait: None,
            ..options(1, false)
        };
        let stopped = advance(&database, &fake, &impatient).unwrap();
        assert_eq!((stopped.embedded, stopped.notes.len()), (0, 1));
        assert_eq!(status(&database, "/mem").unwrap().pending, 1);

        fake.limited.set(2);
        let waited = advance(&database, &fake, &options(2, false)).unwrap();
        assert_eq!((waited.embedded, waited.notes.len()), (1, 0));
    }

    fn found(name: &str) -> MemoryMatch {
        MemoryMatch {
            project: "-proj".into(),
            name: name.into(),
            path: format!("/mem/-proj/memory/{name}"),
            count: 1,
            snippets: vec![],
            description: None,
            kind: None,
        }
    }

    #[test]
    fn fusion_puts_files_found_both_ways_first_and_labels_each_match() {
        let ranked = fuse(
            vec![found("k1.md"), found("both.md")],
            vec![(found("s1.md"), 0.9), (found("both.md"), 0.8123456)],
            3,
        );
        let summary: Vec<(&str, &str, Option<f64>)> = ranked
            .iter()
            .map(|m| (m.found.name.as_str(), m.matched, m.similarity))
            .collect();
        assert_eq!(
            summary,
            [
                ("both.md", "both", Some(0.812)),
                ("k1.md", "keyword", None),
                ("s1.md", "semantic", Some(0.9)),
            ]
        );
    }

    #[test]
    fn semantic_search_finds_a_paraphrase_and_honors_the_filters() {
        let files = Files::default();
        files.put(
            "space.md",
            &note("Root partition", "The root partition ran out of room."),
        );
        files.put("ship.md", &note("Releases", "Deploy on Friday."));
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let index = dir.path().join("index.sqlite");
        let index = index.to_str().unwrap();
        let fake = Fake::new(false);
        let search = |query: &str, options: &SearchOptions| {
            semantic_search(query, "/mem", options, &files, index, &fake, 1, 10)
                .unwrap()
                .0
                .into_iter()
                .map(|(found, _)| found.name)
                .collect::<Vec<_>>()
        };
        let all = SearchOptions {
            limit: 10,
            snippets: 2,
            ..SearchOptions::default()
        };
        // "storage" appears in neither file; the first search also embeds them.
        assert_eq!(search("storage is full", &all), ["space.md"]);
        let other_project = SearchOptions {
            project: Some("elsewhere".into()),
            ..all.clone()
        };
        assert!(search("storage is full", &other_project).is_empty());
        let other_type = SearchOptions {
            kind: Some("feedback".into()),
            ..all.clone()
        };
        assert!(search("storage is full", &other_type).is_empty());
    }

    #[test]
    fn every_part_of_an_oversized_credential_block_is_withheld() {
        let key: String = (0..100)
            .map(|_| "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj\n")
            .collect();
        assert!(key.len() > MAX_CHARS);
        let body = format!(
            "Before.\n\n-----BEGIN PRIVATE KEY-----\n{key}-----END PRIVATE KEY-----\n\nAfter."
        );
        let chunks = chunk_file("-proj", "k.md", &body).unwrap();
        assert!(chunks.iter().any(|chunk| chunk.withheld));
        for chunk in chunks.iter().filter(|chunk| !chunk.withheld) {
            assert!(!chunk.text.contains("MIIEvQ"), "sendable: {}", chunk.text);
        }
        assert!(
            chunks
                .iter()
                .any(|chunk| !chunk.withheld && chunk.text == "After.")
        );
    }

    #[test]
    fn a_gate_marker_in_a_multi_line_frontmatter_field_gates_the_file() {
        for field in [
            "description: >\n  Do not read unless the vendor is named\n",
            "name: |\n  DO NOT READ UNLESS asked\n",
            "description: Vendor notes,\n  do not read unless asked\n",
        ] {
            let text = format!("---\n{field}metadata:\n  type: project\n---\n\nvendor notes");
            assert_eq!(chunk_file("-proj", "v.md", &text), Err("gated"), "{field}");
        }
        let elsewhere = "---\nname: v\nmetadata:\n  note: do not read unless\n---\n\nbody";
        assert!(chunk_file("-proj", "v.md", elsewhere).is_ok());
    }

    #[test]
    fn a_vector_lands_only_on_the_row_that_still_holds_its_text() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 1).unwrap();
        let path = dir
            .path()
            .join("index.sqlite")
            .to_str()
            .unwrap()
            .to_string();
        let mut fake = Fake::new(false);
        // While the provider works, another process gives the row's id other text.
        fake.on_embed = Some(Box::new(move || {
            open_store(&path)
                .unwrap()
                .execute(
                    "UPDATE memory_chunks SET content_sha = 'other', text = 'Pancakes.'",
                    [],
                )
                .unwrap();
        }));
        let stats = advance(&database, &fake, &options(1, false)).unwrap();
        assert_eq!((stats.embedded, stats.discarded), (0, 1));
        let state = status(&database, "/mem").unwrap();
        assert_eq!((state.done, state.pending), (0, 1));
    }

    #[test]
    fn a_search_requeues_old_vectors_after_a_model_change_with_nothing_pending() {
        let files = Files::default();
        files.put(
            "space.md",
            &note("Root partition", "The root partition ran out of room."),
        );
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let index = dir.path().join("index.sqlite");
        let index = index.to_str().unwrap();
        let mut fake = Fake::new(false);
        {
            let mut database = open_store(index).unwrap();
            sync(&mut database, "/mem", &files, 1).unwrap();
            advance(&database, &fake, &options(1, false)).unwrap();
            assert_eq!(status(&database, "/mem").unwrap().pending, 0);
        }
        fake.model = "fake-2".into();
        let all = SearchOptions {
            limit: 10,
            snippets: 2,
            ..SearchOptions::default()
        };
        let (found, _) =
            semantic_search("storage is full", "/mem", &all, &files, index, &fake, 2, 10).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_batch_that_cannot_be_polled_is_released() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 0).unwrap();
        let mut fake = Fake::new(true);
        assert_eq!(
            advance(&database, &fake, &options(0, false))
                .unwrap()
                .submitted,
            1
        );

        // A transient poll failure is waited out for a day, and no longer.
        fake.poll_error = Some(EmbedError {
            retryable: true,
            status: Some(503),
            message: "busy".into(),
        });
        advance(&database, &fake, &options(1, false)).unwrap();
        assert_eq!(status(&database, "/mem").unwrap().submitted, 1);
        advance(&database, &fake, &options(BATCH_PATIENCE_MS + 1, false)).unwrap();
        let state = status(&database, "/mem").unwrap();
        assert_eq!((state.done, state.submitted, state.batches), (1, 0, 0));

        // A batch that is gone (HTTP 404) is released at once.
        files.put("b.md", "Deploy on Friday.");
        sync(&mut database, "/mem", &files, 1).unwrap();
        fake.poll_error = None;
        let later = BATCH_PATIENCE_MS + 2;
        assert_eq!(
            advance(&database, &fake, &options(later, false))
                .unwrap()
                .submitted,
            1
        );
        fake.poll_error = Some(EmbedError {
            retryable: false,
            status: Some(404),
            message: "not found".into(),
        });
        advance(&database, &fake, &options(later + 1, false)).unwrap();
        let state = status(&database, "/mem").unwrap();
        assert_eq!((state.done, state.submitted, state.batches), (2, 0, 0));
    }

    #[test]
    fn a_second_process_does_not_embed_while_another_holds_the_lease() {
        let files = Files::default();
        files.put("a.md", "The disk filled up.");
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 1).unwrap();
        let fake = Fake::new(false);
        meta_set(
            &database,
            LEASE_KEY,
            &format!("{}:other", wall_ms() + 60_000),
        )
        .unwrap();
        let skipped = advance(&database, &fake, &options(1, false)).unwrap();
        assert_eq!(skipped.notes, [LEASE_HELD]);
        assert!(fake.sent.borrow().is_empty());

        // An expired lease is taken over, and released when the run ends.
        meta_set(&database, LEASE_KEY, &format!("{}:other", wall_ms() - 1)).unwrap();
        assert_eq!(
            advance(&database, &fake, &options(2, false))
                .unwrap()
                .embedded,
            1
        );
        assert_eq!(meta_get(&database, LEASE_KEY).unwrap(), None);
    }

    #[test]
    fn the_type_filter_runs_before_the_similarity_cut_off() {
        let typed = |kind: &str, body: &str| {
            format!(
                "---\nname: n\ndescription: {kind} note\nmetadata:\n  type: {kind}\n---\n\n{body}"
            )
        };
        let files = Files::default();
        // The closest file overall is feedback; the project file is further off.
        files.put("disk.md", &typed("feedback", "The disk filled up."));
        files.put(
            "mixed.md",
            &typed("project", "The disk filled up before the deploy."),
        );
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let index = dir.path().join("index.sqlite");
        let index = index.to_str().unwrap();
        let fake = Fake::new(false);
        let projects = SearchOptions {
            limit: 10,
            snippets: 2,
            kind: Some("project".into()),
            ..SearchOptions::default()
        };
        let (found, _) =
            semantic_search("storage", "/mem", &projects, &files, index, &fake, 1, 10).unwrap();
        let names: Vec<&str> = found.iter().map(|(found, _)| found.name.as_str()).collect();
        assert_eq!(names, ["mixed.md"]);
    }

    #[test]
    fn a_rejected_batch_submit_is_tried_once_per_run_with_a_one_line_note() {
        let files = Files::default();
        for n in 0..=BATCH_CHUNKS {
            files.put(&format!("n{n}.md"), "The disk filled up.");
        }
        let dir = TempDir::new("dejavu-embed-").unwrap();
        let mut database = open(&dir);
        sync(&mut database, "/mem", &files, 1).unwrap();
        let mut fake = Fake::new(true);
        fake.submit_error = Some(EmbedError {
            retryable: false,
            status: Some(400),
            message:
                "embedding provider returned HTTP 400: {\n  \"error\": \"FAILED_PRECONDITION\"\n}"
                    .into(),
        });
        let stats = advance(&database, &fake, &options(1, false)).unwrap();
        assert_eq!((fake.submits.get(), stats.embedded), (1, BATCH_CHUNKS + 1));
        assert_eq!(
            stats.notes,
            ["batch API unavailable (HTTP 400); used the standard endpoint"]
        );
    }
}
