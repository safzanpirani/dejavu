/**
 * Project memory service: validation, lifecycle rules, and orchestration.
 *
 * Domain logic never reads global cwd or environment repeatedly — the CLI
 * resolves a concrete `ProjectContext` once and passes it in, so a request
 * cannot change project halfway through execution.
 */

import { createHash } from "node:crypto";
import { basename } from "node:path";
import { ProjectMemoryStore, type WriteRequestRef } from "./project-memory-store.ts";
import {
  canonicalize,
  normalizePathPrefix,
  probeGit,
  relativeToRoot,
  selectBoundary,
  type Boundary,
  type IdentityDeps,
} from "./project-identity.ts";
import { recall, renderRecall, type RecallOutcome } from "./project-memory-recall.ts";
import {
  BODY_LIMIT,
  DEFAULT_RECALL_BUDGET,
  DEFAULT_RECALL_RECORDS,
  HANDOFF_DEFAULT_DAYS,
  MAX_EVIDENCE,
  MAX_RECALL_BUDGET,
  MAX_TAGS,
  ProjectMemoryError,
  TAG_LIMIT,
  TITLE_LIMIT,
} from "./project-memory-types.ts";
import type {
  Actor,
  Evidence,
  Harness,
  MemoryInput,
  MemoryKind,
  MemoryRecord,
  MemoryStatus,
  ProjectBinding,
  ProjectContext,
  ProjectRecord,
  ResolvedProject,
  Verification,
} from "./project-memory-types.ts";

const HARNESSES: Harness[] = ["claude", "codex", "pi", "opencode", "manual", "unknown"];
const KINDS: MemoryKind[] = ["decision", "convention", "procedure", "pitfall", "handoff"];
const VERIFICATIONS: Verification[] = ["unverified", "user_confirmed", "code_verified"];

const INPUT_FIELDS = new Set([
  "kind", "title", "body", "pathPrefix", "branch", "tags", "pinned",
  "verification", "evidence", "reviewAfter", "expiresAt",
]);

function invalid(message: string): never {
  throw new ProjectMemoryError("INVALID_ARGUMENT", message, 2);
}

function isoTimestamp(value: unknown, field: string): string {
  if (typeof value !== "string" || !value.trim()) invalid(`${field} must be an ISO timestamp string`);
  const parsed = Date.parse(value);
  if (Number.isNaN(parsed)) invalid(`${field} is not a valid ISO timestamp: '${value}'`);
  return new Date(parsed).toISOString();
}

function optionalTimestamp(value: unknown, field: string): string | undefined {
  if (value === undefined || value === null) return undefined;
  return isoTimestamp(value, field);
}

function requireString(value: unknown, field: string, max: number, min = 1): string {
  if (typeof value !== "string") invalid(`${field} must be a string`);
  const trimmed = value.trim();
  if (trimmed.length < min) invalid(`${field} must not be empty`);
  if (trimmed.length > max) invalid(`${field} must be at most ${max} characters`);
  return trimmed;
}

