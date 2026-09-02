import { Database } from "bun:sqlite";
import { parseOpenCodeLocator } from "./opencode-store.ts";
import { loadBranchEntries, type TreeEntry } from "./session-reader.ts";
import { sourceFromLocator } from "./source-registry.ts";
import { compactHome, projectFromTranscriptPath } from "./transcript-paths.ts";
import type { TranscriptSource } from "./transcript-types.ts";

export type TranscriptEvent =
  | { kind: "user"; text: string; timestamp?: string }
  | { kind: "assistant"; text: string; timestamp?: string }
  | { kind: "thinking"; text: string; timestamp?: string }
  | { kind: "tool_call"; name: string; input: unknown; callId?: string; timestamp?: string }
  | { kind: "tool_result"; name?: string; callId?: string; output: string; isError: boolean; timestamp?: string };

export interface TranscriptCounts {
  user: number;
  assistant: number;
  thinking: number;
  toolCalls: number;
  toolResults: number;
}

export interface TranscriptView {
  path: string;
  source: TranscriptSource;
  project: string;
  counts: TranscriptCounts;
  events: TranscriptEvent[];
}

export interface TranscriptViewOptions {
  /** Include model thinking blocks (default false). */
  thinking?: boolean;
  /** Include tool calls and tool results (default true). */
  tools?: boolean;
}

export interface TranscriptViewDeps {
  detectSource?: (locator: string) => TranscriptSource;
  loadEvents?: (locator: string, source: TranscriptSource) => Promise<{ project: string; events: TranscriptEvent[] }>;
}

export async function viewTranscript(
  locator: string,
  options: TranscriptViewOptions = {},
  deps: TranscriptViewDeps = {},
): Promise<TranscriptView> {
  const source = (deps.detectSource ?? sourceFromLocator)(locator);
  const { project, events: all } = await (deps.loadEvents ?? loadTranscriptEvents)(locator, source);
  const includeThinking = options.thinking ?? false;
  const includeTools = options.tools ?? true;
  const events = all.filter((event) => {
    if (event.kind === "thinking") return includeThinking;
    if (event.kind === "tool_call" || event.kind === "tool_result") return includeTools;
    return true;
  });
  if (events.length === 0) throw new Error("transcript has no viewable turns");
  return { path: locator, source, project, counts: countEvents(all), events };
}

export function countEvents(events: TranscriptEvent[]): TranscriptCounts {
  const counts: TranscriptCounts = { user: 0, assistant: 0, thinking: 0, toolCalls: 0, toolResults: 0 };
  for (const event of events) {
    if (event.kind === "user") counts.user++;
    else if (event.kind === "assistant") counts.assistant++;
    else if (event.kind === "thinking") counts.thinking++;
    else if (event.kind === "tool_call") counts.toolCalls++;
    else counts.toolResults++;
  }
  return counts;
}

export async function loadTranscriptEvents(
  locator: string,
  source: TranscriptSource = sourceFromLocator(locator),
): Promise<{ project: string; events: TranscriptEvent[] }> {
  if (source === "opencode") return loadOpenCodeEvents(locator);
  const entries = await loadBranchEntries(locator, source);
  if (source === "claude") return { project: claudeProject(locator, entries), events: nameResults(entries.flatMap(claudeEvents)) };
  if (source === "pi") return { project: projectFromTranscriptPath(locator, "pi"), events: nameResults(entries.flatMap(piEvents)) };
  return { project: codexProject(entries), events: nameResults(entries.flatMap(codexEvents)) };
}

/** Fills in a tool result's name from the matching earlier tool call when the store records only a call id. */
function nameResults(events: TranscriptEvent[]): TranscriptEvent[] {
  const names = new Map<string, string>();
  for (const event of events) {
    if (event.kind === "tool_call" && event.callId) names.set(event.callId, event.name);
    else if (event.kind === "tool_result" && !event.name && event.callId) event.name = names.get(event.callId);
  }
  return events;
}

// ---------------------------------------------------------------------------
// Claude Code: ~/.claude/projects/<project>/<session>.jsonl

function claudeProject(locator: string, entries: TreeEntry[]): string {
  const cwd = entries.find((entry) => entry.cwd)?.cwd;
  return cwd ? compactHome(cwd) : projectFromTranscriptPath(locator, "claude");
}

