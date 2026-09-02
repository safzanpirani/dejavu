import { basename } from "node:path";
import { DEFAULT_MAX_PARALLEL, mapPool } from "./concurrency.ts";
import { dateFromPath, projectFromClaudePath, projectFromPiPath, readTranscriptProject } from "./core.ts";
import { searchOpenCodeStore } from "./opencode-store.ts";
import { searchFileCounts, searchMatchingLines } from "./search-backend.ts";
import { refreshTranscriptIndex, searchTranscriptIndex } from "./transcript-index.ts";
import { discoverTranscriptStores, sourceFromLocator } from "./source-registry.ts";
import { extractVisibleMessage, loadRecallMessages } from "./session-reader.ts";
import { prepareRecallMessages } from "./core.ts";
import type { SourceSelector, StoreDiagnostic, StoreSearchMatch, TranscriptSource, TranscriptStore } from "./transcript-types.ts";

export const DEFAULT_FIND_LIMIT = 5;
const CANDIDATE_CAP = 40;
const USER_WEIGHT = 5;
const NOISE_PATH = /[/\\](subagents|tool-results|subagent-artifacts)[/\\]/;

export interface FindOptions {
  source?: SourceSelector;
  limit?: number;
  project?: string;
  since?: string;
  userOnly?: boolean;
  maxParallel?: number;
  noIndex?: boolean;
}

export interface FindHit {
  source: TranscriptSource;
  path: string;
  project: string;
  date: string;
  score: number;
  termCounts: Record<string, { user: number; assistant: number }>;
  openingPrompt: string;
  matches: { role: string; date?: string; text: string }[];
  resume?: string;
}

export interface FindResult {
  terms: string[];
  requiredTerms: string[];
  sources: TranscriptSource[];
  hits: FindHit[];
  skippedStores: StoreDiagnostic[];
  elapsedMs: number;
  storeTimings: Record<string, number>;
}

export interface FindDeps {
  discoverStores?: (selector: SourceSelector) => Promise<TranscriptStore[]>;
  countFiles?: typeof searchFileCounts;
  findLines?: typeof searchMatchingLines;
  searchOpenCode?: typeof searchOpenCodeStore;
  readProject?: typeof readTranscriptProject;
  readPrefix?: (path: string, bytes: number) => Promise<string>;
  loadMessages?: typeof loadRecallMessages;
  now?: () => number;
}

export interface ShowDeps {
  detectSource?: typeof sourceFromLocator;
  loadMessages?: typeof loadRecallMessages;
}

const defaultReadPrefix = (path: string, bytes: number) => Bun.file(path).slice(0, bytes).text();

export function parseSince(value: string, today = new Date()): string {
  const absolute = value.match(/^\d{4}-\d{2}-\d{2}$/);
  if (absolute) return value;
  const relative = value.match(/^(\d+)([dwm])$/);
  if (!relative) throw new Error(`--since needs YYYY-MM-DD or <N>d/<N>w/<N>m (got '${value}')`);
  const amount = Number(relative[1]);
  const days = relative[2] === "d" ? amount : relative[2] === "w" ? amount * 7 : amount * 30;
  const date = new Date(today.getTime() - days * 86_400_000);
  return date.toISOString().slice(0, 10);
}

export function resumeCommand(source: TranscriptSource, path: string, project: string): string | undefined {
  const name = basename(path);
  if (source === "claude") {
    const id = name.replace(/\.jsonl$/, "");
    if (!/^[0-9a-f-]{36}$/.test(id)) return undefined;
    return `claude --resume ${id}`;
  }
  if (source === "codex") {
    const id = name.match(/rollout-.*-([0-9a-f-]{36})\.jsonl$/)?.[1];
    return id ? `codex resume ${id}` : undefined;
  }
  if (source === "pi") return `pi --session ${path}`;
  return undefined;
}

function isRealUserPrompt(text: string): boolean {
  const trimmed = text.trim();
  if (!trimmed || trimmed.startsWith("<") || trimmed.startsWith("Caveat:")) return false;
  if (trimmed.startsWith("[Request interrupted")) return false;
  if (trimmed.startsWith("Base directory for this skill")) return false;
  if (trimmed.startsWith("This session is being continued from a previous conversation")) return false;
  return true;
}