function validateEvidence(raw: unknown, now: Date): Evidence[] {
  if (raw === undefined || raw === null) return [];
  if (!Array.isArray(raw)) invalid("evidence must be an array");
  if (raw.length > MAX_EVIDENCE) invalid(`evidence must contain at most ${MAX_EVIDENCE} references`);
  return raw.map((entry, index) => {
    const where = `evidence[${index}]`;
    if (typeof entry !== "object" || entry === null) invalid(`${where} must be an object`);
    const item = entry as Record<string, unknown>;
    const kind = item.kind;
    const observedAt = optionalTimestamp(item.observedAt, `${where}.observedAt`) ?? now.toISOString();
    if (kind === "transcript") {
      const source = item.source;
      if (typeof source !== "string" || !HARNESSES.includes(source as Harness)) invalid(`${where}.source must be one of ${HARNESSES.join(", ")}`);
      const eventIds = item.eventIds ?? [];
      if (!Array.isArray(eventIds) || eventIds.some((id) => !Number.isInteger(id) || (id as number) < 0)) {
        invalid(`${where}.eventIds must be an array of non-negative integers`);
      }
      const evidence: Evidence = {
        kind: "transcript",
        source: source as Harness,
        locator: requireString(item.locator, `${where}.locator`, 2_048),
        eventIds: (eventIds as number[]).slice(0, 200),
        observedAt,
      };
      if (typeof item.excerptHash === "string") evidence.excerptHash = item.excerptHash.slice(0, 128);
      return evidence;
    }
    if (kind === "file") {
      const evidence: Evidence = {
        kind: "file",
        path: requireString(item.path, `${where}.path`, 2_048),
        observedAt,
      };
      if (typeof item.commit === "string" && item.commit.trim()) evidence.commit = item.commit.trim().slice(0, 128);
      if (item.line !== undefined) {
        if (!Number.isInteger(item.line) || (item.line as number) < 1) invalid(`${where}.line must be an integer >= 1`);
        evidence.line = item.line as number;
      }
      if (typeof item.contentHash === "string") evidence.contentHash = item.contentHash.slice(0, 128);
      return evidence;
    }
    if (kind === "user") {
      return { kind: "user", note: requireString(item.note, `${where}.note`, 2_048), observedAt };
    }
    return invalid(`${where}.kind must be transcript, file, or user`);
  });
}

/** Validate a mutation payload, rejecting unknown fields to catch agent typos. */
export function validateMemoryInput(raw: unknown, now = new Date()): MemoryInput {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) invalid("memory input must be a JSON object");
  const input = raw as Record<string, unknown>;
  for (const key of Object.keys(input)) {
    if (!INPUT_FIELDS.has(key)) invalid(`unknown field '${key}'`);
  }
  if (typeof input.kind !== "string" || !KINDS.includes(input.kind as MemoryKind)) {
    invalid(`kind must be one of ${KINDS.join(", ")}`);
  }
  if (typeof input.verification !== "string" || !VERIFICATIONS.includes(input.verification as Verification)) {
    invalid(`verification must be one of ${VERIFICATIONS.join(", ")}`);
  }
  const title = requireString(input.title, "title", TITLE_LIMIT);
  if (typeof input.body !== "string" || input.body.length === 0) invalid("body must be a non-empty string");
  if (input.body.length > BODY_LIMIT) invalid(`body must be at most ${BODY_LIMIT} characters`);

  const tags = input.tags === undefined || input.tags === null ? [] : input.tags;
  if (!Array.isArray(tags)) invalid("tags must be an array of strings");
  if (tags.length > MAX_TAGS) invalid(`tags must contain at most ${MAX_TAGS} entries`);
  const normalizedTags: string[] = [];
  for (const tag of tags) {
    if (typeof tag !== "string" || !tag.trim()) invalid("each tag must be a non-empty string");
    const value = tag.trim().toLowerCase();
    if (value.length > TAG_LIMIT) invalid(`each tag must be at most ${TAG_LIMIT} characters`);
    if (!normalizedTags.includes(value)) normalizedTags.push(value);
  }
  if (normalizedTags.length > MAX_TAGS) invalid(`tags must contain at most ${MAX_TAGS} distinct entries`);

  const evidence = validateEvidence(input.evidence, now);
  if (input.verification === "code_verified" && !evidence.some((item) => item.kind === "file" || item.kind === "transcript")) {
    invalid("code_verified requires at least one file or transcript evidence reference");
  }
  if (input.verification === "user_confirmed" && !evidence.some((item) => item.kind === "user" || item.kind === "transcript")) {
    invalid("user_confirmed requires a user or transcript evidence reference");
  }

  if (input.pinned !== undefined && typeof input.pinned !== "boolean") invalid("pinned must be a boolean");
  if (input.branch !== undefined && input.branch !== null && typeof input.branch !== "string") invalid("branch must be a string");
  if (input.pathPrefix !== undefined && input.pathPrefix !== null && typeof input.pathPrefix !== "string") invalid("pathPrefix must be a string");

  const kind = input.kind as MemoryKind;
  let expiresAt = optionalTimestamp(input.expiresAt, "expiresAt");
  const reviewAfter = optionalTimestamp(input.reviewAfter, "reviewAfter");
  if (kind === "handoff" && !expiresAt) {
    expiresAt = new Date(now.getTime() + HANDOFF_DEFAULT_DAYS * 86_400_000).toISOString();
  }
  if (reviewAfter && expiresAt && Date.parse(reviewAfter) > Date.parse(expiresAt)) {
    invalid("reviewAfter must not be later than expiresAt");
  }

  return {
    kind,
    title,
    body: input.body,
    pathPrefix: input.pathPrefix ? normalizePathPrefix(input.pathPrefix as string) : undefined,
    branch: input.branch ? requireString(input.branch, "branch", 512) : undefined,
    tags: normalizedTags,
    pinned: input.pinned === true,
    verification: input.verification as Verification,
    evidence,
    reviewAfter,
    expiresAt,
  };
}

