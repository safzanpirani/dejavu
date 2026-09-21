/**
 * Export, import, and explicit Claude-memory migration.
 *
 * Migration is repeatable and never touches the source files. Unverified
 * imports stay out of automatic recall until a human or agent confirms them.
 */

import { createHash } from "node:crypto";
import { readdir, readFile, stat, writeFile, rename } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { ProjectMemoryStore } from "./project-memory-store.ts";
import { contentHash, validateMemoryInput } from "./project-memory.ts";
import { canonicalize } from "./project-identity.ts";
import { ProjectMemoryError } from "./project-memory-types.ts";
import type { MemoryInput, MemoryKind, MemoryRecord, MemoryStatus, Verification } from "./project-memory-types.ts";

export const SNAPSHOT_KIND = "dejavu-project-memory";
export const SNAPSHOT_VERSION = 1;
const MAX_IMPORT_BYTES = 8 * 1024 * 1024;
const CLAUDE_SOURCE_KIND = "claude_memory";

interface SnapshotRecord {
  id: string;
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
  evidence: MemoryRecord["evidence"];
  createdAt: string;
  updatedAt: string;
  lastVerifiedAt?: string;
  reviewAfter?: string;
  expiresAt?: string;
  supersededBy?: string;
  contentHash: string;
  provenanceReduced?: true;
}

export interface Snapshot {
  kind: typeof SNAPSHOT_KIND;
  formatVersion: number;
  exportedAt: string;
  project: { id: string; name: string };
  includeLocalMetadata: boolean;
  records: SnapshotRecord[];
}

function isAbsoluteLocal(path: string): boolean {
  return path.startsWith("/") || /^[A-Za-z]:[\\/]/.test(path);
}

/** Drop absolute local provenance paths unless explicitly requested. */
function reduceProvenance(record: MemoryRecord): SnapshotRecord {
  let reduced = false;
  const evidence = record.evidence.filter((item) => {
    if (item.kind === "file" && isAbsoluteLocal(item.path)) {
      reduced = true;
      return false;
    }
    if (item.kind === "transcript" && isAbsoluteLocal(item.locator)) {
      reduced = true;
      return false;
    }
    return true;
  });
  const snapshot: SnapshotRecord = {
    id: record.id,
    revision: record.revision,
    kind: record.kind,
    title: record.title,
    body: record.body,
    status: record.status,
    verification: record.verification,
    pathPrefix: record.pathPrefix,
    branch: record.branch,
    pinned: record.pinned,
    tags: record.tags,
    evidence,
    createdAt: record.createdAt,
    updatedAt: record.updatedAt,
    lastVerifiedAt: record.lastVerifiedAt,
    reviewAfter: record.reviewAfter,
    expiresAt: record.expiresAt,
    supersededBy: record.supersededBy,
    contentHash: record.contentHash,
  };
  if (reduced) snapshot.provenanceReduced = true;
  return snapshot;
}

export function buildSnapshot(store: ProjectMemoryStore, projectId: string, options: { includeLocalMetadata?: boolean } = {}): Snapshot {
  const project = store.getProject(projectId);
  if (!project) throw new ProjectMemoryError("PROJECT_NOT_FOUND", `no project '${projectId}'`, 4);
  const includeLocalMetadata = options.includeLocalMetadata === true;
  const records = store.allMemories(projectId).map((record) =>
    includeLocalMetadata ? (record as unknown as SnapshotRecord) : reduceProvenance(record));
  return {
    kind: SNAPSHOT_KIND,
    formatVersion: SNAPSHOT_VERSION,
    exportedAt: new Date().toISOString(),
    project: { id: project.id, name: project.name },
    includeLocalMetadata,
    records,
  };
}

