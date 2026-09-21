/**
 * Public types for the cross-harness project memory service.
 *
 * The legacy Claude reader in `memory.ts` keeps its own types; nothing here
 * changes it. See `docs/cross-harness-project-memory-implementation-handout.md`.
 */

export type Harness = "claude" | "codex" | "pi" | "opencode" | "manual" | "unknown";

export type MemoryKind = "decision" | "convention" | "procedure" | "pitfall" | "handoff";

export type Verification = "unverified" | "user_confirmed" | "code_verified";

export type MemoryStatus = "active" | "archived" | "superseded";

export type Evidence =
  | { kind: "transcript"; source: Harness; locator: string; eventIds: number[]; observedAt: string; excerptHash?: string }
  | { kind: "file"; path: string; commit?: string; line?: number; observedAt: string; contentHash?: string }
  | { kind: "user"; note: string; observedAt: string };

export interface Actor {
  harness: Harness;
  sessionId?: string;
}

export interface MemoryInput {
  kind: MemoryKind;
  title: string;
  body: string;
  pathPrefix?: string;
  branch?: string;
  tags?: string[];
  pinned?: boolean;
  verification: Verification;
  evidence: Evidence[];
  reviewAfter?: string;
  expiresAt?: string;
}

export interface MemoryRecord {
  id: string;
  projectId: string;
  revision: number;
  kind: MemoryKind;
  title: string;
  body: string;
  status: MemoryStatus;
  verification: Verification;
  pathPrefix?: string;
  branch?: string;
  pinned: boolean;
  tags: string[];
  evidence: Evidence[];
  createdAt: string;
  updatedAt: string;
  lastVerifiedAt?: string;
  reviewAfter?: string;
  expiresAt?: string;
  supersededBy?: string;
  contentHash: string;
}

export interface ProjectRecord {
  id: string;
  name: string;
  createdAt: string;
  updatedAt: string;
}

export interface ProjectBinding {
  kind: "git_common_dir" | "folder";
  canonicalPath: string;
  projectId: string;
  createdAt: string;
}

/** Resolved location + identity for one command invocation. */
export interface ProjectContext {
  projectId: string;
  cwd: string;
  /** Canonical boundary root: the Git checkout root or the explicit folder. */
  root: string;
  /** Repository-relative POSIX path of `cwd` beneath `root`, when inside it. */
  relativePath?: string;
  branch?: string;
}

export interface ResolvedProject {
  project: ProjectRecord;
  context: ProjectContext;
  /** Which binding matched, for `resolve` diagnostics. */
  matched: ProjectBinding;
  worktrees: string[];
}

export interface EnvelopeProject {
  id: string;
  name: string;
}

export interface EnvelopeError {
  code: string;
  message: string;
}

export interface Envelope<T> {
  version: 1;
  ok: boolean;
  project?: EnvelopeProject;
  data?: T;
  error?: EnvelopeError;
  diagnostics: string[];
}

export class ProjectMemoryError extends Error {
  readonly code: string;
  readonly exitCode: number;
  constructor(code: string, message: string, exitCode = 1) {
    super(message);
    this.name = "ProjectMemoryError";
    this.code = code;
    this.exitCode = exitCode;
  }
}

export const DEFAULT_RECALL_BUDGET = 6_000;
export const MAX_RECALL_BUDGET = 24_000;
export const DEFAULT_RECALL_RECORDS = 12;
export const DEFAULT_LIST_LIMIT = 20;
export const MAX_LIST_LIMIT = 200;
export const BODY_LIMIT = 16_000;
export const TITLE_LIMIT = 160;
export const TAG_LIMIT = 48;
export const MAX_TAGS = 20;
export const MAX_EVIDENCE = 20;
export const HANDOFF_DEFAULT_DAYS = 7;
