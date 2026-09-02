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
  const database = new Database(databasePath, { readonly: true, strict: true });
  try {
    const rows = database.query<SearchRow, [string]>(`
      SELECT s.id AS session_id, s.directory, s.title, s.time_updated,
             json_extract(m.data, '$.role') AS role,
             json_extract(p.data, '$.text') AS text
      FROM part p
      JOIN message m ON m.id = p.message_id
      JOIN session s ON s.id = p.session_id
      WHERE json_extract(p.data, '$.type') = 'text'
        AND instr(lower(json_extract(p.data, '$.text')), lower(?1)) > 0
      ORDER BY s.time_updated DESC
    `).all(query);
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
  const database = new Database(databasePath, { readonly: true, strict: true });
  try {
    const rows = database.query<MessageRow, [string]>(`
      SELECT m.id AS message_id, json_extract(m.data, '$.role') AS role, p.data AS part_data
      FROM message m
      JOIN part p ON p.message_id = m.id
      WHERE m.session_id = ?1
      ORDER BY m.time_created, m.id, p.time_created, p.id
    `).all(sessionId);
    const messages = new Map<string, RecallMessage>();
    for (const row of rows) {
      const data = safeJson(row.part_data);
      if (data.type !== "text" || typeof data.text !== "string") continue;
      const message = messages.get(row.message_id) ?? { role: row.role || "unknown", content: [] };
      message.content.push({ type: "text", text: data.text });
      messages.set(row.message_id, message);
    }
    return [...messages.values()].filter((message) => message.content.length > 0);
  } finally {
    database.close();
  }
}

function safeJson(value: string): Record<string, unknown> {
  try { return JSON.parse(value) as Record<string, unknown>; }
  catch { return {}; }
}