function claudeEvents(entry: TreeEntry): TranscriptEvent[] {
  const role = entry.message?.role;
  if (role !== "user" && role !== "assistant") return [];
  const timestamp = entry.timestamp;
  const content = entry.message?.content;
  if (typeof content === "string") return content.trim() ? [{ kind: role, text: content, timestamp }] : [];
  if (!Array.isArray(content)) return [];
  return content.flatMap((raw): TranscriptEvent[] => {
    const block = asRecord(raw);
    if (!block) return [];
    switch (block.type) {
      case "text":
        return typeof block.text === "string" && block.text.trim() ? [{ kind: role, text: block.text, timestamp }] : [];
      case "thinking":
        return typeof block.thinking === "string" && block.thinking.trim() ? [{ kind: "thinking", text: block.thinking, timestamp }] : [];
      case "tool_use":
        return [{ kind: "tool_call", name: stringOr(block.name, "unknown"), input: block.input ?? {}, callId: stringOr(block.id), timestamp }];
      case "tool_result":
        return [{
          kind: "tool_result",
          callId: stringOr(block.tool_use_id),
          output: textOf(block.content),
          isError: block.is_error === true,
          timestamp,
        }];
      case "image":
        return [{ kind: role, text: "[image]", timestamp }];
      default:
        return [];
    }
  });
}

// ---------------------------------------------------------------------------
// Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl

interface CodexPayload {
  type?: string;
  role?: string;
  content?: unknown;
  summary?: unknown;
  name?: string;
  arguments?: unknown;
  input?: unknown;
  call_id?: string;
  output?: unknown;
  action?: { command?: unknown };
  cwd?: string;
  status?: string;
}

function codexProject(entries: TreeEntry[]): string {
  for (const entry of entries) {
    const payload = entry.payload as CodexPayload | undefined;
    if (entry.type === "session_meta" && payload?.cwd) return compactHome(payload.cwd);
  }
  return "~";
}

function codexEvents(entry: TreeEntry): TranscriptEvent[] {
  if (entry.type !== "response_item") return [];
  const payload = entry.payload as CodexPayload | undefined;
  if (!payload) return [];
  const timestamp = entry.timestamp;
  switch (payload.type) {
    case "message": {
      if (payload.role !== "user" && payload.role !== "assistant") return [];
      const text = textOf(payload.content);
      return text.trim() ? [{ kind: payload.role, text, timestamp }] : [];
    }
    case "reasoning": {
      const text = [textOf(payload.summary), textOf(payload.content)].filter(Boolean).join("\n");
      return text.trim() ? [{ kind: "thinking", text, timestamp }] : [];
    }
    case "function_call":
      return [{ kind: "tool_call", name: stringOr(payload.name, "unknown"), input: parseJsonArguments(payload.arguments), callId: payload.call_id, timestamp }];
    case "custom_tool_call":
      return [{ kind: "tool_call", name: stringOr(payload.name, "unknown"), input: payload.input ?? "", callId: payload.call_id, timestamp }];
    case "local_shell_call":
      return [{ kind: "tool_call", name: "shell", input: payload.action?.command ?? payload.action ?? {}, callId: payload.call_id, timestamp }];
    case "function_call_output":
    case "custom_tool_call_output": {
      const output = textOf(payload.output);
      return [{ kind: "tool_result", callId: payload.call_id, output, isError: looksLikeCodexError(output), timestamp }];
    }
    default:
      return [];
  }
}

function parseJsonArguments(value: unknown): unknown {
  if (typeof value !== "string") return value ?? {};
  try { return JSON.parse(value) as unknown; }
  catch { return value; }
}

function looksLikeCodexError(output: string): boolean {
  return /^(?:Script failed|Error:|Process exited with code [1-9]|Exit code: [1-9])/i.test(output.trimStart());
}

// ---------------------------------------------------------------------------
// Pi: ~/.pi/agent/sessions/<project>/<session>.jsonl

