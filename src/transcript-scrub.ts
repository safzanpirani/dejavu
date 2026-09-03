import { Database } from "bun:sqlite";
import { copyFileSync } from "node:fs";
import { parseOpenCodeLocator } from "./opencode-store.ts";
import { sourceFromLocator } from "./source-registry.ts";
import type { TranscriptSource } from "./transcript-types.ts";
import { loadTranscriptEvents, type EventRef, type TranscriptEvent } from "./transcript-view.ts";

export const DEFAULT_PLACEHOLDER = "[redacted]";

export interface ScrubOptions {
  /** Event numbers from `dejavu transcript` whose content is replaced by the placeholder. Dropping a tool call also drops its result. */
  drop?: number[];
  /** Case-insensitive literal fragments. Every line of every string field containing one is removed, in every record of the store. */
  patterns?: string[];
  placeholder?: string;
  /** Report what would change without writing. */
  dryRun?: boolean;
}

export interface ScrubResult {
  path: string;
  source: TranscriptSource;
  dryRun: boolean;
  backup: string | null;
  /** Event numbers that were redacted, including tool results pulled in by their call. */
  droppedEvents: number[];
  /** Lines removed from string fields by pattern matching. */
  patternLines: number;
  /** Records (JSONL lines or database rows) rewritten. */
  changedRecords: number;
}

export interface ScrubDeps {
  detectSource?: (locator: string) => TranscriptSource;
  loadEvents?: typeof loadTranscriptEvents;
  now?: () => number;
}

/** Fields that carry conversational text or tool payloads. Structural fields (type, id, role, names) are left alone. */
const TEXT_KEYS = new Set(["text", "thinking", "output", "stdout", "stderr", "error", "command", "lastPrompt", "title", "summary", "description"]);
const PAYLOAD_KEYS = new Set(["input", "arguments", "action", "toolUseResult", "content", "metadata"]);

export async function scrubTranscript(locator: string, options: ScrubOptions = {}, deps: ScrubDeps = {}): Promise<ScrubResult> {
  const drop = [...new Set(options.drop ?? [])].sort((a, b) => a - b);
  const patterns = (options.patterns ?? []).map((pattern) => pattern.trim()).filter(Boolean);
  if (drop.length === 0 && patterns.length === 0) throw new Error("scrub needs at least one --drop event number or --pattern");
  const placeholder = options.placeholder ?? DEFAULT_PLACEHOLDER;
  const source = (deps.detectSource ?? sourceFromLocator)(locator);
  const { events } = await (deps.loadEvents ?? loadTranscriptEvents)(locator, source);
  const targets = resolveDrops(events, drop);
  const context = { placeholder, patterns: patterns.map((pattern) => pattern.toLowerCase()), targets };
  if (source === "opencode") return scrubOpenCode(locator, source, context, options.dryRun ?? false, deps);
  return scrubJsonl(locator, source, context, options.dryRun ?? false, deps);
}

interface ScrubContext {
  placeholder: string;
  patterns: string[];
  targets: TranscriptEvent[];
}

function resolveDrops(events: TranscriptEvent[], drop: number[]): TranscriptEvent[] {
  const byIndex = new Map(events.map((event) => [event.index ?? -1, event]));
  const chosen = new Map<number, TranscriptEvent>();
  for (const index of drop) {
    const event = byIndex.get(index);
    if (!event) throw new Error(`no event #${index}; the transcript has events #0 to #${events.length - 1}`);
    if (!event.ref) throw new Error(`event #${index} has no source reference and cannot be scrubbed`);
    chosen.set(index, event);
    if (event.kind === "tool_call" && event.callId) {
      for (const candidate of events) {
        if (candidate.kind === "tool_result" && candidate.callId === event.callId && candidate.index !== undefined && candidate.ref) {
          chosen.set(candidate.index, candidate);
        }
      }
    }
  }
  return [...chosen.values()].sort((a, b) => (a.index ?? 0) - (b.index ?? 0));
}

