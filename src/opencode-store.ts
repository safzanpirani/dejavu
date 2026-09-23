import { Database } from "bun:sqlite";
import { compactHome, countOccurrences, snippetAround } from "./transcript-paths.ts";
import type { RecallMessage, StoreSearchMatch, TranscriptSnippet } from "./transcript-types.ts";

interface SearchRow {
  session_id: string;
  directory: string;
  title: string;
  time_updated: number;
  role: string;
  text: string;
}

interface MessageRow {
  message_id: string;
  role: string;
  part_data: string;
}

// A read-only connection to a WAL database fails with SQLITE_CANTOPEN when no -shm file exists,
// because it cannot create one. Opening the file as immutable reads the committed database
// directly, which is correct for a store no OpenCode process is currently writing to.
export function openOpenCodeDatabase(databasePath: string): Database {
  let database: Database | null = null;
  try {
    database = new Database(databasePath, { readonly: true, strict: true });
    database.query("SELECT 1 FROM sqlite_master LIMIT 1").all();
    return database;
  } catch (error) {
    database?.close();
    if ((error as { code?: string }).code !== "SQLITE_CANTOPEN") throw error;
    return new Database(`file:${encodeURI(databasePath)}?immutable=1`, { readonly: true, strict: true });
  }
}

export interface OpenCodeSchema {
  legacy: boolean;
  v2: boolean;
}

// Legacy stores keep text in part rows under message and session. OpenCode v2 stores keep one JSON row per
// message in session_message under session_v2. A migrated store can hold both; a session with any
// session_message row is read from v2 only, so residual legacy parts do not double its matches.
export function openCodeSchema(database: Database): OpenCodeSchema {
  const tables = new Set(database.query<{ name: string }, []>("SELECT name FROM sqlite_master WHERE type = 'table'").all().map((row) => row.name));
  const schema = {
    legacy: ["session", "message", "part"].every((name) => tables.has(name)),
    v2: ["session_v2", "session_message"].every((name) => tables.has(name)),
  };
  if (!schema.legacy && !schema.v2) throw new Error("unsupported OpenCode schema: expected a part or session_message table");
  return schema;
}

// Joins a v2 message row to its visible text: user text sits in $.text, assistant text in the
// $.content items of type "text". Assistant rows yield one row per content item.
export const OPENCODE_V2_FROM = `session_message sm
  LEFT JOIN json_each(CASE WHEN sm.type = 'assistant' THEN json_extract(sm.data, '$.content') END) item`;
export const OPENCODE_V2_TEXT = `CASE WHEN sm.type = 'user' THEN json_extract(sm.data, '$.text')
  WHEN json_extract(item.value, '$.type') = 'text' THEN json_extract(item.value, '$.text') END`;
export const OPENCODE_V2_VISIBLE = "sm.type IN ('user', 'assistant')";

export function legacyOnlyClause(schema: OpenCodeSchema, sessionColumn: string): string {
  return schema.v2 ? `AND NOT EXISTS (SELECT 1 FROM session_message v2 WHERE v2.session_id = ${sessionColumn})` : "";
}

/** Whether a session is read from session_message; see openCodeSchema for how a hybrid store splits. */
export function openCodeSessionUsesV2(database: Database, schema: OpenCodeSchema, sessionId: string): boolean {
  return schema.v2 && database.query("SELECT 1 FROM session_message WHERE session_id = ?1 LIMIT 1").get(sessionId) !== null;
}

export function openCodeLocator(databasePath: string, sessionId: string): string {
  return `opencode://${encodeURI(databasePath)}#${encodeURIComponent(sessionId)}`;
}

export function parseOpenCodeLocator(locator: string): { databasePath: string; sessionId: string } {
  const url = new URL(locator);
  const databasePath = decodeURI(`${url.host}${url.pathname}`);
  const sessionId = decodeURIComponent(url.hash.slice(1));
  if (!databasePath.startsWith("/") || !sessionId) throw new Error(`invalid OpenCode locator: ${locator}`);
  return { databasePath, sessionId };
}