function piEvents(entry: TreeEntry): TranscriptEvent[] {
  if (entry.type !== "message") return [];
  const message = entry.message as (TreeEntry["message"] & { toolCallId?: string; toolName?: string; isError?: boolean }) | undefined;
  if (!message?.role) return [];
  const timestamp = entry.timestamp;
  if (message.role === "toolResult") {
    return [{
      kind: "tool_result",
      name: message.toolName,
      callId: message.toolCallId,
      output: textOf(message.content),
      isError: message.isError === true,
      timestamp,
    }];
  }
  const role = message.role;
  if (role !== "user" && role !== "assistant") return [];
  const content = message.content;
  if (typeof content === "string") return content.trim() ? [{ kind: role, text: content, timestamp }] : [];
  if (!Array.isArray(content)) return [];
  return content.flatMap((raw): TranscriptEvent[] => {
    const block = asRecord(raw);
    if (!block) return [];
    switch (block.type) {
      case "text":
        return typeof block.text === "string" && block.text.trim() ? [{ kind: role, text: block.text, timestamp }] : [];
      case "thinking":
        return typeof block.thinking === "string" && block.thinking.trim() ? [{ kind: "thinking", text: block.thinking, timestamp }] : [];
      case "toolCall":
        return [{ kind: "tool_call", name: stringOr(block.name, "unknown"), input: block.arguments ?? {}, callId: stringOr(block.id), timestamp }];
      case "image":
        return [{ kind: role, text: "[image]", timestamp }];
      default:
        return [];
    }
  });
}

// ---------------------------------------------------------------------------
// OpenCode: sqlite message + part rows

interface OpenCodeRow {
  message_id: string;
  message_data: string;
  part_data: string;
}

async function loadOpenCodeEvents(locator: string): Promise<{ project: string; events: TranscriptEvent[] }> {
  const { databasePath, sessionId } = parseOpenCodeLocator(locator);
  const database = new Database(databasePath, { readonly: true, strict: true });
  try {
    const directory = database.query<{ directory: string | null }, [string]>(
      "SELECT directory FROM session WHERE id = ?1",
    ).get(sessionId)?.directory;
    const rows = database.query<OpenCodeRow, [string]>(`
      SELECT m.id AS message_id, m.data AS message_data, p.data AS part_data
      FROM message m
      JOIN part p ON p.message_id = m.id
      WHERE m.session_id = ?1
      ORDER BY m.time_created, m.id, p.time_created, p.id
    `).all(sessionId);
    const events: TranscriptEvent[] = [];
    for (const row of rows) {
      const message = safeJson(row.message_data);
      const part = safeJson(row.part_data);
      const role = message.role === "assistant" ? "assistant" : "user";
      const created = asRecord(message.time)?.created;
      const timestamp = typeof created === "number" ? new Date(created).toISOString() : undefined;
      switch (part.type) {
        case "text":
          if (typeof part.text === "string" && part.text.trim()) events.push({ kind: role, text: part.text, timestamp });
          break;
        case "reasoning":
          if (typeof part.text === "string" && part.text.trim()) events.push({ kind: "thinking", text: part.text, timestamp });
          break;
        case "file":
          events.push({ kind: role, text: `[file: ${stringOr(part.filename, stringOr(part.mime, "attachment"))}]`, timestamp });
          break;
        case "tool": {
          const state = asRecord(part.state) ?? {};
          const name = stringOr(part.tool, "unknown");
          const callId = stringOr(part.callID);
          events.push({ kind: "tool_call", name, input: state.input ?? {}, callId, timestamp });
          const isError = state.status === "error";
          const output = isError ? textOf(state.error) : textOf(state.output);
          if (state.status === "completed" || isError) events.push({ kind: "tool_result", name, callId, output, isError, timestamp });
          break;
        }
        default:
          break;
      }
    }
    return { project: compactHome(directory || "~"), events };
  } finally {
    database.close();
  }
}

// ---------------------------------------------------------------------------
// helpers

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function stringOr(value: unknown): string | undefined;
function stringOr(value: unknown, fallback: string): string;
function stringOr(value: unknown, fallback?: string): string | undefined {
  return typeof value === "string" ? value : fallback;
}

/** Flattens the text-bearing shapes every store uses for content: a string, or a list of text-like blocks. */
function textOf(value: unknown): string {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return value === undefined || value === null ? "" : JSON.stringify(value);
  return value.flatMap((raw) => {
    if (typeof raw === "string") return [raw];
    const block = asRecord(raw);
    if (!block) return [];
    if (typeof block.text === "string") return [block.text];
    if (typeof block.thinking === "string") return [block.thinking];
    if (block.type === "image" || block.type === "input_image") return ["[image]"];
    return [];
  }).join("\n");
}

function safeJson(value: string): Record<string, unknown> {
  try { return asRecord(JSON.parse(value)) ?? {}; }
  catch { return {}; }
}
