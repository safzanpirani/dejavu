import type { QueryResult, SearchResult } from "./core.ts";
import type { FindResult, ShowResult } from "./find.ts";
import type { TranscriptView } from "./transcript-view.ts";

export function renderSearch(result: SearchResult): string {
  if (result.matches.length === 0) {
    return `No sessions found matching "${result.query}".\n\nSearch is literal, not semantic. Retry with one exact distinctive token or phrase.`;
  }
  const sections = result.matches.map((match) => {
    const snippets = match.snippets.map((snippet) => `  [${snippet.role}] ${snippet.text}`).join("\n");
    return `${match.date} · ${match.source} · ${match.project} · ${match.count} match${match.count === 1 ? "" : "es"}\nTranscript: ${match.path}\n${snippets}`;
  });
  return `Found ${result.matches.length} session${result.matches.length === 1 ? "" : "s"} matching "${result.query}":\n\n${sections.join("\n\n---\n\n")}`;
}

export function renderQuery(result: QueryResult): string { return result.answer; }

export function renderFind(result: FindResult): string {
  if (result.hits.length === 0) {
    return `No sessions found for: ${result.terms.join(", ")}.\n\nTerms are literal (AND). Try fewer or different exact terms, or drop filters.`;
  }
  const relaxed = result.requiredTerms.length < result.terms.length
    ? `No session matched all ${result.terms.length} terms; showing sessions matching ${result.requiredTerms.length}.\n\n`
    : "";
  const sections = result.hits.map((hit) => {
    const counts = Object.entries(hit.termCounts)
      .map(([term, count]) => `${term}×${count.user + count.assistant}(u${count.user})`).join(" ");
    const lines = [
      `${hit.date} · ${hit.source} · ${hit.project} · ${counts}`,
      hit.openingPrompt ? `Opened with: ${hit.openingPrompt}` : undefined,
      ...hit.matches.slice(0, 3).map((match) => `  [${match.role}${match.date ? ` ${match.date}` : ""}] ${match.text}`),
      `Transcript: ${hit.path}`,
      hit.resume ? `Resume: ${hit.resume}` : undefined,
    ];
    return lines.filter((line): line is string => line !== undefined).join("\n");
  });
  return `${relaxed}Found ${result.hits.length} session${result.hits.length === 1 ? "" : "s"} for ${result.terms.join(" + ")}:\n\n${sections.join("\n\n---\n\n")}`;
}

export function renderShow(result: ShowResult): string {
  return result.messages.map((message) => `[${message.role}]\n${message.text}`).join("\n\n");
}

export interface RenderTranscriptOptions {
  /** Do not truncate messages, tool inputs, or tool outputs. */
  full?: boolean;
  /** Emit ANSI colors. */
  color?: boolean;
}

const TEXT_LIMIT = 1200;
const TOOL_INPUT_LIMIT = 300;
const TOOL_OUTPUT_LINES = 8;
const TOOL_OUTPUT_LIMIT = 600;

