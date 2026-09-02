import { stat } from "node:fs/promises";
import { DEFAULT_MAX_PARALLEL, mapPool } from "./concurrency.ts";
import { completeQuery, resolveQueryModel } from "./model-client.ts";
import { searchOpenCodeStore } from "./opencode-store.ts";
import { searchFileCounts, searchMatchingLines } from "./search-backend.ts";
import { refreshTranscriptIndex, searchTranscriptIndexMatches } from "./transcript-index.ts";
import { discoverTranscriptStores, sourceFromLocator } from "./source-registry.ts";
import { extractVisibleMessage, loadRecallMessages, type RecallMessage } from "./session-reader.ts";
import type {
  SourceSelector,
  StoreDiagnostic,
  StoreSearchMatch,
  TranscriptSource,
  TranscriptStore,
} from "./transcript-types.ts";

export const DEFAULT_SEARCH_LIMIT = 10;
export const DEFAULT_SNIPPET_LIMIT = 3;

export interface SearchResult {
  query: string;
  sources: TranscriptSource[];
  matches: StoreSearchMatch[];
  skippedStores: StoreDiagnostic[];
  elapsedMs: number;
}

export interface SearchOptions {
  source?: SourceSelector;
  limit?: number;
  snippets?: number;
  maxParallel?: number;
  noIndex?: boolean;
}

export interface SearchDeps {
  discoverStores?: (selector: SourceSelector) => Promise<TranscriptStore[]>;
  countFiles?: typeof searchFileCounts;
  findLines?: typeof searchMatchingLines;
  searchOpenCode?: typeof searchOpenCodeStore;
  readProject?: typeof readTranscriptProject;
  now?: () => number;
}

export interface QueryOptions {
  agentDir: string;
  model?: string;
  signal?: AbortSignal;
}

export interface QueryResult {
  source: TranscriptSource;
  sessionPath: string;
  question: string;
  answer: string;
  model: { provider: string; id: string };
  messageCount: number;
  wasWindowed: boolean;
  elapsedMs: number;
}

export interface QueryDeps {
  pathExists?: (path: string) => Promise<boolean>;
  loadMessages?: typeof loadRecallMessages;
  resolveModel?: typeof resolveQueryModel;
  complete?: typeof completeQuery;
  detectSource?: typeof sourceFromLocator;
  now?: () => number;
}

async function defaultPathExists(path: string): Promise<boolean> {
  try { await stat(path); return true; }
  catch { return false; }
}

function positiveInteger(value: number, name: string): void {
  if (!Number.isInteger(value) || value < 1) throw new Error(`${name} must be an integer >= 1`);
}