export async function findOpeningPrompt(
  path: string,
  source: TranscriptSource,
  deps: Pick<FindDeps, "readPrefix" | "loadMessages"> = {},
): Promise<string> {
  const readPrefix = deps.readPrefix ?? defaultReadPrefix;
  const loadMessages = deps.loadMessages ?? loadRecallMessages;
  try {
    if (source === "opencode") {
      const messages = prepareRecallMessages(await loadMessages(path, "opencode"));
      const first = messages.find((message) => message.role === "user" && message.content.some(
        (block) => block.type === "text" && typeof block.text === "string" && isRealUserPrompt(block.text),
      ));
      const text = first?.content.filter((block) => block.type === "text").map((block) => block.text).join(" ") ?? "";
      return text.trim();
    }
    const prefix = await readPrefix(path, 256 * 1024);
    for (const line of prefix.split("\n")) {
      const message = extractVisibleMessage(line, source);
      if (message && message.role === "user" && isRealUserPrompt(message.text)) return message.text.trim();
    }
  } catch { /* Opening prompt is best-effort. */ }
  return "";
}

interface Candidate {
  source: TranscriptSource;
  path: string;
  rawCounts: Map<string, number>;
}

export async function findSessions(
  terms: string[],
  options: FindOptions = {},
  deps: FindDeps = {},
): Promise<FindResult> {
  const cleaned: string[] = [];
  const normalizedTerms = new Set<string>();
  for (const rawTerm of terms) {
    const term = rawTerm.trim();
    const normalized = term.toLowerCase();
    if (!term || normalizedTerms.has(normalized)) continue;
    cleaned.push(term);
    normalizedTerms.add(normalized);
  }
  if (cleaned.length === 0) throw new Error("find needs at least one term");
  const limit = options.limit ?? DEFAULT_FIND_LIMIT;
  if (!Number.isInteger(limit) || limit < 1) throw new Error(`limit must be an integer >= 1 (got '${limit}')`);
  const maxParallel = options.maxParallel ?? DEFAULT_MAX_PARALLEL;
  if (!Number.isInteger(maxParallel) || maxParallel < 1) throw new Error(`maxParallel must be an integer >= 1 (got '${maxParallel}')`);
  const discoverStores = deps.discoverStores ?? discoverTranscriptStores;
  const now = deps.now ?? Date.now;
  const startedAt = now();
  const stores = await discoverStores(options.source ?? "all");
  if (stores.length === 0) throw new Error(`no transcript stores found for source: ${options.source ?? "all"}`);
  const countFiles = deps.countFiles ?? searchFileCounts;
  const searchOpenCode = deps.searchOpenCode ?? searchOpenCodeStore;
  const indexedCounts = new Map<string, Map<TranscriptSource, Array<{ path: string; count: number }>>>();
  const useIndex = !options.noIndex && cleaned.every((term) => term.length >= 3) && !deps.discoverStores && !deps.countFiles;
  if (useIndex) {
    try {
      const jsonlStores = stores.filter((store) => store.kind === "jsonl");
      await refreshTranscriptIndex(jsonlStores);
      const sources = [...new Set(jsonlStores.map((store) => store.source))];
      for (const term of cleaned) {
        const bySource = new Map<TranscriptSource, Array<{ path: string; count: number }>>();
        for (const sourceName of sources) bySource.set(sourceName, []);
        for (const item of searchTranscriptIndex(term, sources, undefined, 800)) {
          const matches = bySource.get(item.source) ?? [];
          matches.push({ path: item.path, count: item.count });
          bySource.set(item.source, matches);
        }
        indexedCounts.set(term, bySource);
      }
    } catch { /* The filesystem scanner remains the compatibility fallback. */ }
  }

  // Per term x store, collect raw match counts with a bounded pool.
  // OpenCode search results (with snippets) are kept to avoid re-querying per candidate.
  const scanBatches = await mapPool(cleaned.flatMap((term) => stores.map((store) => ({ term, store }))), maxParallel, async ({ term, store }) => {
    const scanStart = now();
    let rows: { source: TranscriptSource; path: string; count: number }[];
    let matches: StoreSearchMatch[] = [];
    if (store.kind === "sqlite") {
      try {
        matches = await searchOpenCode(term, store.path, 200, 4);
      } catch (error) {
        return {
          term,
          store,
          source: store.source,
          elapsedMs: now() - scanStart,
          rows: [],
          matches,
          diagnostic: { source: store.source, path: store.path, error: error instanceof Error ? error.message : String(error) },
        };
      }
      rows = matches.map((match) => ({ source: store.source, path: match.path, count: match.count }));
    } else {
      const counts = indexedCounts.get(term)?.get(store.source) ?? await countFiles(term, store.path);
      rows = counts
        .filter((item) => item.path.endsWith(".jsonl") && !NOISE_PATH.test(item.path))
        .map((item) => ({ source: store.source, path: item.path, count: item.count }));
    }
    return { term, store, source: store.source, elapsedMs: now() - scanStart, rows: rows.map((row) => ({ term, ...row })), matches, diagnostic: undefined };
  });
  const storeElapsed = new Map<string, number>();
  const openCodeMatches = new Map<string, Map<string, StoreSearchMatch>>();
  const skippedByPath = new Map<string, StoreDiagnostic>();
  for (const batch of scanBatches) {
    storeElapsed.set(batch.source, Math.max(storeElapsed.get(batch.source) ?? 0, batch.elapsedMs));
    if (batch.diagnostic) skippedByPath.set(batch.store.path, batch.diagnostic);
    for (const match of batch.matches) {
      const perTerm = openCodeMatches.get(match.path) ?? new Map<string, StoreSearchMatch>();
      perTerm.set(batch.term, match);
      openCodeMatches.set(match.path, perTerm);
    }
  }
  const scans = scanBatches.flatMap((batch) => batch.rows);
  const byPath = new Map<string, Candidate>();
  for (const { term, source, path, count } of scans) {
    const candidate = byPath.get(path) ?? { source, path, rawCounts: new Map<string, number>() };
    candidate.rawCounts.set(term, count);
    byPath.set(path, candidate);
  }

  let candidates = [...byPath.values()];
  // Path-derived project filter must run before the candidate cap, or matching
  // sessions can be capped away before the post-cap project check ever sees them.
  if (options.project) {
    const needle = options.project.toLowerCase();
    candidates = candidates.filter((candidate) => {
      if (candidate.source === "claude") return projectFromClaudePath(candidate.path).toLowerCase().includes(needle);
      if (candidate.source === "pi") return projectFromPiPath(candidate.path).toLowerCase().includes(needle);
      return true;
    });
  }
  // Path-dated candidates outside the --since window skip the deep scan entirely;
  // the rest are re-checked against message timestamps after scoring. Runs before
  // the cap for the same reason as the project filter.
  if (options.since) {
    const cutoff = parseSince(options.since);
    candidates = candidates.filter((candidate) => {
      const date = dateFromPath(candidate.path);
      return date === "unknown" || date >= cutoff;
    });
  }
  // Rank balanced multi-term relevance above one-term spam: a session mentioning
  // every term dozens of times beats one mentioning a single term hundreds of times.
  const rawMin = (candidate: Candidate) => Math.min(...candidate.rawCounts.values());
  const rawTotal = (candidate: Candidate) => [...candidate.rawCounts.values()].reduce((sum, count) => sum + count, 0);
  candidates.sort((a, b) => b.rawCounts.size - a.rawCounts.size || rawMin(b) - rawMin(a) || rawTotal(b) - rawTotal(a));
  candidates = candidates.slice(0, CANDIDATE_CAP);

  const findLines = deps.findLines ?? searchMatchingLines;
  const readProject = deps.readProject ?? readTranscriptProject;
  const readPrefix = deps.readPrefix ?? defaultReadPrefix;
  const loadMessages = deps.loadMessages ?? loadRecallMessages;
  const sinceDate = options.since ? parseSince(options.since) : undefined;

  const hitCandidates = await mapPool(candidates, maxParallel, async (candidate): Promise<FindHit | null> => {
    const termCounts: FindHit["termCounts"] = {};
    const matches: FindHit["matches"] = [];
    let score = 0;
    let latestDate = "";
    if (candidate.source === "opencode") {
      for (const term of candidate.rawCounts.keys()) {
        const stored = openCodeMatches.get(candidate.path)?.get(term);
        const user = stored?.snippets.filter((snippet) => snippet.role === "user").length ?? 0;
        const assistant = (stored?.snippets.length ?? 0) - user;
        termCounts[term] = { user, assistant };
        score += user * USER_WEIGHT + assistant;
        for (const snippet of stored?.snippets ?? []) matches.push({ role: snippet.role, text: snippet.text });
        if (stored?.date && stored.date > latestDate) latestDate = stored.date;
      }
    } else {
      for (const term of candidate.rawCounts.keys()) {
        const lines = await findLines(term, candidate.path, 400);
        let user = 0;
        let assistant = 0;
        const lowered = term.toLowerCase();
        for (const line of lines) {
          const message = extractVisibleMessage(line, candidate.source);
          if (!message || !message.text.toLowerCase().includes(lowered)) continue;
          if (message.role === "user") user++;
          else assistant++;
          if (message.date && message.date > latestDate) latestDate = message.date;
          if (message.role === "user" && matches.length < 6 && isRealUserPrompt(message.text)) {
            const text = message.text.slice(0, 240);
            if (!matches.some((existing) => existing.text.slice(0, 80) === text.slice(0, 80))) {
              matches.push({ role: message.role, date: message.date, text });
            }
          }
        }
        termCounts[term] = { user, assistant };
        score += user * USER_WEIGHT + assistant;
      }
    }
    if (score === 0) return null;
    const date = latestDate || dateFromPath(candidate.path);
    if (sinceDate && date !== "unknown" && date < sinceDate) return null;
    const project = candidate.source === "opencode" ? "opencode" : await readProject(candidate.path, candidate.source);
    if (options.project && !project.toLowerCase().includes(options.project.toLowerCase())) return null;
    const openingPrompt = await findOpeningPrompt(candidate.path, candidate.source, { readPrefix, loadMessages });
    return {
      source: candidate.source,
      path: candidate.path,
      project,
      date,
      score,
      termCounts,
      openingPrompt: openingPrompt.slice(0, 300),
      matches,
      resume: resumeCommand(candidate.source, candidate.path, project),
    };
  });
  const hits = hitCandidates.filter((hit): hit is FindHit => hit !== null);
  const matchedTerms = (hit: FindHit) => cleaned.filter((term) => {
    const count = hit.termCounts[term];
    return options.userOnly ? (count?.user ?? 0) > 0 : (count?.user ?? 0) + (count?.assistant ?? 0) > 0;
  });
  hits.sort((a, b) => matchedTerms(b).length - matchedTerms(a).length || b.score - a.score || b.date.localeCompare(a.date));
  const requiredTerms = hits[0] ? matchedTerms(hits[0]) : [];
  const strictHits = hits.filter((hit) => requiredTerms.every((term) => {
    const count = hit.termCounts[term];
    return options.userOnly ? (count?.user ?? 0) > 0 : (count?.user ?? 0) + (count?.assistant ?? 0) > 0;
  }));
  return {
    terms: cleaned,
    requiredTerms,
    sources: [...new Set(scanBatches.filter((batch) => !batch.diagnostic).map((batch) => batch.source))],
    hits: strictHits.slice(0, limit),
    skippedStores: stores.flatMap((store) => skippedByPath.has(store.path) ? [skippedByPath.get(store.path)!] : []),
    elapsedMs: now() - startedAt,
    storeTimings: Object.fromEntries(storeElapsed),
  };
}