export function renderTranscript(view: TranscriptView, options: RenderTranscriptOptions = {}): string {
  const full = options.full ?? false;
  const paint = options.color ? ansi : plain;
  const header = paint.dim(`${view.source} · ${view.project} · ${view.counts.user} user · ${view.counts.assistant} assistant · ${view.counts.toolCalls} tool call${view.counts.toolCalls === 1 ? "" : "s"}`);
  const blocks = view.events.map((event, index) => {
    switch (event.kind) {
      case "user":
        return `${rule(paint.user("USER"), event, paint)}\n${clip(event.text, full ? Infinity : TEXT_LIMIT)}`;
      case "assistant":
        return `${rule(paint.assistant("ASSISTANT"), event, paint)}\n${clip(event.text, full ? Infinity : TEXT_LIMIT)}`;
      case "thinking":
        return `${rule(paint.dim("THINKING"), event, paint)}\n${paint.dim(clip(event.text, full ? Infinity : TEXT_LIMIT))}`;
      case "tool_call": {
        const previous = view.events[index - 1];
        const lead = previous && previous.kind !== "user" ? "" : `${rule(paint.assistant("ASSISTANT"), event, paint)}\n`;
        return `${lead}${tag(event, paint)}${paint.tool(`▶ ${event.name}`)} ${indent(formatToolInput(event.input, full), "    ")}`;
      }
      case "tool_result": {
        const label = `${tag(event, paint)}${event.isError ? paint.error(`◀ ${event.name ?? "tool"} error`) : paint.dim(`◀ ${event.name ?? "tool"} result`)}`;
        const body = full ? event.output : clipLines(event.output, TOOL_OUTPUT_LINES, TOOL_OUTPUT_LIMIT);
        return body.trim() ? `${label}\n${indentAll(paint.dim(body), "    ")}` : `${label} ${paint.dim("(empty)")}`;
      }
    }
  });
  return `${header}\n\n${blocks.join("\n\n")}`;
}

function rule(label: string, event: { index?: number; timestamp?: string }, paint: Paint): string {
  const when = event.timestamp ? ` ${paint.dim(event.timestamp.replace("T", " ").slice(0, 19))}` : "";
  return `${paint.dim("───")} ${tag(event, paint)}${label}${when} ${paint.dim("─".repeat(40))}`;
}

function tag(event: { index?: number }, paint: Paint): string {
  return event.index === undefined ? "" : `${paint.dim(`#${event.index}`)} `;
}

function formatToolInput(input: unknown, full: boolean): string {
  if (typeof input === "string") return full ? input : clip(input, TOOL_INPUT_LIMIT);
  const record = typeof input === "object" && input !== null ? input as Record<string, unknown> : undefined;
  const command = record && typeof (record.command ?? record.cmd) === "string" ? String(record.command ?? record.cmd) : undefined;
  const keys = record ? Object.keys(record) : [];
  if (command && keys.length === 1) return full ? command : clip(command, TOOL_INPUT_LIMIT);
  const text = full ? JSON.stringify(input, null, 2) : JSON.stringify(input);
  return full ? text : clip(text, TOOL_INPUT_LIMIT);
}

function clip(text: string, limit: number): string {
  const trimmed = text.trim();
  return trimmed.length <= limit ? trimmed : `${trimmed.slice(0, limit)} [...]`;
}

function clipLines(text: string, maxLines: number, maxChars: number): string {
  const lines = text.trim().split("\n");
  const kept = lines.slice(0, maxLines).join("\n");
  const clipped = kept.length > maxChars ? kept.slice(0, maxChars) : kept;
  const truncated = lines.length > maxLines || kept.length > maxChars;
  return truncated ? `${clipped}\n[... ${lines.length} lines, ${text.length} chars]` : clipped;
}

function indentAll(text: string, prefix: string): string {
  return text.split("\n").map((line) => `${prefix}${line}`).join("\n");
}

function indent(text: string, prefix: string): string {
  return text.split("\n").map((line, index) => (index === 0 ? line : `${prefix}${line}`)).join("\n");
}

interface Paint {
  user: (text: string) => string;
  assistant: (text: string) => string;
  tool: (text: string) => string;
  error: (text: string) => string;
  dim: (text: string) => string;
}

const ansi: Paint = {
  user: (text) => `\x1b[1;36m${text}\x1b[0m`,
  assistant: (text) => `\x1b[1;32m${text}\x1b[0m`,
  tool: (text) => `\x1b[33m${text}\x1b[0m`,
  error: (text) => `\x1b[31m${text}\x1b[0m`,
  dim: (text) => `\x1b[90m${text}\x1b[0m`,
};

const plain: Paint = {
  user: (text) => text,
  assistant: (text) => text,
  tool: (text) => text,
  error: (text) => text,
  dim: (text) => text,
};