/** Canonical content hash for duplicate diagnostics — never a uniqueness key. */
export function contentHash(input: MemoryInput): string {
  const canonical = JSON.stringify({
    kind: input.kind,
    title: input.title,
    body: input.body,
    pathPrefix: input.pathPrefix ?? null,
    branch: input.branch ?? null,
    tags: [...input.tags ?? []].sort(),
    verification: input.verification,
    evidence: input.evidence,
    reviewAfter: input.reviewAfter ?? null,
    expiresAt: input.expiresAt ?? null,
    pinned: input.pinned === true,
  });
  return `sha256:${createHash("sha256").update(canonical).digest("hex")}`;
}

export function hashRequest(payload: unknown): string {
  return `sha256:${createHash("sha256").update(JSON.stringify(payload)).digest("hex")}`;
}

export interface ServiceDeps {
  store: ProjectMemoryStore;
  now?: () => Date;
  uuid?: () => string;
  identity?: IdentityDeps;
}

export interface InitResult {
  project: ProjectRecord;
  binding: ProjectBinding;
  created: boolean;
}

export class ProjectMemoryService {
  readonly store: ProjectMemoryStore;
  private readonly now: () => Date;
  private readonly uuid: () => string;
  private readonly identity: IdentityDeps;

  constructor(deps: ServiceDeps) {
    this.store = deps.store;
    this.now = deps.now ?? (() => new Date());
    this.uuid = deps.uuid ?? (() => crypto.randomUUID());
    this.identity = deps.identity ?? {};
  }

  private timestamp(): string {
    return this.now().toISOString();
  }

  private git(cwd: string) {
    return (this.identity.git ?? probeGit)(cwd);
  }

  private async folderBindings(): Promise<ProjectBinding[]> {
    if (this.identity.listFolderBindings) return this.identity.listFolderBindings();
    return this.store.listBindings().filter((binding) => binding.kind === "folder");
  }

  // ------------------------------------------------------------- identity

  /** Determine the boundary that `cwd` belongs to, without consulting bindings. */
  async boundaryFor(cwd: string): Promise<{ canonicalCwd: string; boundary: Boundary | undefined; gitUnavailable?: string }> {
    const canonicalCwd = await canonicalize(cwd, this.identity);
    const git = await this.git(canonicalCwd);
    const folders = (await this.folderBindings()).filter((binding) => binding.kind === "folder");
    const boundary = selectBoundary(canonicalCwd, git, folders);
    return { canonicalCwd, boundary, gitUnavailable: git.unavailable };
  }