export function renderMarkdown(snapshot: Snapshot): string {
  const lines = [
    `# Project memory export — ${snapshot.project.name}`,
    "",
    `- project id: \`${snapshot.project.id}\``,
    `- exported: ${snapshot.exportedAt}`,
    `- formatVersion: ${snapshot.formatVersion}`,
    `- records: ${snapshot.records.length}`,
    "",
  ];
  for (const record of snapshot.records) {
    lines.push(`## ${record.id} · ${record.title}`, "");
    lines.push(`- kind: ${record.kind}`);
    lines.push(`- revision: ${record.revision}`);
    lines.push(`- status: ${record.status}`);
    lines.push(`- verification: ${record.verification}`);
    if (record.pathPrefix) lines.push(`- path: ${record.pathPrefix}`);
    if (record.branch) lines.push(`- branch: ${record.branch}`);
    if (record.tags.length) lines.push(`- tags: ${record.tags.join(", ")}`);
    if (record.pinned) lines.push("- pinned: true");
    lines.push(`- created: ${record.createdAt}`);
    lines.push(`- updated: ${record.updatedAt}`);
    if (record.expiresAt) lines.push(`- expires: ${record.expiresAt}`);
    if (record.supersededBy) lines.push(`- superseded by: ${record.supersededBy}`);
    if (record.provenanceReduced) lines.push("- provenance: reduced (local paths omitted)");
    lines.push("", record.body, "");
    if (record.evidence.length) {
      lines.push("Evidence:", ...record.evidence.map((item) =>
        item.kind === "file" ? `- file: ${item.path}${item.line ? `:${item.line}` : ""}`
          : item.kind === "transcript" ? `- transcript: ${item.source} ${item.locator}`
            : `- user: ${item.note}`), "");
    }
  }
  return lines.join("\n");
}

/** Atomic write: temp file in the destination directory, then rename. */
export async function writeExport(output: string, content: string): Promise<void> {
  const temporary = join(dirname(output), `.${basename(output)}.tmp-${process.pid}`);
  await writeFile(temporary, content, { mode: 0o600 });
  await rename(temporary, output);
}

// ------------------------------------------------------------------- import

export interface ImportPlanEntry {
  id: string;
  title: string;
  action: "create" | "skip-identical" | "conflict" | "remap";
}

export interface ImportPlan {
  project: { id: string; name: string };
  createProject: boolean;
  entries: ImportPlanEntry[];
  rejected: Array<{ id: string; reason: string }>;
  diagnostics: string[];
}

function parseSnapshot(raw: string): Snapshot {
  if (raw.length > MAX_IMPORT_BYTES) throw new ProjectMemoryError("IMPORT_TOO_LARGE", "import file exceeds the size limit", 2);
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new ProjectMemoryError("IMPORT_INVALID_JSON", `import file is not valid JSON: ${error instanceof Error ? error.message : String(error)}`, 2);
  }
  if (typeof parsed !== "object" || parsed === null) throw new ProjectMemoryError("IMPORT_INVALID", "import file must contain a JSON object", 2);
  const snapshot = parsed as Partial<Snapshot>;
  if (snapshot.kind !== SNAPSHOT_KIND) throw new ProjectMemoryError("IMPORT_INVALID", `import file kind must be '${SNAPSHOT_KIND}'`, 2);
  if (snapshot.formatVersion !== SNAPSHOT_VERSION) {
    throw new ProjectMemoryError("IMPORT_VERSION", `unsupported import formatVersion ${String(snapshot.formatVersion)}`, 2);
  }
  if (!snapshot.project || typeof snapshot.project.id !== "string" || typeof snapshot.project.name !== "string") {
    throw new ProjectMemoryError("IMPORT_INVALID", "import file is missing project identity", 2);
  }
  if (!Array.isArray(snapshot.records)) throw new ProjectMemoryError("IMPORT_INVALID", "import file is missing a records array", 2);
  return snapshot as Snapshot;
}

function validateSnapshotRecord(record: SnapshotRecord, now: Date): MemoryInput {
  if (typeof record !== "object" || record === null) throw new ProjectMemoryError("IMPORT_INVALID", "each record must be an object", 2);
  if (typeof record.id !== "string" || !record.id.trim()) throw new ProjectMemoryError("IMPORT_INVALID", "each record needs a string id", 2);
  if (!["active", "archived", "superseded"].includes(record.status)) throw new ProjectMemoryError("IMPORT_INVALID", `record ${record.id} has an invalid status`, 2);
  if (record.supersededBy !== undefined && typeof record.supersededBy !== "string") {
    throw new ProjectMemoryError("IMPORT_INVALID", `record ${record.id} has an invalid supersededBy`, 2);
  }
  return validateMemoryInput({
    kind: record.kind,
    title: record.title,
    body: record.body,
    pathPrefix: record.pathPrefix,
    branch: record.branch,
    tags: record.tags,
    pinned: record.pinned,
    verification: record.verification,
    evidence: record.evidence,
    reviewAfter: record.reviewAfter,
    expiresAt: record.expiresAt,
  }, now);
}