export interface ShowResult {
  path: string;
  source: TranscriptSource;
  messageCount: number;
  messages: { role: string; text: string }[];
}

export interface ShowOptions { full?: boolean; around?: string }

export async function showSession(locator: string, options: ShowOptions = {}, deps: ShowDeps = {}): Promise<ShowResult> {
  const full = options.full ?? false;
  const source = (deps.detectSource ?? sourceFromLocator)(locator);
  let messages = prepareRecallMessages(await (deps.loadMessages ?? loadRecallMessages)(locator));
  if (messages.length === 0) throw new Error("transcript has no recallable messages");
  let omitted: Set<number> | undefined;
  if (options.around) {
    const needle = options.around.toLowerCase();
    const matchIndexes = messages.flatMap((message, index) => message.content.some(
      (block) => block.type === "text" && typeof block.text === "string" && block.text.toLowerCase().includes(needle),
    ) ? [index] : []);
    if (matchIndexes.length === 0) throw new Error(`no message contains '${options.around}'`);
    const keep = new Set<number>();
    for (const index of matchIndexes) {
      for (let offset = -3; offset <= 3; offset++) {
        const neighbor = index + offset;
        if (neighbor >= 0 && neighbor < messages.length) keep.add(neighbor);
      }
    }
    const ordered = [...keep].sort((a, b) => a - b);
    omitted = new Set();
    const windowed: typeof messages = [];
    let previous = -1;
    for (const index of ordered) {
      if (index > previous + 1) omitted.add(windowed.length);
      windowed.push(messages[index]!);
      previous = index;
    }
    if (previous < messages.length - 1) omitted.add(windowed.length);
    messages = windowed;
  }
  const rendered: { role: string; text: string }[] = [];
  messages.forEach((message, index) => {
    if (omitted?.has(index)) rendered.push({ role: "…", text: "[messages omitted]" });
    const text = message.content.flatMap((block) => {
      if (block.type === "text" && typeof block.text === "string") return [block.text];
      if (block.type === "toolCall") return [`[tool: ${block.name ?? "unknown"}]`];
      if (block.type === "image") return ["[image]"];
      return [];
    }).join("\n").trim();
    if (text.length === 0) return;
    rendered.push({ role: message.role, text: full || text.length <= 700 ? text : `${text.slice(0, 700)} [...]` });
  });
  if (omitted?.has(messages.length)) rendered.push({ role: "…", text: "[messages omitted]" });
  return { path: locator, source, messageCount: rendered.length, messages: rendered };
}