// ---------------------------------------------------------------------------
// redaction primitives

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Replaces every text-bearing field beneath `node` with the placeholder while keeping ids, types, and names intact. */
export function redactNode(node: unknown, placeholder: string): unknown {
  if (typeof node === "string") return placeholder;
  if (Array.isArray(node)) return node.map((item) => redactNode(item, placeholder));
  if (!isRecord(node)) return node;
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(node)) {
    if (TEXT_KEYS.has(key)) out[key] = typeof value === "string" ? placeholder : redactNode(value, placeholder);
    else if (PAYLOAD_KEYS.has(key)) out[key] = typeof value === "string" ? placeholder : isRecord(value) || Array.isArray(value) ? redactNode(value, placeholder) : value;
    else if (isRecord(value) || Array.isArray(value)) out[key] = redactNode(value, placeholder);
    else out[key] = value;
  }
  return out;
}

/** Applies the event redaction to one parsed JSONL entry. Returns true when something changed. */
export function redactEntry(entry: Record<string, unknown>, event: TranscriptEvent, source: TranscriptSource, placeholder: string): boolean {
  const before = JSON.stringify(entry);
  const ref = event.ref as { line: number; block?: number } | undefined;
  if (source === "codex") {
    if (isRecord(entry.payload)) entry.payload = redactNode(entry.payload, placeholder);
  } else {
    const message = isRecord(entry.message) ? entry.message : undefined;
    if (message) {
      if (ref?.block !== undefined && Array.isArray(message.content)) {
        message.content = message.content.map((block, position) => position === ref.block ? redactNode(block, placeholder) : block);
      } else if (typeof message.content === "string") {
        message.content = placeholder;
      } else {
        message.content = redactNode(message.content, placeholder);
      }
    }
    if (source === "claude" && event.kind === "tool_result" && entry.toolUseResult !== undefined) {
      entry.toolUseResult = redactNode(entry.toolUseResult, placeholder);
    }
  }
  return JSON.stringify(entry) !== before;
}

/** Removes matching lines from every string in the tree, per the redaction handoff: walk the whole record, not only the message. */
export function scrubPatterns(node: unknown, patterns: string[], placeholder: string, counter: { lines: number }): unknown {
  if (typeof node === "string") {
    if (!patterns.some((pattern) => node.toLowerCase().includes(pattern))) return node;
    const kept = node.split("\n").filter((line) => {
      const lower = line.toLowerCase();
      const hit = patterns.some((pattern) => lower.includes(pattern));
      if (hit) counter.lines++;
      return !hit;
    });
    const joined = kept.join("\n");
    return joined.trim() ? joined : placeholder;
  }
  if (Array.isArray(node)) return node.map((item) => scrubPatterns(item, patterns, placeholder, counter));
  if (!isRecord(node)) return node;
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(node)) {
    let name = key;
    if (patterns.some((pattern) => key.toLowerCase().includes(pattern))) {
      counter.lines++;
      name = placeholder;
      for (let suffix = 2; name in out; suffix++) name = `${placeholder}-${suffix}`;
    }
    out[name] = scrubPatterns(value, patterns, placeholder, counter);
  }
  return out;
}

// ---------------------------------------------------------------------------
// JSONL stores (Claude, Codex, Pi)

async function scrubJsonl(locator: string, source: TranscriptSource, context: ScrubContext, dryRun: boolean, deps: ScrubDeps): Promise<ScrubResult> {
  const text = await Bun.file(locator).text();
  const lines = text.split("\n");
  const byLine = new Map<number, TranscriptEvent[]>();
  for (const event of context.targets) {
    const ref = event.ref as { line: number } | undefined;
    if (!ref || !("line" in ref)) continue;
    byLine.set(ref.line, [...(byLine.get(ref.line) ?? []), event]);
  }
  const counter = { lines: 0 };
  let changedRecords = 0;
  for (let index = 0; index < lines.length; index++) {
    const raw = lines[index]!;
    if (!raw.trim()) continue;
    let entry: unknown;
    try { entry = JSON.parse(raw); }
    catch { continue; }
    if (!isRecord(entry)) continue;
    const before = JSON.stringify(entry);
    for (const event of byLine.get(index + 1) ?? []) redactEntry(entry, event, source, context.placeholder);
    if (context.patterns.length) entry = scrubPatterns(entry, context.patterns, context.placeholder, counter);
    const after = JSON.stringify(entry);
    if (after !== before) {
      changedRecords++;
      lines[index] = after;
    }
  }
  const backup = dryRun ? null : `${locator}.bak-${Math.floor((deps.now ?? Date.now)() / 1000)}`;
  if (!dryRun && changedRecords > 0) {
    copyFileSync(locator, backup!);
    const output = lines.join("\n");
    for (const line of output.split("\n")) {
      if (line.trim()) JSON.parse(line);
    }
    await Bun.write(locator, output);
  }
  return {
    path: locator,
    source,
    dryRun,
    backup: changedRecords > 0 ? backup : null,
    droppedEvents: context.targets.map((event) => event.index!),
    patternLines: counter.lines,
    changedRecords,
  };
}

