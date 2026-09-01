import { loadOpenCodeMessages } from "./opencode-store.ts";
import { sourceFromLocator } from "./source-registry.ts";
import type { RecallBlock, RecallMessage, TranscriptSource } from "./transcript-types.ts";
export type { RecallBlock, RecallMessage } from "./transcript-types.ts";

interface TreeEntry {
  type?: string;
  id?: string;
  parentId?: string | null;
  uuid?: string;
  parentUuid?: string | null;
  leafUuid?: string;
  message?: { role?: string; content?: unknown };
  payload?: { type?: string; role?: string; content?: unknown };
}

export async function loadRecallMessages(locator: string, forcedSource?: TranscriptSource): Promise<RecallMessage[]> {
  const source = forcedSource ?? sourceFromLocator(locator);
  if (source === "opencode") return loadOpenCodeMessages(locator);
  const entries = parseJsonl(await Bun.file(locator).text());
  if (source === "pi") return loadPiBranch(entries);
  if (source === "claude") return loadClaudeBranch(entries);
  return loadCodexMessages(entries);
}

function parseJsonl(text: string): TreeEntry[] {
  return text.split("\n").filter((line) => line.trim()).flatMap((line) => {
    try { return [JSON.parse(line) as TreeEntry]; }
    catch { return []; }
  });
}

function loadPiBranch(entries: TreeEntry[]): RecallMessage[] {
  const treeEntries = entries.filter((entry) => entry.type !== "session");
  const byId = new Map(treeEntries.filter((entry) => entry.id).map((entry) => [entry.id!, entry]));
  const branch: TreeEntry[] = [];
  let current = treeEntries.at(-1);
  while (current) {
    branch.push(current);
    current = current.parentId ? byId.get(current.parentId) : undefined;
  }
  return branch.reverse().flatMap((entry) => normalizeEnvelope(entry.message));
}

function loadClaudeBranch(entries: TreeEntry[]): RecallMessage[] {
  const byId = new Map(entries.filter((entry) => entry.uuid).map((entry) => [entry.uuid!, entry]));
  const recordedLeaf = entries.findLast((entry) => entry.type === "last-prompt")?.leafUuid;
  let current = recordedLeaf ? byId.get(recordedLeaf) : entries.findLast((entry) => entry.uuid);
  const branch: TreeEntry[] = [];
  while (current) {
    branch.push(current);
    current = current.parentUuid ? byId.get(current.parentUuid) : undefined;
  }
  return branch.reverse().flatMap((entry) => normalizeEnvelope(entry.message));
}

function loadCodexMessages(entries: TreeEntry[]): RecallMessage[] {
  return entries.flatMap((entry) => {
    if (entry.type !== "response_item" || entry.payload?.type !== "message") return [];
    if (entry.payload.role !== "user" && entry.payload.role !== "assistant") return [];
    return normalizeEnvelope(entry.payload);
  });
}

function normalizeEnvelope(envelope: { role?: string; content?: unknown } | undefined): RecallMessage[] {
  if (!envelope?.role || (envelope.role !== "user" && envelope.role !== "assistant")) return [];
  const content = normalizeContent(envelope.content);
  return content.length > 0 ? [{ role: envelope.role, content }] : [];
}

function normalizeContent(content: unknown): RecallBlock[] {
  if (typeof content === "string") return [{ type: "text", text: content }];
  if (!Array.isArray(content)) return [];
  return content.flatMap((raw): RecallBlock[] => {
    if (typeof raw !== "object" || raw === null) return [];
    const block = raw as Record<string, unknown>;
    if (["text", "input_text", "output_text"].includes(String(block.type)) && typeof block.text === "string") {
      return [{ type: "text", text: block.text }];
    }
    if (["toolCall", "tool_use"].includes(String(block.type))) {
      return [{
        type: "toolCall",
        name: typeof block.name === "string" ? block.name : "unknown",
        arguments: block.arguments ?? block.input ?? {},
      }];
    }
    if (["image", "input_image"].includes(String(block.type))) return [{ type: "image" }];
    return [];
  });
}

export function serializeRecallMessages(messages: RecallMessage[]): string {
  return messages.map((message) => {
    const blocks = message.content.flatMap((block) => {
      if (block.type === "text" && typeof block.text === "string") return [block.text];
      if (block.type === "toolCall") return [`[tool call: ${block.name ?? "unknown"}]\n${JSON.stringify(block.arguments ?? {})}`];
      if (block.type === "image") return ["[image omitted]"];
      return [];
    });
    return `[${message.role}]\n${blocks.join("\n")}`;
  }).join("\n\n");
}

export function extractVisibleMessage(line: string, source: TranscriptSource): { role: string; text: string; date?: string; project?: string } | null {
  let entry: TreeEntry & { timestamp?: string; cwd?: string };
  try { entry = JSON.parse(line) as typeof entry; }
  catch { return null; }
  const envelope = source === "codex" ? entry.payload : entry.message;
  if (source === "codex" && (entry.type !== "response_item" || entry.payload?.type !== "message")) return null;
  const normalized = normalizeEnvelope(envelope);
  const message = normalized[0];
  if (!message) return null;
  const text = message.content.filter((block) => block.type === "text").map((block) => block.text).join(" ");
  if (!text) return null;
  return {
    role: message.role,
    text,
    date: entry.timestamp?.slice(0, 10),
    project: entry.cwd,
  };
}