export async function searchSessions(
  query: string,
  options: SearchOptions = {},
  deps: SearchDeps = {},
): Promise<SearchResult> {
  const needle = query.trim();
  if (!needle) throw new Error("search token must not be empty");
  const source = options.source ?? "all";
  const limit = options.limit ?? DEFAULT_SEARCH_LIMIT;
  const snippetLimit = options.snippets ?? DEFAULT_SNIPPET_LIMIT;
  const maxParallel = options.maxParallel ?? DEFAULT_MAX_PARALLEL;
  positiveInteger(limit, "limit");
  positiveInteger(snippetLimit, "snippets");
  positiveInteger(maxParallel, "maxParallel");
  const discoverStores = deps.discoverStores ?? discoverTranscriptStores;
  const stores = await discoverStores(source);
  if (stores.length === 0) throw new Error(`no transcript stores found for source: ${source}`);
  const now = deps.now ?? Date.now;
  const startedAt = now();
  const countFiles = deps.countFiles ?? searchFileCounts;
  const searchOpenCode = deps.searchOpenCode ?? searchOpenCodeStore;
  const indexedMatches = new Map<TranscriptSource, StoreSearchMatch[]>();
  const useIndex = !options.noIndex && needle.length >= 3 && !deps.discoverStores && !deps.countFiles;
  if (useIndex) {
    try {
      const jsonlStores = stores.filter((store) => store.kind === "jsonl");
      await refreshTranscriptIndex(jsonlStores);
      const sources = [...new Set(jsonlStores.map((store) => store.source))];
      for (const sourceName of sources) indexedMatches.set(sourceName, []);
      for (const item of searchTranscriptIndexMatches(needle, sources, undefined, limit * 4 * Math.max(1, sources.length), snippetLimit)) {
        const matches = indexedMatches.get(item.source) ?? [];
        matches.push(item);
        indexedMatches.set(item.source, matches);
      }
    } catch { /* The filesystem scanner remains the compatibility fallback. */ }
  }
  const storeScans = await mapPool(stores, maxParallel, async (store) => {
    if (store.kind === "sqlite") {
      try {
        return { store, direct: await searchOpenCode(needle, store.path, limit * 4, snippetLimit), counts: [], diagnostic: undefined };
      } catch (error) {
        return {
          store,
          direct: [] as StoreSearchMatch[],
          counts: [],
          diagnostic: { source: store.source, path: store.path, error: error instanceof Error ? error.message : String(error) },
        };
      }
    }
    const direct = indexedMatches.get(store.source);
    if (direct) return { store, direct, counts: [], diagnostic: undefined };
    const counts = (await countFiles(needle, store.path))
      .sort((a, b) => b.count - a.count || b.path.localeCompare(a.path))
      .slice(0, limit * 4);
    return { store, direct: [] as StoreSearchMatch[], counts, diagnostic: undefined };
  });
  const fileTasks = storeScans.flatMap((scan) => scan.counts.map((item) => ({ store: scan.store, item })));
  const findLines = deps.findLines ?? searchMatchingLines;
  const readProject = deps.readProject ?? readTranscriptProject;
  const fileMatches = await mapPool(fileTasks, maxParallel, async ({ store, item }): Promise<StoreSearchMatch | null> => {
    const lines = await findLines(needle, item.path, snippetLimit * 20);
    const visible = lines.map((line) => extractVisibleMessage(line, store.source))
      .filter((message): message is NonNullable<typeof message> => message !== null)
      .filter((message) => message.text.toLowerCase().includes(needle.toLowerCase()))
      .slice(0, snippetLimit);
    if (visible.length === 0) return null;
    const project = compactHome(visible.find((message) => message.project)?.project
      ?? await readProject(item.path, store.source));
    return {
      source: store.source,
      path: item.path,
      count: item.count,
      date: visible.find((message) => message.date)?.date ?? dateFromPath(item.path),
      project,
      snippets: visible.map((message) => ({ role: message.role, text: snippetAround(message.text, needle) })),
    };
  });
  const candidates: StoreSearchMatch[] = [];
  let fileCursor = 0;
  for (const scan of storeScans) {
    candidates.push(...scan.direct);
    for (const match of fileMatches.slice(fileCursor, fileCursor + scan.counts.length)) {
      if (match) candidates.push(match);
    }
    fileCursor += scan.counts.length;
  }
  candidates.sort((a, b) => b.count - a.count || b.date.localeCompare(a.date));
  return {
    query: needle,
    sources: [...new Set(storeScans.filter((scan) => !scan.diagnostic).map((scan) => scan.store.source))],
    matches: candidates.slice(0, limit),
    skippedStores: storeScans.flatMap((scan) => scan.diagnostic ? [scan.diagnostic] : []),
    elapsedMs: now() - startedAt,
  };
}

export async function readTranscriptProject(path: string, source: TranscriptSource): Promise<string> {
  if (source === "pi") return projectFromPiPath(path);
  if (source === "claude") return projectFromClaudePath(path);
  try {
    const prefix = await Bun.file(path).slice(0, 128 * 1024).text();
    for (const line of prefix.split("\n")) {
      const entry = JSON.parse(line) as { type?: string; payload?: { cwd?: string } };
      if (source === "codex" && entry.type === "session_meta" && entry.payload?.cwd) return compactHome(entry.payload.cwd);
    }
  } catch { /* Metadata is optional. */ }
  return "~";
}

export function snippetAround(text: string, query: string, radius = 100): string {
  const index = text.toLowerCase().indexOf(query.toLowerCase());
  if (index < 0) return text.slice(0, radius * 2);
  const start = Math.max(0, index - radius);
  const end = Math.min(text.length, index + query.length + radius);
  return `${start > 0 ? "..." : ""}${text.slice(start, end)}${end < text.length ? "..." : ""}`;
}

export function dateFromPath(path: string): string {
  return path.match(/(\d{4}-\d{2}-\d{2})T/)?.[1]
    ?? path.match(/sessions[/\\](\d{4})[/\\](\d{2})[/\\](\d{2})/)?.slice(1, 4).join("-")
    ?? "unknown";
}

export function projectFromPiPath(path: string, home = process.env.HOME ?? ""): string {
  const match = path.match(/sessions[/\\](--.*?--)[/\\]/);
  if (!match?.[1]) return "~";
  return decodeProject(match[1].slice(2, -2), home);
}

export function projectFromClaudePath(path: string, home = process.env.HOME ?? ""): string {
  const match = path.match(/\.claude[/\\]projects[/\\]([^/\\]+)[/\\]/);
  if (!match?.[1]) return "~";
  return decodeProject(match[1].replace(/^-/, ""), home);
}

