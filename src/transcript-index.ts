import { Database } from "bun:sqlite";
import { mkdir, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { openCodeLocator } from "./opencode-store.ts";
import { extractVisibleMessage } from "./session-reader.ts";
import { compactHome, dateFromPath, projectFromTranscriptPath, projectFromTranscriptText, snippetAround } from "./transcript-paths.ts";
import type { StoreDiagnostic, StoreSearchMatch, TranscriptSource, TranscriptStore } from "./transcript-types.ts";

const SCHEMA_VERSION = 2;
/** Bytes hashed at the start of each JSONL file to detect in-place rewrites versus appends. */
const HEAD_BYTES = 4096;

export interface IndexRefreshResult {
  path: string;
  files: number;
  messages: number;
  indexed: number;
  removed: number;
  skipped: StoreDiagnostic[];
  elapsedMs: number;
}

export interface IndexedFileMatch {
  store: string;
  source: TranscriptSource;
  path: string;
  count: number;
}

export interface IndexedStoreMatch extends StoreSearchMatch {
  store: string;
}

export interface TranscriptIndexStatus {
  path: string;
  exists: boolean;
  schemaVersion: number;
  files: number;
  messages: number;
  bytes: number;
}

interface FileRow {
  path: string;
  source: TranscriptSource;
  store: string;
  size: number;
  mtime_ms: number;
  indexed_bytes: number;
  head_bytes: number;
  head: string;
  project: string;
}

export function defaultIndexPath(): string {
  const cacheRoot = process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache");
  return process.env.DEJAVU_INDEX_PATH ?? join(cacheRoot, "dejavu", "transcripts.sqlite");
}

function storedSchemaVersion(database: Database): number {
  const table = database.query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'metadata'").get();
  if (!table) return 0;
  const row = database.query("SELECT value FROM metadata WHERE key = 'schema_version'").get() as { value: string } | null;
  return row ? Number.parseInt(row.value, 10) || 0 : 0;
}

function dropSchema(database: Database): void {
  for (const name of ["messages", "message_rows", "files", "opencode_cursors", "metadata"]) database.run(`DROP TABLE IF EXISTS ${name}`);
}

function createSchema(database: Database): void {
  database.run("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
  database.run(`CREATE TABLE files (
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
  )`);
  database.run("CREATE TABLE opencode_cursors (store TEXT PRIMARY KEY, time_updated INTEGER NOT NULL, part_id TEXT NOT NULL)");
  database.run(`CREATE TABLE message_rows (
    id INTEGER PRIMARY KEY,
    store TEXT NOT NULL,
    path TEXT NOT NULL,
    source TEXT NOT NULL,
    role TEXT NOT NULL,
    date TEXT NOT NULL,
    project TEXT NOT NULL,
    key TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL
  )`);
  database.run("CREATE INDEX message_rows_path ON message_rows (path)");
  database.run("CREATE INDEX message_rows_store_key ON message_rows (store, key)");
  // External-content FTS: rows live in message_rows so deletes by path or key use B-tree indexes
  // instead of scanning the full-text table.
  database.run("CREATE VIRTUAL TABLE messages USING fts5(text, content='message_rows', content_rowid='id', tokenize='trigram')");
  database.run(`CREATE TRIGGER message_rows_ai AFTER INSERT ON message_rows BEGIN
    INSERT INTO messages(rowid, text) VALUES (new.id, new.text);
  END`);
  database.run(`CREATE TRIGGER message_rows_ad AFTER DELETE ON message_rows BEGIN
    INSERT INTO messages(messages, rowid, text) VALUES ('delete', old.id, old.text);
  END`);
  database.run("INSERT INTO metadata (key, value) VALUES ('schema_version', ?)", [String(SCHEMA_VERSION)]);
}

function openIndex(path: string, options: { create?: boolean; readonly?: boolean } = {}): Database {
  const database = new Database(path, { create: options.create ?? true, readonly: options.readonly ?? false });
  if (options.readonly) return database;
  database.run("PRAGMA journal_mode = WAL");
  database.run("PRAGMA synchronous = NORMAL");
  if (storedSchemaVersion(database) !== SCHEMA_VERSION) {
    dropSchema(database);
    createSchema(database);
  }
  return database;
}

async function jsonlFiles(store: TranscriptStore): Promise<string[]> {
  const paths: string[] = [];
  const glob = new Bun.Glob("**/*.jsonl");
  for await (const path of glob.scan({ cwd: store.path, absolute: true, onlyFiles: true })) paths.push(path);
  return paths;
}

async function hashHead(path: string, bytes: number): Promise<string> {
  if (bytes <= 0) return "";
  return String(Bun.hash(await Bun.file(path).slice(0, bytes).arrayBuffer()));
}

function diagnostic(store: TranscriptStore, error: unknown): StoreDiagnostic {
  return { source: store.source, path: store.path, error: error instanceof Error ? error.message : String(error) };
}

export async function refreshTranscriptIndex(
  stores: TranscriptStore[],
  path = defaultIndexPath(),
  rebuild = false,
): Promise<IndexRefreshResult> {
  const startedAt = Date.now();
  await mkdir(dirname(path), { recursive: true });
  const database = openIndex(path);
  try {
    if (rebuild) {
      dropSchema(database);
      createSchema(database);
    }
    let indexed = 0;
    let removed = 0;
    const skipped: StoreDiagnostic[] = [];
    for (const store of stores) {
      try {
        const outcome = store.kind === "sqlite" ? refreshOpenCodeStore(database, store) : await refreshJsonlStore(database, store);
        indexed += outcome.indexed;
        removed += outcome.removed;
      } catch (error) {
        skipped.push(diagnostic(store, error));
      }
    }
    if (rebuild) database.run("VACUUM");
    const totals = database.query("SELECT COUNT(*) AS files, (SELECT COUNT(*) FROM message_rows) AS messages FROM files").get() as { files: number; messages: number };
    return { path, ...totals, indexed, removed, skipped, elapsedMs: Date.now() - startedAt };
  } finally {
    database.close();
  }
}

async function refreshJsonlStore(database: Database, store: TranscriptStore): Promise<{ indexed: number; removed: number }> {
  const livePaths = new Set(await jsonlFiles(store));
  const knownRows = database.query("SELECT * FROM files WHERE store = ?").all(store.path) as FileRow[];
  const known = new Map(knownRows.map((row) => [row.path, row]));
  const insert = database.prepare("INSERT INTO message_rows (store, path, source, role, date, project, text) VALUES (?, ?, ?, ?, ?, ?, ?)");
  const upsertFile = database.prepare(`INSERT OR REPLACE INTO files
    (path, source, store, size, mtime_ms, message_count, indexed_bytes, head_bytes, head, project)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);

  const indexLines = (filePath: string, text: string, fileProject: string, fileDate: string): number => {
    let count = 0;
    for (const line of text.split("\n")) {
      const message = extractVisibleMessage(line, store.source);
      if (!message) continue;
      insert.run(store.path, filePath, store.source, message.role, message.date ?? fileDate, compactHome(message.project ?? fileProject), message.text);
      count++;
    }
    return count;
  };

  const replaceFile = database.transaction((filePath: string, size: number, mtimeMs: number, text: string, head: string, headBytes: number) => {
    database.run("DELETE FROM message_rows WHERE path = ?", [filePath]);
    const fileProject = projectFromTranscriptText(filePath, store.source, text);
    const count = indexLines(filePath, text, fileProject, dateFromPath(filePath));
    upsertFile.run(filePath, store.source, store.path, size, mtimeMs, count, size, headBytes, head, fileProject);
  });

  const appendFile = database.transaction((previous: FileRow, size: number, mtimeMs: number, tail: string) => {
    // Only consume through the last newline so a half-written trailing line is picked up next time.
    const end = tail.lastIndexOf("\n");
    const complete = end >= 0 ? tail.slice(0, end + 1) : "";
    const count = indexLines(previous.path, complete, previous.project, dateFromPath(previous.path));
    const messageCount = (database.query("SELECT message_count FROM files WHERE path = ?").get(previous.path) as { message_count: number }).message_count + count;
    const indexedBytes = previous.indexed_bytes + Buffer.byteLength(complete);
    upsertFile.run(previous.path, previous.source, store.path, size, mtimeMs, messageCount, indexedBytes, previous.head_bytes, previous.head, previous.project);
  });

  let indexed = 0;
  for (const filePath of livePaths) {
    const fileStat = await stat(filePath);
    const previous = known.get(filePath);
    if (previous && previous.size === fileStat.size && previous.mtime_ms === fileStat.mtimeMs) continue;
    const appendOnly = previous
      && fileStat.size >= previous.size
      && previous.indexed_bytes <= fileStat.size
      && previous.head === await hashHead(filePath, previous.head_bytes);
    if (appendOnly) {
      appendFile(previous, fileStat.size, fileStat.mtimeMs, await Bun.file(filePath).slice(previous.indexed_bytes).text());
    } else {
      const headBytes = Math.min(HEAD_BYTES, fileStat.size);
      replaceFile(filePath, fileStat.size, fileStat.mtimeMs, await Bun.file(filePath).text(), await hashHead(filePath, headBytes), headBytes);
    }
    indexed++;
  }

  const removeFile = database.transaction((filePath: string) => {
    database.run("DELETE FROM message_rows WHERE path = ?", [filePath]);
    database.run("DELETE FROM files WHERE path = ?", [filePath]);
  });
  let removed = 0;
  for (const row of knownRows) {
    if (livePaths.has(row.path)) continue;
    removeFile(row.path);
    removed++;
  }
  return { indexed, removed };
}

interface OpenCodePartRow {
  part_id: string;
  time_updated: number;
  session_id: string;
  directory: string;
  title: string;
  session_updated: number;
  role: string | null;
  text: string | null;
}

function refreshOpenCodeStore(database: Database, store: TranscriptStore): { indexed: number; removed: number } {
  const cursor = database.query("SELECT time_updated, part_id FROM opencode_cursors WHERE store = ?").get(store.path) as
    { time_updated: number; part_id: string } | null ?? { time_updated: -1, part_id: "" };
  const source = new Database(store.path, { readonly: true, strict: true });
  try {
    // Text parts are re-read whenever OpenCode touches them, so streamed parts converge once they finish.
    const rows = source.query<OpenCodePartRow, [number, string]>(`
      SELECT p.id AS part_id, p.time_updated, s.id AS session_id, s.directory, s.title, s.time_updated AS session_updated,
             json_extract(m.data, '$.role') AS role, json_extract(p.data, '$.text') AS text
      FROM part p
      JOIN message m ON m.id = p.message_id
      JOIN session s ON s.id = p.session_id
      WHERE (p.time_updated > ?1 OR (p.time_updated = ?1 AND p.id > ?2))
        AND json_extract(p.data, '$.type') = 'text'
      ORDER BY p.time_updated, p.id
    `).all(cursor.time_updated, cursor.part_id);
    if (rows.length === 0) return { indexed: 0, removed: 0 };
    const insert = database.prepare("INSERT INTO message_rows (store, path, source, role, date, project, key, text) VALUES (?, ?, ?, ?, ?, ?, ?, ?)");
    const remove = database.prepare("DELETE FROM message_rows WHERE store = ? AND key = ?");
    const apply = database.transaction(() => {
      let indexed = 0;
      for (const row of rows) {
        remove.run(store.path, row.part_id);
        if (!row.text) continue;
        insert.run(
          store.path,
          openCodeLocator(store.path, row.session_id),
          "opencode",
          row.role || "unknown",
          new Date(row.session_updated).toISOString().slice(0, 10),
          compactHome(row.directory || row.title || "~"),
          row.part_id,
          row.text,
        );
        indexed++;
      }
      const last = rows.at(-1)!;
      database.run("INSERT OR REPLACE INTO opencode_cursors (store, time_updated, part_id) VALUES (?, ?, ?)", [store.path, last.time_updated, last.part_id]);
      return indexed;
    });
    return { indexed: apply(), removed: 0 };
  } finally {
    source.close();
  }
}

function ftsLiteral(query: string): string {
  return `"${query.replaceAll('"', '""')}"`;
}

interface GroupedRow {
  store: string;
  path: string;
  source: TranscriptSource;
  count: number;
  date: string;
  project: string;
}

/**
 * Ranks transcripts by literal occurrence count, aggregated in SQL so only one row per transcript
 * crosses into JavaScript. The trigram match already guarantees a case-folded substring hit, so a
 * count that SQLite's ASCII-only lower() misses is floored at one instead of being dropped.
 */
function groupedMatches(database: Database, query: string, stores: TranscriptStore[], limit: number, tieBreak: "path" | "date"): GroupedRow[] {
  const placeholders = stores.map(() => "?").join(", ");
  const lowered = query.toLowerCase();
  return database.query(`
    SELECT store, path, source, SUM(MAX(1, occurrences)) AS count, MIN(date) AS date, MIN(project) AS project
    FROM (
      SELECT r.store, r.path, r.source, r.date, r.project,
             (length(lower(r.text)) - length(replace(lower(r.text), ?, ''))) / length(?) AS occurrences
      FROM messages
      JOIN message_rows r ON r.id = messages.rowid
      WHERE messages MATCH ? AND r.store IN (${placeholders})
    )
    GROUP BY path
    ORDER BY count DESC, ${tieBreak} DESC
    LIMIT ?
  `).all(lowered, lowered, ftsLiteral(query), ...stores.map((store) => store.path), limit) as GroupedRow[];
}

export function searchTranscriptIndex(
  query: string,
  stores: TranscriptStore[],
  path = defaultIndexPath(),
  limit = 200,
): IndexedFileMatch[] {
  if (query.length < 3 || stores.length === 0) return [];
  const database = openIndex(path);
  try {
    return groupedMatches(database, query, stores, limit, "path")
      .map(({ store, source, path: filePath, count }) => ({ store, source, path: filePath, count }));
  } finally {
    database.close();
  }
}

export function searchTranscriptIndexMatches(
  query: string,
  stores: TranscriptStore[],
  path = defaultIndexPath(),
  limit = 40,
  snippetLimit = 3,
): IndexedStoreMatch[] {
  if (query.length < 3 || stores.length === 0) return [];
  const database = openIndex(path);
  try {
    const snippetRows = database.prepare(`
      SELECT r.role, r.text FROM messages JOIN message_rows r ON r.id = messages.rowid
      WHERE messages MATCH ? AND r.path = ? ORDER BY r.id LIMIT ?
    `);
    const literal = ftsLiteral(query);
    return groupedMatches(database, query, stores, limit, "date").map((row) => {
      const rows = snippetRows.all(literal, row.path, snippetLimit) as Array<{ role: string; text: string }>;
      return {
        store: row.store,
        source: row.source,
        path: row.path,
        count: row.count,
        date: row.date || dateFromPath(row.path),
        project: row.project || projectFromTranscriptPath(row.path, row.source),
        snippets: rows.map((snippet) => ({ role: snippet.role, text: snippetAround(snippet.text, query) })),
      };
    });
  } finally {
    database.close();
  }
}

export async function transcriptIndexStatus(path = defaultIndexPath()): Promise<TranscriptIndexStatus> {
  try {
    const fileStat = await stat(path);
    const database = openIndex(path, { readonly: true, create: false });
    try {
      const schemaVersion = storedSchemaVersion(database);
      if (schemaVersion !== SCHEMA_VERSION) return { path, exists: true, schemaVersion, files: 0, messages: 0, bytes: fileStat.size };
      const totals = database.query("SELECT COUNT(*) AS files, (SELECT COUNT(*) FROM message_rows) AS messages FROM files").get() as { files: number; messages: number };
      return { path, exists: true, schemaVersion, ...totals, bytes: fileStat.size };
    } finally {
      database.close();
    }
  } catch {
    return { path, exists: false, schemaVersion: 0, files: 0, messages: 0, bytes: 0 };
  }
}
