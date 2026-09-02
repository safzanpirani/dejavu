import { Database } from "bun:sqlite";
import { mkdir, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { extractVisibleMessage } from "./session-reader.ts";
import type { StoreSearchMatch, TranscriptSource, TranscriptStore } from "./transcript-types.ts";

const SCHEMA_VERSION = 1;

export interface IndexRefreshResult {
  path: string;
  files: number;
  messages: number;
  indexed: number;
  removed: number;
  elapsedMs: number;
}

export interface IndexedFileMatch {
  source: TranscriptSource;
  path: string;
  count: number;
}

export interface TranscriptIndexStatus {
  path: string;
  exists: boolean;
  files: number;
  messages: number;
  bytes: number;
}

export function defaultIndexPath(): string {
  const cacheRoot = process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache");
  return process.env.DEJAVU_INDEX_PATH ?? join(cacheRoot, "dejavu", "transcripts.sqlite");
}

function openIndex(path: string, create = true): Database {
  const database = new Database(path, { create });
  database.run("PRAGMA journal_mode = WAL");
  database.run("PRAGMA synchronous = NORMAL");
  database.run("CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
  database.run(`CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    message_count INTEGER NOT NULL
  )`);
  database.run(`CREATE VIRTUAL TABLE IF NOT EXISTS messages USING fts5(
    path UNINDEXED,
    source UNINDEXED,
    role UNINDEXED,
    date UNINDEXED,
    project UNINDEXED,
    text,
    tokenize='trigram'
  )`);
  database.run("INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?)", [String(SCHEMA_VERSION)]);
  return database;
}

async function jsonlFiles(store: TranscriptStore): Promise<string[]> {
  const paths: string[] = [];
  const glob = new Bun.Glob("**/*.jsonl");
  for await (const path of glob.scan({ cwd: store.path, absolute: true, onlyFiles: true })) paths.push(path);
  return paths;
}

export async function refreshTranscriptIndex(
  stores: TranscriptStore[],
  path = defaultIndexPath(),
  rebuild = false,
): Promise<IndexRefreshResult> {
  const startedAt = Date.now();
  await mkdir(dirname(path), { recursive: true });
  const database = openIndex(path);
  if (rebuild) {
    database.run("DELETE FROM messages");
    database.run("DELETE FROM files");
  }
  const selectedStores = stores.filter((store) => store.kind === "jsonl");
  const selectedSources = new Set(selectedStores.map((store) => store.source));
  const discovered = (await Promise.all(selectedStores.map(async (store) =>
    (await jsonlFiles(store)).map((filePath) => ({ path: filePath, source: store.source })),
  ))).flat();
  const livePaths = new Set(discovered.map((file) => file.path));
  const knownRows = database.query("SELECT path, source, size, mtime_ms FROM files").all() as Array<{
    path: string; source: TranscriptSource; size: number; mtime_ms: number;
  }>;
  const known = new Map(knownRows.map((row) => [row.path, row]));
  let indexed = 0;
  let removed = 0;

  const replaceFile = database.transaction((file: {
    path: string; source: TranscriptSource; size: number; mtimeMs: number; text: string;
  }) => {
    database.run("DELETE FROM messages WHERE path = ?", [file.path]);
    let messageCount = 0;
    const fileProject = projectFromTranscript(file.path, file.source, file.text);
    const fileDate = dateFromTranscriptPath(file.path);
    const insert = database.prepare("INSERT INTO messages (path, source, role, date, project, text) VALUES (?, ?, ?, ?, ?, ?)");
    for (const line of file.text.split("\n")) {
      const message = extractVisibleMessage(line, file.source);
      if (!message) continue;
      insert.run(file.path, file.source, message.role, message.date ?? fileDate, message.project ?? fileProject, message.text);
      messageCount++;
    }
    database.run(
      "INSERT OR REPLACE INTO files (path, source, size, mtime_ms, message_count) VALUES (?, ?, ?, ?, ?)",
      [file.path, file.source, file.size, file.mtimeMs, messageCount],
    );
  });

  for (const file of discovered) {
    const fileStat = await stat(file.path);
    const previous = known.get(file.path);
    if (previous && previous.size === fileStat.size && previous.mtime_ms === fileStat.mtimeMs) continue;
    replaceFile({ path: file.path, source: file.source, size: fileStat.size, mtimeMs: fileStat.mtimeMs, text: await Bun.file(file.path).text() });
    indexed++;
  }

  const removeFile = database.transaction((filePath: string) => {
    database.run("DELETE FROM messages WHERE path = ?", [filePath]);
    database.run("DELETE FROM files WHERE path = ?", [filePath]);
  });
  for (const row of knownRows) {
    if (!selectedSources.has(row.source) || livePaths.has(row.path)) continue;
    removeFile(row.path);
    removed++;
  }
  const totals = database.query("SELECT COUNT(*) AS files, COALESCE(SUM(message_count), 0) AS messages FROM files").get() as { files: number; messages: number };
  database.close();
  return { path, ...totals, indexed, removed, elapsedMs: Date.now() - startedAt };
}

function ftsLiteral(query: string): string {
  return `"${query.replaceAll('"', '""')}"`;
}

export function searchTranscriptIndex(
  query: string,
  sources: TranscriptSource[],
  path = defaultIndexPath(),
  limit = 200,
): IndexedFileMatch[] {
  if (query.length < 3) return [];
  const database = openIndex(path);
  const rows = matchingRows(database, query, sources);
  database.close();
  const lower = query.toLowerCase();
  const matches = new Map<string, IndexedFileMatch>();
  for (const row of rows) {
    let count = 0;
    let offset = 0;
    const text = row.text.toLowerCase();
    while ((offset = text.indexOf(lower, offset)) >= 0) {
      count++;
      offset += Math.max(1, lower.length);
    }
    if (count === 0) continue;
    const current = matches.get(row.path) ?? { source: row.source, path: row.path, count: 0 };
    current.count += count;
    matches.set(row.path, current);
  }
  return [...matches.values()].sort((a, b) => b.count - a.count || b.path.localeCompare(a.path)).slice(0, limit);
}

interface IndexedMessageRow {
  path: string;
  source: TranscriptSource;
  role: string;
  date: string;
  project: string;
  text: string;
}

function matchingRows(database: Database, query: string, sources: TranscriptSource[]): IndexedMessageRow[] {
  const placeholders = sources.map(() => "?").join(", ");
  return database.query(`
    SELECT path, source, role, date, project, text
    FROM messages
    WHERE messages MATCH ? AND source IN (${placeholders})
  `).all(ftsLiteral(query), ...sources) as IndexedMessageRow[];
}

function literalCount(text: string, query: string): number {
  const haystack = text.toLowerCase();
  const needle = query.toLowerCase();
  let count = 0;
  let offset = 0;
  while ((offset = haystack.indexOf(needle, offset)) >= 0) {
    count++;
    offset += Math.max(1, needle.length);
  }
  return count;
}

function indexedSnippet(text: string, query: string, radius = 100): string {
  const index = text.toLowerCase().indexOf(query.toLowerCase());
  if (index < 0) return text.slice(0, radius * 2);
  const start = Math.max(0, index - radius);
  const end = Math.min(text.length, index + query.length + radius);
  return `${start > 0 ? "..." : ""}${text.slice(start, end)}${end < text.length ? "..." : ""}`;
}

export function searchTranscriptIndexMatches(
  query: string,
  sources: TranscriptSource[],
  path = defaultIndexPath(),
  limit = 40,
  snippetLimit = 3,
): StoreSearchMatch[] {
  if (query.length < 3) return [];
  const database = openIndex(path);
  const rows = matchingRows(database, query, sources);
  database.close();
  const matches = new Map<string, StoreSearchMatch>();
  for (const row of rows) {
    const count = literalCount(row.text, query);
    if (count === 0) continue;
    const current = matches.get(row.path) ?? {
      source: row.source,
      path: row.path,
      count: 0,
      date: row.date || dateFromTranscriptPath(row.path),
      project: row.project || projectFromTranscriptPath(row.path, row.source),
      snippets: [],
    };
    current.count += count;
    if (current.snippets.length < snippetLimit) current.snippets.push({ role: row.role, text: indexedSnippet(row.text, query) });
    matches.set(row.path, current);
  }
  return [...matches.values()]
    .sort((a, b) => b.count - a.count || b.date.localeCompare(a.date))
    .slice(0, limit);
}

function dateFromTranscriptPath(path: string): string {
  return path.match(/(\d{4}-\d{2}-\d{2})T/)?.[1]
    ?? path.match(/sessions[/\\](\d{4})[/\\](\d{2})[/\\](\d{2})/)?.slice(1, 4).join("-")
    ?? "unknown";
}

function projectFromTranscriptPath(path: string, source: TranscriptSource): string {
  if (source === "claude") {
    const encoded = path.match(/\.claude[/\\]projects[/\\]([^/\\]+)/)?.[1]?.replace(/^-/, "");
    return encoded ? decodeProject(encoded) : "~";
  }
  if (source === "pi") {
    const encoded = path.match(/sessions[/\\](--.*?--)[/\\]/)?.[1];
    return encoded ? decodeProject(encoded.slice(2, -2)) : "~";
  }
  return "~";
}

function projectFromTranscript(path: string, source: TranscriptSource, text: string): string {
  if (source === "codex") {
    for (const line of text.split("\n")) {
      try {
        const entry = JSON.parse(line) as { type?: string; payload?: { cwd?: string } };
        if (entry.type === "session_meta" && entry.payload?.cwd) return compactHome(entry.payload.cwd);
      } catch { /* Invalid JSONL rows do not invalidate the transcript. */ }
    }
  }
  return projectFromTranscriptPath(path, source);
}

function decodeProject(encodedPath: string): string {
  const home = process.env.HOME ?? "";
  const homeEncoded = home.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-");
  let encoded = encodedPath;
  if (homeEncoded && encoded.startsWith(`${homeEncoded}-`)) encoded = encoded.slice(homeEncoded.length + 1);
  else if (encoded === homeEncoded) return "~";
  return encoded.replace(/-/g, "/") || "~";
}

function compactHome(path: string): string {
  const home = process.env.HOME ?? "";
  return home && path.startsWith(`${home}/`) ? path.slice(home.length + 1) : path;
}

export async function transcriptIndexStatus(path = defaultIndexPath()): Promise<TranscriptIndexStatus> {
  try {
    const fileStat = await stat(path);
    const database = new Database(path, { readonly: true });
    const totals = database.query("SELECT COUNT(*) AS files, COALESCE(SUM(message_count), 0) AS messages FROM files").get() as { files: number; messages: number };
    database.close();
    return { path, exists: true, ...totals, bytes: fileStat.size };
  } catch {
    return { path, exists: false, files: 0, messages: 0, bytes: 0 };
  }
}