export function planImport(
  store: ProjectMemoryStore | undefined,
  raw: string,
  options: { projectId?: string; createProjectName?: string },
  now = new Date(),
): ImportPlan {
  const snapshot = parseSnapshot(raw);
  const restoringSameProject = options.projectId !== undefined && options.projectId === snapshot.project.id;
  const destinationId = options.projectId ?? undefined;
  if (!destinationId && !options.createProjectName) {
    throw new ProjectMemoryError("IMPORT_DESTINATION", "import needs --project-id or --create-project NAME", 2);
  }
  const diagnostics: string[] = [];
  if (!snapshot.includeLocalMetadata) diagnostics.push("snapshot was exported without local metadata; bindings were not imported");
  const existing = destinationId && store ? store.getProject(destinationId) : undefined;
  if (destinationId && !existing && !options.createProjectName) {
    throw new ProjectMemoryError("PROJECT_NOT_FOUND", `no project '${destinationId}' to import into`, 4);
  }

  const ids = new Set<string>();
  const rejected: ImportPlan["rejected"] = [];
  const entries: ImportPlanEntry[] = [];
  const remap = new Map<string, string>();
  const records: SnapshotRecord[] = [];

  for (const record of snapshot.records) {
    let validated: MemoryInput;
    try {
      validated = validateSnapshotRecord(record, now);
    } catch (error) {
      rejected.push({ id: String(record?.id ?? "?"), reason: error instanceof Error ? error.message : String(error) });
      continue;
    }
    if (ids.has(record.id)) {
      rejected.push({ id: record.id, reason: "duplicate id inside the snapshot" });
      continue;
    }
    ids.add(record.id);
    // Trust the content, not the recorded hash: a tampered or stale hash must
    // not turn a real conflict into a silent skip.
    const effectiveHash = contentHash(validated);
    const existingRecord = store && destinationId
      ? store.getMemory(destinationId, record.id)
      : undefined;
    if (existingRecord) {
      if (existingRecord.contentHash === effectiveHash) {
        entries.push({ id: record.id, title: record.title, action: "skip-identical" });
      } else {
        entries.push({ id: record.id, title: record.title, action: "conflict" });
      }
      continue;
    }
    entries.push({ id: record.id, title: record.title, action: restoringSameProject ? "create" : "remap" });
    records.push(record);
  }

  if (!restoringSameProject) for (const record of records) remap.set(record.id, crypto.randomUUID());
  for (const record of records) {
    if (record.supersededBy && !ids.has(record.supersededBy)) {
      rejected.push({ id: record.id, reason: `supersession points at '${record.supersededBy}', which is not in the snapshot` });
    }
  }
  const blocked = new Set(rejected.map((entry) => entry.id));
  if (blocked.size > 0) {
    for (let index = records.length - 1; index >= 0; index -= 1) {
      if (blocked.has(records[index]!.id)) records.splice(index, 1);
    }
    for (let index = entries.length - 1; index >= 0; index -= 1) {
      if (blocked.has(entries[index]!.id)) entries.splice(index, 1);
    }
  }
  void remap;
  return {
    project: { id: destinationId ?? "new", name: options.createProjectName ?? existing?.name ?? snapshot.project.name },
    createProject: Boolean(options.createProjectName) && !existing,
    entries,
    rejected,
    diagnostics,
  };
}

export interface ImportResult {
  projectId: string;
  imported: number;
  skipped: number;
  remapped: number;
  conflicts: string[];
  rejected: Array<{ id: string; reason: string }>;
  diagnostics: string[];
}