  async resolve(cwd: string, projectId?: string): Promise<ResolvedProject> {
    const { canonicalCwd, boundary } = await this.boundaryFor(cwd);
    if (projectId) {
      const project = this.store.getProject(projectId);
      if (!project) throw new ProjectMemoryError("PROJECT_NOT_FOUND", `no project '${projectId}'`, 4);
      const binding = boundary
        ? this.store.findBinding(boundary.kind, boundary.canonicalPath)
        : undefined;
      if (binding && binding.projectId !== projectId) {
        throw new ProjectMemoryError(
          "CROSS_PROJECT",
          `'${boundary!.root}' belongs to project ${binding.projectId}, not ${projectId}`,
          3,
        );
      }
      return {
        project,
        matched: binding ?? { kind: "folder", canonicalPath: canonicalCwd, projectId, createdAt: project.createdAt },
        worktrees: boundary?.worktrees ?? [],
        context: {
          projectId,
          cwd: canonicalCwd,
          root: boundary?.root ?? canonicalCwd,
          relativePath: boundary ? relativeToRoot(boundary.root, canonicalCwd) : undefined,
          branch: boundary?.branch,
        },
      };
    }
    if (!boundary) {
      throw new ProjectMemoryError(
        "PROJECT_NOT_INITIALIZED",
        `no project is initialized for '${canonicalCwd}'; run 'dejavu memory project init --cwd ${canonicalCwd}'`,
        4,
      );
    }
    const binding = this.store.findBinding(boundary.kind, boundary.canonicalPath);
    if (!binding) {
      throw new ProjectMemoryError(
        "PROJECT_NOT_INITIALIZED",
        `no project is bound to '${boundary.root}'; run 'dejavu memory project init --cwd ${boundary.root}'`,
        4,
      );
    }
    const project = this.store.getProject(binding.projectId);
    if (!project) {
      throw new ProjectMemoryError("BINDING_DANGLING", `binding ${binding.canonicalPath} references missing project ${binding.projectId}`, 1);
    }
    return {
      project,
      matched: binding,
      worktrees: boundary.worktrees ?? [],
      context: {
        projectId: project.id,
        cwd: canonicalCwd,
        root: boundary.root,
        relativePath: relativeToRoot(boundary.root, canonicalCwd),
        branch: boundary.branch,
      },
    };
  }

  async init(options: { cwd: string; name?: string; folder?: boolean }): Promise<InitResult> {
    const { canonicalCwd, boundary, gitUnavailable } = await this.boundaryFor(options.cwd);
    let chosen: Boundary;
    if (options.folder || !boundary) {
      // Not a repository, Git unavailable, or an explicit folder request: the
      // selected directory itself becomes the boundary.
      chosen = { kind: "folder", canonicalPath: canonicalCwd, root: canonicalCwd, branch: boundary?.branch };
    } else {
      chosen = boundary;
    }
    const existing = this.store.findBinding(chosen.kind, chosen.canonicalPath);
    if (existing) {
      const project = this.store.getProject(existing.projectId);
      if (!project) throw new ProjectMemoryError("BINDING_DANGLING", `binding references missing project ${existing.projectId}`, 1);
      return { project, binding: existing, created: false };
    }
    const at = this.timestamp();
    const project: ProjectRecord = {
      id: this.uuid(),
      name: options.name?.trim() || basename(chosen.root) || "project",
      createdAt: at,
      updatedAt: at,
    };
    const binding: ProjectBinding = {
      kind: chosen.kind,
      canonicalPath: chosen.canonicalPath,
      projectId: project.id,
      createdAt: at,
    };
    this.store.createProject(project);
    try {
      this.store.addBinding(binding);
    } catch (error) {
      // A concurrent init won the race; converge on the existing identity.
      const raced = this.store.findBinding(chosen.kind, chosen.canonicalPath);
      if (raced) {
        const winner = this.store.getProject(raced.projectId);
        if (winner) return { project: winner, binding: raced, created: false };
      }
      throw error;
    }
    return { project, binding, created: true };
  }

  async bind(projectId: string, cwd: string): Promise<ProjectBinding> {
    const project = this.store.getProject(projectId);
    if (!project) throw new ProjectMemoryError("PROJECT_NOT_FOUND", `no project '${projectId}'`, 4);
    const { canonicalCwd, boundary } = await this.boundaryFor(cwd);
    const chosen = boundary ?? { kind: "folder" as const, canonicalPath: canonicalCwd, root: canonicalCwd };
    const existing = this.store.findBinding(chosen.kind, chosen.canonicalPath);
    if (existing) {
      if (existing.projectId !== projectId) {
        throw new ProjectMemoryError(
          "BINDING_TAKEN",
          `'${chosen.canonicalPath}' is already bound to project ${existing.projectId}`,
          3,
        );
      }
      return existing;
    }
    const binding: ProjectBinding = {
      kind: chosen.kind,
      canonicalPath: chosen.canonicalPath,
      projectId,
      createdAt: this.timestamp(),
    };
    this.store.addBinding(binding);
    return binding;
  }

