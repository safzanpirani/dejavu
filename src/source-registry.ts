import { homedir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { stat } from "node:fs/promises";
import type { SourceSelector, TranscriptSource, TranscriptStore } from "./transcript-types.ts";

const SOURCE_NAMES = new Set<SourceSelector>(["all", "claude", "codex", "pi", "opencode"]);

export function parseSource(value: string): SourceSelector {
  if (!SOURCE_NAMES.has(value as SourceSelector)) {
    throw new Error(`source must be one of: all, claude, codex, pi, opencode (got '${value}')`);
  }
  return value as SourceSelector;
}

export type StoreEnv = Record<string, string | undefined>;

export interface TranscriptStoreRoots {
  claude: string;
  codex: string;
  pi: string;
  opencode: string[];
}

/**
 * Resolves each agent's transcript store the way the agent itself does. A set variable replaces the
 * home-directory default; it never adds a second store:
 * Claude Code `$CLAUDE_CONFIG_DIR/projects`, Codex `$CODEX_HOME/sessions`,
 * Pi `$PI_CODING_AGENT_DIR/sessions`, OpenCode `$OPENCODE_DB` or `$XDG_DATA_HOME/opencode/*.db`.
 */
export function transcriptStoreRoots(env: StoreEnv = process.env, home = homedir()): TranscriptStoreRoots {
  const configured = (value: string | undefined, fallback: string) => value ? resolve(value) : fallback;
  // Pi expands a leading tilde itself; the other agents take the path as given.
  const piDir = env.PI_CODING_AGENT_DIR?.replace(/^~(?=$|[/\\])/, home);
  const opencodeData = join(configured(env.XDG_DATA_HOME, join(home, ".local", "share")), "opencode");
  const opencodeDb = env.OPENCODE_DB;
  return {
    claude: join(configured(env.CLAUDE_CONFIG_DIR, join(home, ".claude")), "projects"),
    codex: join(configured(env.CODEX_HOME, join(home, ".codex")), "sessions"),
    pi: join(configured(piDir, join(home, ".pi", "agent")), "sessions"),
    opencode: opencodeDb === ":memory:" ? []
      : opencodeDb ? [isAbsolute(opencodeDb) ? opencodeDb : join(opencodeData, opencodeDb)]
      : ["opencode.db", "opencode-next.db", "opencode-local.db"].map((name) => join(opencodeData, name)),
  };
}

export async function discoverTranscriptStores(
  selector: SourceSelector = "all",
  home = homedir(),
  env: StoreEnv = process.env,
): Promise<TranscriptStore[]> {
  const roots = transcriptStoreRoots(env, home);
  const candidates: TranscriptStore[] = [
    { source: "claude", kind: "jsonl", path: roots.claude },
    { source: "codex", kind: "jsonl", path: roots.codex },
    { source: "pi", kind: "jsonl", path: roots.pi },
    ...roots.opencode.map((path): TranscriptStore => ({ source: "opencode", kind: "sqlite", path })),
  ];
  const selected = candidates.filter((store) => selector === "all" || store.source === selector);
  const present = await Promise.all(selected.map(async (store) => {
    try { await stat(store.path); return store; }
    catch { return null; }
  }));
  return present.filter((store): store is TranscriptStore => store !== null);
}

export function sourceFromLocator(locator: string, roots: TranscriptStoreRoots = transcriptStoreRoots()): TranscriptSource {
  if (locator.startsWith("opencode://")) return "opencode";
  for (const source of ["claude", "codex", "pi"] as const) {
    if (locator.startsWith(`${roots[source]}/`)) return source;
  }
  if (locator.includes("/.claude/projects/")) return "claude";
  if (locator.includes("/.codex/sessions/")) return "codex";
  if (locator.includes("/.pi/agent/sessions/")) return "pi";
  throw new Error(`cannot determine transcript source from locator: ${locator} (use a transcript path or opencode:// locator from search results)`);
}