export function importSnapshot(
  store: ProjectMemoryStore,
  raw: string,
  options: { projectId?: string; createProjectName?: string; dryRun?: boolean },
  now = new Date(),
): ImportResult {
  const plan = planImport(store, raw, options, now);
  const snapshot = parseSnapshot(raw);
  const restoringSameProject = options.projectId !== undefined && options.projectId === snapshot.project.id;

  let projectId = options.projectId;
  if (!projectId && options.createProjectName) {
    if (plan.createProject && !options.dryRun) {
      const at = now.toISOString();
      const project = { id: crypto.randomUUID(), name: options.createProjectName, createdAt: at, updatedAt: at };
      store.createProject(project);
      projectId = project.id;
    } else if (plan.createProject) {
      projectId = "dry-run";
    }
  }
  if (!projectId) throw new ProjectMemoryError("IMPORT_DESTINATION", "import needs a destination project", 2);
  const conflicts = plan.entries.filter((entry) => entry.action === "conflict").map((entry) => entry.id);
  if (conflicts.length > 0) {
    throw new ProjectMemoryError("IMPORT_CONFLICT", `records already exist with different content: ${conflicts.join(", ")}`, 3);
  }
  if (options.dryRun) {
    return {
      projectId,
      imported: 0,
      skipped: plan.entries.filter((entry) => entry.action === "skip-identical").length,
      remapped: plan.entries.filter((entry) => entry.action === "remap").length,
      conflicts,
      rejected: plan.rejected,
      diagnostics: [...plan.diagnostics, "dry run: no database writes"],
    };
  }

  const remap = new Map<string, string>();
  const keep = plan.entries.filter((entry) => entry.action === "create" || entry.action === "remap").map((entry) => entry.id);
  if (!restoringSameProject) for (const id of keep) remap.set(id, crypto.randomUUID());

  const records: MemoryRecord[] = [];
  for (const record of snapshot.records) {
    if (!keep.includes(record.id)) continue;
    const input = validateSnapshotRecord(record, now);
    const id = remap.get(record.id) ?? record.id;
    const supersededBy = record.supersededBy ? (remap.get(record.supersededBy) ?? record.supersededBy) : undefined;
    records.push({
      id,
      projectId,
      revision: record.revision,
      kind: input.kind,
      title: input.title,
      body: input.body,
      status: record.status,
      verification: input.verification,
      pathPrefix: input.pathPrefix,
      branch: input.branch,
      pinned: input.pinned === true,
      tags: input.tags ?? [],
      evidence: input.evidence,
      createdAt: record.createdAt,
      updatedAt: record.updatedAt,
      lastVerifiedAt: record.lastVerifiedAt,
      reviewAfter: input.reviewAfter,
      expiresAt: input.expiresAt,
      supersededBy,
      contentHash: contentHash(input),
    });
  }
  store.insertImported(records, []);
  return {
    projectId,
    imported: records.length,
    skipped: plan.entries.filter((entry) => entry.action === "skip-identical").length,
    remapped: remap.size,
    conflicts,
    rejected: plan.rejected,
    diagnostics: plan.diagnostics,
  };
}

// --------------------------------------------------------- Claude migration

export interface ClaudeImportEntry {
  file: string;
  title: string;
  action: "import" | "skip-unchanged" | "review-changed" | "skip-index" | "too-large";
  memoryId?: string;
}

export interface ClaudeImportPlan {
  source: string;
  projectId: string;
  entries: ClaudeImportEntry[];
  diagnostics: string[];
}

function looksLikeIndex(text: string): boolean {
  const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  if (lines.length === 0) return false;
  const linkish = lines.filter((line) => /^[-*]\s/.test(line) || line.includes("](") || line.includes("[[")).length;
  return linkish / lines.length >= 0.6;
}

function titleFromMarkdown(text: string, fallback: string): string {
  for (const line of text.split(/\r?\n/)) {
    if (line.startsWith("# ")) {
      const title = line.slice(2).trim();
      if (title) return title.slice(0, 160);
    }
  }
  return fallback.slice(0, 160);
}