function decodeProject(encodedPath: string, home: string): string {
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

export function prepareRecallMessages(messages: RecallMessage[]): RecallMessage[] {
  return messages.filter((message) => message.role === "user" || message.role === "assistant")
    .map((message) => ({ ...message, content: message.content.filter((block) => block.type !== "thinking") }))
    .filter((message) => message.content.length > 0);
}

interface SerializedMessage { role: string; text: string; charCount: number }
const STOP_WORDS = new Set([
  "a", "an", "the", "is", "was", "were", "are", "be", "been", "being", "have", "has", "had", "do", "does", "did",
  "will", "would", "could", "should", "may", "might", "can", "shall", "to", "of", "in", "for", "on", "with", "at", "by",
  "from", "as", "into", "about", "like", "through", "after", "over", "between", "out", "against", "during", "without", "before",
  "under", "around", "among", "and", "but", "or", "nor", "not", "so", "yet", "both", "either", "neither", "each", "every", "all",
  "any", "few", "more", "most", "other", "some", "such", "no", "only", "own", "same", "than", "too", "very", "just", "because",
  "if", "when", "where", "how", "what", "which", "who", "whom", "this", "that", "these", "those", "it", "its", "they", "them",
  "their", "we", "us", "our", "you", "your", "he", "him", "his", "she", "her", "i", "me", "my",
]);

export function extractKeywords(question: string): string[] {
  return question.toLowerCase().replace(/[^\w\s-]/g, " ").split(/\s+/)
    .filter((word) => word.length > 2 && !STOP_WORDS.has(word));
}

export function buildWindowedContext(messages: SerializedMessage[], question: string, tokenBudget: number): string {
  const bookends = 3;
  if (messages.length <= bookends * 2 + 2) return messages.map(formatSerialized).join("\n\n");
  const keywords = extractKeywords(question);
  const matches = messages.map((message, index) => ({
    index, score: keywords.filter((keyword) => message.text.toLowerCase().includes(keyword)).length,
  })).filter(({ score }) => score > 0).sort((a, b) => b.score - a.score).map(({ index }) => index);
  const included = new Set<number>();
  for (let i = 0; i < Math.min(bookends, messages.length); i++) included.add(i);
  for (let i = Math.max(0, messages.length - bookends); i < messages.length; i++) included.add(i);
  for (const index of matches) included.add(index);
  const charBudget = tokenBudget * 4;
  const currentChars = () => [...included].reduce((sum, index) => sum + messages[index]!.charCount + 20, 0);
  let radius = 1;
  while (currentChars() < charBudget * 0.8 && radius < messages.length) {
    const previousSize = included.size;
    for (const index of matches) for (let distance = -radius; distance <= radius; distance++) {
      const candidate = index + distance;
      if (candidate >= 0 && candidate < messages.length) included.add(candidate);
    }
    if (included.size === previousSize) break;
    radius++;
  }
  for (let i = bookends; i < messages.length && currentChars() < charBudget * 0.8; i++) included.add(i);
  const parts: string[] = [];
  let previous = -1;
  for (const index of [...included].sort((a, b) => a - b)) {
    if (previous >= 0 && index > previous + 1) {
      const gap = index - previous - 1;
      parts.push(`[... ${gap} message${gap === 1 ? "" : "s"} omitted ...]`);
    }
    parts.push(formatSerialized(messages[index]!));
    previous = index;
  }
  return parts.join("\n\n");
}

function formatSerialized(message: SerializedMessage): string { return `[${message.role}]\n${message.text}`; }

export async function querySession(
  sessionPath: string,
  question: string,
  options: QueryOptions,
  deps: QueryDeps = {},
): Promise<QueryResult> {
  if (!question.trim()) throw new Error("question must not be empty");
  const detectSource = deps.detectSource ?? sourceFromLocator;
  const source = detectSource(sessionPath);
  const pathExists = deps.pathExists ?? defaultPathExists;
  if (source !== "opencode" && !(await pathExists(sessionPath))) throw new Error(`transcript not found: ${sessionPath}`);
  const loadMessages = deps.loadMessages ?? loadRecallMessages;
  const resolveModel = deps.resolveModel ?? resolveQueryModel;
  const complete = deps.complete ?? completeQuery;
  const now = deps.now ?? Date.now;
  const startedAt = now();
  const messages = prepareRecallMessages(await loadMessages(sessionPath));
  if (messages.length === 0) throw new Error("transcript has no recallable messages");
  const resolved = await resolveModel(options.agentDir, options.model);
  const fullText = resolved.serialize(messages);
  const tokenBudget = Math.floor(resolved.contextWindow * 0.8);
  const wasWindowed = Math.ceil(fullText.length / 4) > tokenBudget;
  const conversation = wasWindowed ? buildWindowedContext(messages.map((message) => {
    const text = resolved.serialize([message]);
    return { role: message.role, text, charCount: text.length };
  }), question, tokenBudget) : fullText;
  const answer = await complete(resolved, conversation, question.trim(), options.signal);
  return {
    source,
    sessionPath,
    question: question.trim(),
    answer,
    model: { provider: resolved.provider, id: resolved.id },
    messageCount: messages.length,
    wasWindowed,
    elapsedMs: now() - startedAt,
  };
}
