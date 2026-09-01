import type { QueryResult, SearchResult } from "./core.ts";
import type { FindResult, ShowResult } from "./find.ts";

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