/** Plan a Claude memory-directory migration without touching either side. */
export async function planClaudeImport(
  store: ProjectMemoryStore,
  sourceDir: string,
  projectId: string,
  kind: MemoryKind = "convention",
): Promise<{ plan: ClaudeImportPlan; payloads: Array<{ file: string; content: string; hash: string; input: MemoryInput }> }> {
  const canonicalSource = await canonicalize(sourceDir);
  const project = store.getProject(projectId);
  if (!project) throw new ProjectMemoryError("PROJECT_NOT_FOUND", `no project '${projectId}'`, 4);
  const entries: ClaudeImportEntry[] = [];
  const payloads: Array<{ file: string; content: string; hash: string; input: MemoryInput }> = [];
  const diagnostics: string[] = [];
  const names = (await readdir(canonicalSource)).filter((name) => name.endsWith(".md")).sort();
  if (names.length === 0) diagnostics.push(`no .md files found in ${canonicalSource}`);

  for (const name of names) {
    const file = join(canonicalSource, name);
    const info = await stat(file);
    if (info.size > MAX_IMPORT_BYTES) {
      entries.push({ file, title: name, action: "too-large" });
      continue;
    }
    const content = await readFile(file, "utf8");
    const hash = createHash("sha256").update(content).digest("hex");
    const existing = store.findImportMapping(projectId, CLAUDE_SOURCE_KIND, file);
    if (name === "MEMORY.md" && looksLikeIndex(content)) {
      entries.push({ file, title: name, action: "skip-index", memoryId: existing?.memoryId });
      continue;
    }
    if (existing && existing.sourceHash === hash) {
      entries.push({ file, title: name, action: "skip-unchanged", memoryId: existing.memoryId });
      continue;
    }
    if (existing) {
      const current = store.getMemory(projectId, existing.memoryId);
      const edited = current ? current.revision > 1 : false;
      entries.push({ file, title: name, action: "review-changed", memoryId: existing.memoryId });
      if (edited) diagnostics.push(`${name}: source changed after an agent edited the imported record; review manually`);
      continue;
    }
    const input = validateMemoryInput({
      kind,
      title: titleFromMarkdown(content, name.replace(/\.md$/, "")),
      body: content.slice(0, 16_000),
      verification: "unverified",
      tags: ["claude-import"],
      evidence: [{ kind: "file", path: file, observedAt: new Date().toISOString(), contentHash: hash }],
    });
    entries.push({ file, title: name, action: "import" });
    payloads.push({ file, content, hash, input });
  }
  if (entries.some((entry) => entry.action === "too-large")) diagnostics.push("some files exceed the body limit and need manual splitting");
  return { plan: { source: canonicalSource, projectId, entries, diagnostics }, payloads };
}

export function applyClaudeImport(
  store: ProjectMemoryStore,
  service: { add: (ctx: { projectId: string; cwd: string; root: string }, raw: unknown, actor: { harness: "manual" }, requestId?: string) => { record: MemoryRecord } },
  planned: Awaited<ReturnType<typeof planClaudeImport>>,
  ctx: { projectId: string; cwd: string; root: string },
): { imported: number; records: MemoryRecord[] } {
  const records: MemoryRecord[] = [];
  const mappings: Array<{ projectId: string; sourceKind: string; sourceKey: string; sourceHash: string; memoryId: string }> = [];
  for (const payload of planned.payloads) {
    const record = service.add(ctx, payload.input, { harness: "manual" }).record;
    records.push(record);
    mappings.push({
      projectId: ctx.projectId,
      sourceKind: CLAUDE_SOURCE_KIND,
      sourceKey: payload.file,
      sourceHash: payload.hash,
      memoryId: record.id,
    });
  }
  for (const entry of planned.plan.entries) {
    if (entry.action !== "review-changed" || !entry.memoryId) continue;
    const payload = planned.payloads.find((candidate) => candidate.file === entry.file);
    if (payload) {
      mappings.push({
        projectId: ctx.projectId,
        sourceKind: CLAUDE_SOURCE_KIND,
        sourceKey: entry.file,
        sourceHash: payload.hash,
        memoryId: entry.memoryId,
      });
    }
  }
  if (records.length > 0 || mappings.length > 0) {
    // Records were inserted through the service; only fresh mappings remain.
    const missing = mappings.filter((mapping) => !store.findImportMapping(mapping.projectId, mapping.sourceKind, mapping.sourceKey));
    if (missing.length > 0) store.insertImported([], missing);
  }
  return { imported: records.length, records };
}