  async unbind(projectId: string, bindingPath: string): Promise<ProjectBinding> {
    const project = this.store.getProject(projectId);
    if (!project) throw new ProjectMemoryError("PROJECT_NOT_FOUND", `no project '${projectId}'`, 4);
    const { canonicalCwd, boundary } = await this.boundaryFor(bindingPath);
    const candidates: Array<{ kind: ProjectBinding["kind"]; canonicalPath: string }> = [];
    if (boundary) candidates.push({ kind: boundary.kind, canonicalPath: boundary.canonicalPath });
    candidates.push({ kind: "folder", canonicalPath: canonicalCwd });
    for (const candidate of candidates) {
      const existing = this.store.findBinding(candidate.kind, candidate.canonicalPath);
      if (existing) {
        if (existing.projectId !== projectId) {
          throw new ProjectMemoryError("CROSS_PROJECT", `'${candidate.canonicalPath}' belongs to project ${existing.projectId}`, 3);
        }
        this.store.removeBinding(candidate.kind, candidate.canonicalPath, projectId);
        return existing;
      }
    }
    throw new ProjectMemoryError("BINDING_NOT_FOUND", `project ${projectId} has no binding at '${bindingPath}'`, 4);
  }

  // -------------------------------------------------------------- memories

  private recordFrom(ctx: ProjectContext, input: MemoryInput, at: string, id?: string): MemoryRecord {
    const record: MemoryRecord = {
      id: id ?? this.uuid(),
      projectId: ctx.projectId,
      revision: 1,
      kind: input.kind,
      title: input.title,
      body: input.body,
      status: "active",
      verification: input.verification,
      pathPrefix: input.pathPrefix,
      branch: input.branch,
      pinned: input.pinned === true,
      tags: input.tags ?? [],
      evidence: input.evidence,
      createdAt: at,
      updatedAt: at,
      lastVerifiedAt: input.verification === "unverified" ? undefined : at,
      reviewAfter: input.reviewAfter,
      expiresAt: input.expiresAt,
      contentHash: contentHash(input),
    };
    return record;
  }

  private scoped(ctx: ProjectContext, id: string): MemoryRecord {
    const record = this.store.getMemory(ctx.projectId, id);
    if (!record) throw new ProjectMemoryError("MEMORY_NOT_FOUND", `no memory '${id}' in project ${ctx.projectId}`, 4);
    return record;
  }

  add(ctx: ProjectContext, raw: unknown, actor: Actor, requestId?: string): { record: MemoryRecord; replayed: boolean } {
    const input = validateMemoryInput(raw, this.now());
    const record = this.recordFrom(ctx, input, this.timestamp());
    const request: WriteRequestRef | undefined = requestId ? { id: requestId, hash: hashRequest(input) } : undefined;
    const outcome = this.store.createMemory(record, actor, "create", request);
    this.store.touchProject(ctx.projectId, record.createdAt);
    return { record: outcome.result, replayed: outcome.replayed };
  }

