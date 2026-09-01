export type TranscriptSource = "claude" | "codex" | "pi" | "opencode";
export type SourceSelector = TranscriptSource | "all";

export interface TranscriptStore {
  source: TranscriptSource;
  kind: "jsonl" | "sqlite";
  path: string;
}

export interface StoreDiagnostic {
  source: TranscriptSource;
  path: string;
  error: string;
}

export interface RecallBlock {
  type: string;
  text?: string;
  name?: string;
  arguments?: unknown;
}

export interface RecallMessage {
  role: string;
  content: RecallBlock[];
}

export interface TranscriptSnippet {
  role: string;
  text: string;
}

export interface StoreSearchMatch {
  source: TranscriptSource;
  path: string;
  count: number;
  date: string;
  project: string;
  snippets: TranscriptSnippet[];
}
