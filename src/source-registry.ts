import { homedir } from "node:os";
import { join } from "node:path";
import { stat } from "node:fs/promises";
import type { SourceSelector, TranscriptSource, TranscriptStore } from "./transcript-types.ts";

const SOURCE_NAMES = new Set<SourceSelector>(["all", "claude", "codex", "pi", "opencode"]);

export function parseSource(value: string): SourceSelector {
  if (!SOURCE_NAMES.has(value as SourceSelector)) {
    throw new Error(`source must be one of: all, claude, codex, pi, opencode (got '${value}')`);
  }
  return value as SourceSelector;
}

export async function discoverTranscriptStores(
  selector: SourceSelector = "all",
  home = homedir(),
): Promise<TranscriptStore[]> {
  const candidates: TranscriptStore[] = [
    { source: "claude", kind: "jsonl", path: join(home, ".claude", "projects") },
    { source: "codex", kind: "jsonl", path: join(home, ".codex", "sessions") },
    { source: "pi", kind: "jsonl", path: join(home, ".pi", "agent", "sessions") },
    { source: "opencode", kind: "sqlite", path: join(home, ".local", "share", "opencode", "opencode.db") },
    { source: "opencode", kind: "sqlite", path: join(home, ".local", "share", "opencode", "opencode-next.db") },
    { source: "opencode", kind: "sqlite", path: join(home, ".local", "share", "opencode", "opencode-local.db") },
  ];
  const selected = candidates.filter((store) => selector === "all" || store.source === selector);
  const present = await Promise.all(selected.map(async (store) => {
    try { await stat(store.path); return store; }
    catch { return null; }
  }));
  return present.filter((store): store is TranscriptStore => store !== null);
}

export function sourceFromLocator(locator: string): TranscriptSource {
  if (locator.startsWith("opencode://")) return "opencode";
  if (locator.includes("/.claude/projects/")) return "claude";
  if (locator.includes("/.codex/sessions/")) return "codex";
  if (locator.includes("/.pi/agent/sessions/")) return "pi";
  throw new Error(`cannot determine transcript source from locator: ${locator} (use a transcript path or opencode:// locator from search results)`);
}