  update(ctx: ProjectContext, id: string, expectedRevision: number, raw: unknown, actor: Actor, requestId?: string): { record: MemoryRecord; replayed: boolean } {
    const input = validateMemoryInput(raw, this.now());
    const existing = this.scoped(ctx, id);
    if (existing.revision !== expectedRevision) {
      throw new ProjectMemoryError(
        "REVISION_CONFLICT",
        `memory ${id} is at revision ${existing.revision}, not ${expectedRevision}; re-read it and retry`,
        3,
      );
    }
    const at = this.timestamp();
    const next: MemoryRecord = {
      ...existing,
      revision: existing.revision + 1,
      kind: input.kind,
      title: input.title,
      body: input.body,
      verification: input.verification,
      pathPrefix: input.pathPrefix,
      branch: input.branch,
      pinned: input.pinned === true,
      tags: input.tags ?? [],
      evidence: input.evidence,
      reviewAfter: input.reviewAfter,
      expiresAt: input.expiresAt,
      updatedAt: at,
      lastVerifiedAt: input.verification === "unverified" ? existing.lastVerifiedAt : at,
      contentHash: contentHash(input),
    };
    const request: WriteRequestRef | undefined = requestId ? { id: requestId, hash: hashRequest({ id, expectedRevision, input }) } : undefined;
    const outcome = this.store.replaceMemory(next, actor, "update", at, request);
    return { record: outcome.result, replayed: outcome.replayed };
  }

  archive(ctx: ProjectContext, id: string, expectedRevision: number, reason: string, actor: Actor): MemoryRecord {
    const existing = this.scoped(ctx, id);
    if (existing.revision !== expectedRevision) {
      throw new ProjectMemoryError("REVISION_CONFLICT", `memory ${id} is at revision ${existing.revision}, not ${expectedRevision}`, 3);
    }
    if (existing.status === "archived") return existing;
    const at = this.timestamp();
    const next: MemoryRecord = { ...existing, revision: existing.revision + 1, status: "archived", updatedAt: at };
    return this.store.replaceMemory(next, actor, `archive: ${reason}`, at).result;
  }

  supersede(ctx: ProjectContext, id: string, expectedRevision: number, raw: unknown, actor: Actor, requestId?: string): { record: MemoryRecord; superseded: MemoryRecord; replayed: boolean } {
    const input = validateMemoryInput(raw, this.now());
    const existing = this.scoped(ctx, id);
    if (existing.revision !== expectedRevision) {
      throw new ProjectMemoryError("REVISION_CONFLICT", `memory ${id} is at revision ${existing.revision}, not ${expectedRevision}`, 3);
    }
    if (existing.status === "superseded") {
      throw new ProjectMemoryError("ALREADY_SUPERSEDED", `memory ${id} is already superseded`, 3);
    }
    const at = this.timestamp();
    const replacement = this.recordFrom(ctx, input, at);
    if (replacement.projectId !== existing.projectId) {
      throw new ProjectMemoryError("CROSS_PROJECT", "a supersession edge cannot cross projects", 3);
    }
    if (replacement.id === existing.id) {
      throw new ProjectMemoryError("SUPERSESSION_CYCLE", "a replacement cannot supersede itself", 3);
    }
    const superseded: MemoryRecord = {
      ...existing,
      revision: existing.revision + 1,
      status: "superseded",
      supersededBy: replacement.id,
      updatedAt: at,
    };
    const request: WriteRequestRef | undefined = requestId ? { id: requestId, hash: hashRequest({ id, expectedRevision, input }) } : undefined;
    const outcome = this.store.supersedeMemory(superseded, replacement, actor, `supersede ${id}`, at, request);
    return { record: outcome.result, superseded, replayed: outcome.replayed };
  }

  get(ctx: ProjectContext, id: string): MemoryRecord {
    return this.scoped(ctx, id);
  }

  list(ctx: ProjectContext, options: { includeInactive?: boolean; limit?: number } = {}): MemoryRecord[] {
    const status: MemoryStatus[] = options.includeInactive ? ["active", "archived", "superseded"] : ["active"];
    return this.store.listMemories(ctx.projectId, { status, limit: options.limit ?? 200 });
  }