// ---------------------------------------------------------------------------
// OpenCode SQLite store

async function scrubOpenCode(locator: string, source: TranscriptSource, context: ScrubContext, dryRun: boolean, deps: ScrubDeps): Promise<ScrubResult> {
  const { databasePath, sessionId } = parseOpenCodeLocator(locator);
  const partTargets = new Map<string, TranscriptEvent[]>();
  for (const event of context.targets) {
    const ref = event.ref as { partId: string } | undefined;
    if (!ref || !("partId" in ref)) continue;
    partTargets.set(ref.partId, [...(partTargets.get(ref.partId) ?? []), event]);
  }
  const backup = dryRun ? null : `${databasePath}.bak-${Math.floor((deps.now ?? Date.now)() / 1000)}`;
  const database = new Database(databasePath, { readonly: dryRun, strict: true });
  const counter = { lines: 0 };
  let changedRecords = 0;
  try {
    const parts = database.query<{ id: string; data: string }, [string]>("SELECT id, data FROM part WHERE session_id = ?1").all(sessionId);
    const messages = database.query<{ id: string; data: string }, [string]>("SELECT id, data FROM message WHERE session_id = ?1").all(sessionId);
    const updates: Array<{ table: "part" | "message"; id: string; data: string }> = [];
    for (const row of parts) {
      let data: unknown;
      try { data = JSON.parse(row.data); }
      catch { continue; }
      if (!isRecord(data)) continue;
      const before = JSON.stringify(data);
      if (partTargets.has(row.id)) {
        const state = isRecord(data.state) ? data.state : undefined;
        if (state) {
          data.state = { ...state, ...(state.input !== undefined ? { input: redactNode(state.input, context.placeholder) } : {}), ...(state.output !== undefined ? { output: context.placeholder } : {}), ...(state.error !== undefined ? { error: context.placeholder } : {}), ...(state.metadata !== undefined ? { metadata: redactNode(state.metadata, context.placeholder) } : {}) };
        }
        if (typeof data.text === "string") data.text = context.placeholder;
        if (typeof data.title === "string") data.title = context.placeholder;
      }
      if (context.patterns.length) data = scrubPatterns(data, context.patterns, context.placeholder, counter);
      const after = JSON.stringify(data);
      if (after !== before) updates.push({ table: "part", id: row.id, data: after });
    }
    if (context.patterns.length) {
      for (const row of messages) {
        let data: unknown;
        try { data = JSON.parse(row.data); }
        catch { continue; }
        const before = JSON.stringify(data);
        const after = JSON.stringify(scrubPatterns(data, context.patterns, context.placeholder, counter));
        if (after !== before) updates.push({ table: "message", id: row.id, data: after });
      }
    }
    changedRecords = updates.length;
    if (!dryRun && updates.length > 0) {
      copyFileSync(databasePath, backup!);
      const part = database.query("UPDATE part SET data = ?1 WHERE id = ?2");
      const message = database.query("UPDATE message SET data = ?1 WHERE id = ?2");
      database.transaction(() => {
        for (const update of updates) (update.table === "part" ? part : message).run(update.data, update.id);
      })();
    }
  } finally {
    database.close();
  }
  return {
    path: locator,
    source,
    dryRun,
    backup: changedRecords > 0 ? backup : null,
    droppedEvents: context.targets.map((event) => event.index!),
    patternLines: counter.lines,
    changedRecords,
  };
}

export function parseDropList(values: string[]): number[] {
  const out: number[] = [];
  for (const value of values) {
    for (const part of value.split(",")) {
      const token = part.trim();
      if (!token) continue;
      const range = token.match(/^(\d+)-(\d+)$/);
      if (range) {
        const [start, end] = [Number(range[1]), Number(range[2])];
        if (end < start) throw new Error(`--drop range '${token}' runs backwards`);
        for (let index = start; index <= end; index++) out.push(index);
        continue;
      }
      if (!/^\d+$/.test(token)) throw new Error(`--drop expects event numbers like 4, 7-9, or 4,7-9 (got '${token}')`);
      out.push(Number(token));
    }
  }
  return out;
}

export type { EventRef };
