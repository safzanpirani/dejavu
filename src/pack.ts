import { basename } from "node:path";
import { DEFAULT_MAX_PARALLEL, mapPool } from "./concurrency.ts";
import { findSessions, type FindOptions } from "./find.ts";
import { renderTranscript } from "./render.ts";
import { viewTranscript } from "./transcript-view.ts";
import { eventBody, renderWindow, validateBound, windowTranscript, type WindowedTranscript } from "./transcript-window.ts";

export interface PackOptions extends FindOptions {
  budgetChars?: number;
  maxChars?: number;
  context?: number;
  excludeSessions?: string[];
}

export interface PackResult {
  terms: string[];
  requiredTerms: string[];
  budgetChars: number;
  usedChars: number;
  candidateCount: number;
  excludedCount: number;
  sessions: WindowedTranscript[];
  skippedStores: Awaited<ReturnType<typeof findSessions>>["skippedStores"];
  skippedSessions: { path: string; error: string }[];
}

export async function packSessions(
  terms: string[], options: PackOptions = {},
  deps: { find?: typeof findSessions; view?: typeof viewTranscript } = {},
): Promise<PackResult> {
  const limit = options.limit ?? 3;
  const budgetChars = options.budgetChars ?? 12000;
  const maxChars = options.maxChars ?? 1200;
  const context = options.context ?? 2;
  validateBound(limit, "--limit");
  validateBound(budgetChars, "--budget-chars");
  validateBound(maxChars, "--max-chars");
  validateBound(context, "--context", 0);
  const found = await (deps.find ?? findSessions)(terms, { ...options, limit: 40 });
  const exclusions = [...(options.excludeSessions ?? []), process.env.CODEX_THREAD_ID, process.env.CLAUDE_SESSION_ID].filter((value): value is string => Boolean(value));
  const candidates = found.hits.filter((hit) => !exclusions.some((value) =>
    hit.path === value || basename(hit.path, ".jsonl") === value ||
    (hit.source === "opencode" && hit.path.endsWith(`#${encodeURIComponent(value)}`)) ||
    (/^[a-f\d]{8}(?:-[a-f\d]{4}){3}-[a-f\d]{12}$/i.test(value) && (hit.path.endsWith(`${value}.jsonl`) || hit.path.endsWith(`/${value}`))),
  ));
  const loaded = await mapPool(candidates, options.maxParallel ?? DEFAULT_MAX_PARALLEL, async (hit) => {
    try {
      const view = await (deps.view ?? viewTranscript)(hit.path, { tools: false });
      const keep = new Set<number>();
      const needles = found.requiredTerms.map((term) => term.toLowerCase());
      view.events.forEach((event, index) => {
        if (!needles.some((term) => eventBody(event).toLowerCase().includes(term))) return;
        for (let neighbor = Math.max(0, index - context); neighbor <= Math.min(view.events.length - 1, index + context); neighbor++) keep.add(neighbor);
      });
      return { view: { ...view, events: view.events.filter((_, index) => keep.has(index)) }, path: hit.path };
    } catch (error) {
      return { path: hit.path, error: error instanceof Error ? error.message : String(error) };
    }
  });
  const selected = loaded.flatMap((item) => item.view?.events.length ? [item.view] : []).slice(0, limit);
  const sessions: WindowedTranscript[] = [];
  let usedChars = 0;
  for (const [index, view] of selected.entries()) {
    if (usedChars >= budgetChars) break;
    const share = Math.max(1, Math.floor((budgetChars - usedChars) / (selected.length - index)));
    // Reserve space for each selected neighbor so a long preceding turn cannot consume the match's budget.
    const perEvent = Math.min(maxChars, Math.max(1, Math.floor(share / view.events.length)));
    const session = windowTranscript(view, { budgetChars: share, maxChars: perEvent, focusTerms: found.requiredTerms });
    sessions.push(session);
    usedChars += session.window.usedChars;
  }
  return {
    terms: found.terms, requiredTerms: found.requiredTerms, budgetChars, usedChars,
    candidateCount: found.hits.length, excludedCount: found.hits.length - candidates.length,
    sessions, skippedStores: found.skippedStores,
    skippedSessions: loaded.flatMap((item) => item.error !== undefined ? [{ path: item.path, error: item.error }] : []),
  };
}

export function renderPack(result: PackResult): string {
  const header = `${result.sessions.length} sessions · ${result.usedChars}/${result.budgetChars} body chars · ${result.candidateCount} ranked candidates examined (search cap 40)`;
  const relaxed = result.requiredTerms.length < result.terms.length ? `\nMatched subset: ${result.requiredTerms.join(" + ")}` : "";
  return `${header}${relaxed}\n\n${result.sessions.map((session) => `Transcript: ${session.path}\n${renderTranscript(session, { full: true })}\n${renderWindow(session.window)}`).join("\n\n---\n\n")}`;
}
