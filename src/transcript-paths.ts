import type { TranscriptSource } from "./transcript-types.ts";

export function compactHome(path: string, home = process.env.HOME ?? ""): string {
  return home && path.startsWith(`${home}/`) ? path.slice(home.length + 1) : path;
}

export function snippetAround(text: string, query: string, radius = 100): string {
  const index = text.toLowerCase().indexOf(query.toLowerCase());
  if (index < 0) return text.slice(0, radius * 2);
  const start = Math.max(0, index - radius);
  const end = Math.min(text.length, index + query.length + radius);
  return `${start > 0 ? "..." : ""}${text.slice(start, end)}${end < text.length ? "..." : ""}`;
}

export function countOccurrences(text: string, query: string): number {
  const haystack = text.toLowerCase();
  const needle = query.toLowerCase();
  if (!needle) return 0;
  let count = 0;
  let index = 0;
  while ((index = haystack.indexOf(needle, index)) >= 0) {
    count++;
    index += needle.length;
  }
  return count;
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

export function projectFromTranscriptPath(path: string, source: TranscriptSource): string {
  if (source === "claude") return projectFromClaudePath(path);
  if (source === "pi") return projectFromPiPath(path);
  return "~";
}

/** Reads the Codex `session_meta` cwd from transcript text; other sources derive the project from the path. */
export function projectFromTranscriptText(path: string, source: TranscriptSource, text: string): string {
  if (source === "codex") {
    for (const line of text.split("\n")) {
      try {
        const entry = JSON.parse(line) as { type?: string; payload?: { cwd?: string } };
        if (entry.type === "session_meta" && entry.payload?.cwd) return compactHome(entry.payload.cwd);
      } catch { /* Invalid JSONL rows do not carry metadata. */ }
    }
  }
  return projectFromTranscriptPath(path, source);
}

function decodeProject(encodedPath: string, home: string): string {
  const homeEncoded = home.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-");
  let encoded = encodedPath;
  if (homeEncoded && encoded.startsWith(`${homeEncoded}-`)) encoded = encoded.slice(homeEncoded.length + 1);
  else if (encoded === homeEncoded) return "~";
  return encoded.replace(/-/g, "/") || "~";
}