  search(ctx: ProjectContext, phrase: string, options: { includeInactive?: boolean; limit?: number; snippets?: number; allProjects?: boolean } = {}) {
    const needle = phrase.trim().toLowerCase();
    if (!needle) throw new ProjectMemoryError("INVALID_ARGUMENT", "search needs one token or exact phrase", 2);
    const projects = options.allProjects ? this.store.listProjects() : [this.store.getProject(ctx.projectId)!];
    const status: MemoryStatus[] = options.includeInactive ? ["active", "archived", "superseded"] : ["active"];
    const results: Array<{ projectId: string; projectName: string; record: MemoryRecord; count: number; snippets: string[] }> = [];
    for (const project of projects.filter(Boolean)) {
      for (const record of this.store.listMemories(project.id, { status, limit: 1_000 })) {
        const haystack = `${record.title}\n${record.body}\n${record.tags.join(" ")}`.toLowerCase();
        let count = 0;
        for (let index = 0; (index = haystack.indexOf(needle, index)) >= 0; index += needle.length) count += 1;
        if (count === 0) continue;
        const lines = `${record.title}\n${record.body}`.split(/\r?\n/).filter((line) => line.toLowerCase().includes(needle));
        results.push({
          projectId: project.id,
          projectName: project.name,
          record,
          count,
          snippets: lines.slice(0, options.snippets ?? 3),
        });
      }
    }
    results.sort((a, b) => b.count - a.count || a.record.id.localeCompare(b.record.id));
    return results.slice(0, options.limit ?? 20);
  }

  recall(
    ctx: ProjectContext,
    projectName: string,
    options: { query?: string; targetPath?: string; budgetChars?: number; limit?: number; includeUnverified?: boolean } = {},
  ): RecallOutcome {
    const budgetChars = options.budgetChars ?? DEFAULT_RECALL_BUDGET;
    if (!Number.isInteger(budgetChars) || budgetChars < 1) {
      throw new ProjectMemoryError("INVALID_ARGUMENT", "budgetChars must be a positive integer", 2);
    }
    if (budgetChars > MAX_RECALL_BUDGET) {
      throw new ProjectMemoryError("INVALID_ARGUMENT", `budgetChars must be at most ${MAX_RECALL_BUDGET}`, 2);
    }
    const records = this.store.listMemories(ctx.projectId, { status: ["active"], limit: 5_000 });
    return recall(projectName, records, {
      query: options.query,
      targetPath: options.targetPath ?? ctx.relativePath,
      branch: ctx.branch,
      now: this.now(),
      budgetChars,
      limit: options.limit ?? DEFAULT_RECALL_RECORDS,
    }, { includeUnverified: options.includeUnverified });
  }

  history(ctx: ProjectContext, id: string) {
    this.scoped(ctx, id);
    return this.store.revisions(id);
  }

  purge(ctx: ProjectContext, id: string, expectedRevision: number, reason: string) {
    const existing = this.scoped(ctx, id);
    if (existing.revision !== expectedRevision) {
      throw new ProjectMemoryError("REVISION_CONFLICT", `memory ${id} is at revision ${existing.revision}, not ${expectedRevision}`, 3);
    }
    return { ...this.store.purgeMemory(ctx.projectId, id), reason, purged: existing };
  }

  projects(): Array<{ project: ProjectRecord; bindings: ProjectBinding[]; memories: number }> {
    return this.store.listProjects().map((project) => ({
      project,
      bindings: this.store.listBindings().filter((binding) => binding.projectId === project.id),
      memories: this.store.memoryCount(project.id),
    }));
  }

  doctor(): {
    path: string;
    schemaVersion: number;
    integrity: string;
    projects: number;
    memories: number;
    bindings: number;
    stale: string[];
  } {
    const stale: string[] = [];
    const projects = new Set(this.store.listProjects().map((project) => project.id));
    for (const binding of this.store.listBindings()) {
      if (!projects.has(binding.projectId)) stale.push(`binding ${binding.kind}:${binding.canonicalPath} -> missing project ${binding.projectId}`);
    }
    return {
      path: this.store.path,
      schemaVersion: this.store.schemaVersion(),
      integrity: this.store.integrity(),
      projects: projects.size,
      memories: this.store.listProjects().reduce((total, project) => total + this.store.memoryCount(project.id), 0),
      bindings: this.store.listBindings().length,
      stale,
    };
  }
}

export { renderRecall };