export async function searchOpenCodeStore(
  query: string,
  databasePath: string,
  limit: number,
  snippetsPerSession: number,
): Promise<StoreSearchMatch[]> {
  const database = openOpenCodeDatabase(databasePath);
  try {
    const schema = openCodeSchema(database);
    const rows: SearchRow[] = [];
    if (schema.v2) {
      rows.push(...database.query<SearchRow, [string]>(`
        SELECT * FROM (
          SELECT s.id AS session_id, s.directory, s.title, s.time_updated, sm.type AS role, ${OPENCODE_V2_TEXT} AS text
          FROM ${OPENCODE_V2_FROM}
          JOIN session_v2 s ON s.id = sm.session_id
          WHERE ${OPENCODE_V2_VISIBLE}
        ) WHERE instr(lower(text), lower(?1)) > 0
      `).all(query));
    }
    if (schema.legacy) {
      rows.push(...database.query<SearchRow, [string]>(`
        SELECT s.id AS session_id, s.directory, s.title, s.time_updated,
               json_extract(m.data, '$.role') AS role,
               json_extract(p.data, '$.text') AS text
        FROM part p
        JOIN message m ON m.id = p.message_id
        JOIN session s ON s.id = p.session_id
        WHERE json_extract(p.data, '$.type') = 'text'
          AND instr(lower(json_extract(p.data, '$.text')), lower(?1)) > 0
          ${legacyOnlyClause(schema, "p.session_id")}
      `).all(query));
    }
    rows.sort((a, b) => b.time_updated - a.time_updated);
    const grouped = new Map<string, StoreSearchMatch>();
    for (const row of rows) {
      const existing = grouped.get(row.session_id) ?? {
        source: "opencode" as const,
        path: openCodeLocator(databasePath, row.session_id),
        count: 0,
        date: new Date(row.time_updated).toISOString().slice(0, 10),
        project: compactHome(row.directory || row.title || "~"),
        snippets: [],
      };
      existing.count += countOccurrences(row.text, query);
      if (existing.snippets.length < snippetsPerSession) {
        existing.snippets.push({ role: row.role || "unknown", text: snippetAround(row.text, query) });
      }
      grouped.set(row.session_id, existing);
    }
    return [...grouped.values()]
      .sort((a, b) => b.count - a.count || b.date.localeCompare(a.date))
      .slice(0, limit);
  } finally {
    database.close();
  }
}

export async function loadOpenCodeMessages(locator: string): Promise<RecallMessage[]> {
  const { databasePath, sessionId } = parseOpenCodeLocator(locator);
  const database = openOpenCodeDatabase(databasePath);
  try {
    const schema = openCodeSchema(database);
    if (openCodeSessionUsesV2(database, schema, sessionId)) {
      const rows = database.query<{ message_id: string; role: string; text: string | null }, [string]>(`
        SELECT sm.id AS message_id, sm.type AS role, ${OPENCODE_V2_TEXT} AS text
        FROM ${OPENCODE_V2_FROM}
        WHERE sm.session_id = ?1 AND ${OPENCODE_V2_VISIBLE}
        ORDER BY sm.seq, item.key
      `).all(sessionId);
      return groupMessages(rows.filter((row): row is typeof row & { text: string } => typeof row.text === "string"));
    }
    if (!schema.legacy) return [];
    const rows = database.query<MessageRow, [string]>(`
      SELECT m.id AS message_id, json_extract(m.data, '$.role') AS role, p.data AS part_data
      FROM message m
      JOIN part p ON p.message_id = m.id
      WHERE m.session_id = ?1
      ORDER BY m.time_created, m.id, p.time_created, p.id
    `).all(sessionId);
    return groupMessages(rows.flatMap((row) => {
      const data = safeJson(row.part_data);
      return data.type === "text" && typeof data.text === "string" ? [{ message_id: row.message_id, role: row.role, text: data.text }] : [];
    }));
  } finally {
    database.close();
  }
}

function groupMessages(rows: Array<{ message_id: string; role: string; text: string }>): RecallMessage[] {
  const messages = new Map<string, RecallMessage>();
  for (const row of rows) {
    const message = messages.get(row.message_id) ?? { role: row.role || "unknown", content: [] };
    message.content.push({ type: "text", text: row.text });
    messages.set(row.message_id, message);
  }
  return [...messages.values()];
}

function safeJson(value: string): Record<string, unknown> {
  try { return JSON.parse(value) as Record<string, unknown>; }
  catch { return {}; }
}
