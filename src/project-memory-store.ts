/**
 * Authoritative project memory storage.
 *
 * This database is deliberately separate from the disposable transcript index
 * in `transcript-index.ts`. Wiping `~/.cache/dejavu/` or rebuilding transcripts
 * must never touch a memory record.
 */

import { Database } from "bun:sqlite";
import { existsSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { ProjectMemoryError } from "./project-memory-types.ts";
import type {
  Actor,
  MemoryRecord,
  MemoryStatus,
  ProjectBinding,
  ProjectRecord,
} from "./project-memory-types.ts";

export const SCHEMA_VERSION = 1;

export function defaultMemoryDbPath(): string {
  const base = process.env.XDG_DATA_HOME?.trim() || join(homedir(), ".local", "share");
  return join(base, "dejavu", "memory.sqlite");
}

/** Resolve the database path. Never inherits `DEJAVU_INDEX_PATH`. */
export function resolveMemoryDbPath(explicit?: string): string {
  return explicit ?? process.env.DEJAVU_MEMORY_DB?.trim() ?? defaultMemoryDbPath();
}

export interface RevisionRecord {
  memoryId: string;
  revision: number;
  snapshot: MemoryRecord;
  actor: Actor;
  reason: string;
  createdAt: string;
}

/** Optional idempotency key for a mutation, scoped to one project. */
export interface WriteRequestRef {
  id: string;
  hash: string;
}

export interface MemoryRow {
  id: string;
  projectId: string;
  revision: number;
  kind: MemoryRecord["kind"];
  title: string;
  body: string;
  status: MemoryStatus;
  verification: MemoryRecord["verification"];
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
}

interface RawMemoryRow {
  id: string;
  project_id: string;
  revision: number;
  kind: MemoryRecord["kind"];
  title: string;
  body: string;
  status: MemoryStatus;
  verification: MemoryRecord["verification"];
  path_prefix: string | null;
  branch: string | null;
  pinned: number;
  tags_json: string;
  evidence_json: string;
  created_at: string;
  updated_at: string;
  last_verified_at: string | null;
  review_after: string | null;
  expires_at: string | null;
  superseded_by: string | null;
  content_hash: string;
}

const MIGRATIONS: Array<{ version: number; statements: string[] }> = [
  {
    version: 1,
    statements: [
      `CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      )`,
      `CREATE TABLE project_bindings (
        kind TEXT NOT NULL CHECK (kind IN ('git_common_dir', 'folder')),
        canonical_path TEXT NOT NULL,
        project_id TEXT NOT NULL REFERENCES projects(id),
        created_at TEXT NOT NULL,
        PRIMARY KEY (kind, canonical_path)
      )`,
      `CREATE TABLE memories (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id),
        revision INTEGER NOT NULL CHECK (revision >= 1),
        kind TEXT NOT NULL CHECK (kind IN
          ('decision', 'convention', 'procedure', 'pitfall', 'handoff')),
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('active', 'archived', 'superseded')),
        verification TEXT NOT NULL CHECK (verification IN
          ('unverified', 'user_confirmed', 'code_verified')),
        path_prefix TEXT,
        branch TEXT,
        pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
        tags_json TEXT NOT NULL,
        evidence_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        last_verified_at TEXT,
        review_after TEXT,
        expires_at TEXT,
        superseded_by TEXT REFERENCES memories(id),
        content_hash TEXT NOT NULL
      )`,
      `CREATE INDEX memories_project_status ON memories(project_id, status)`,
      `CREATE TABLE memory_revisions (
        memory_id TEXT NOT NULL REFERENCES memories(id),
        revision INTEGER NOT NULL,
        snapshot_json TEXT NOT NULL,
        actor_json TEXT NOT NULL,
        reason TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (memory_id, revision)
      )`,
      `CREATE TABLE write_requests (
        project_id TEXT NOT NULL REFERENCES projects(id),
        request_id TEXT NOT NULL,
        input_hash TEXT NOT NULL,
        result_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (project_id, request_id)
      )`,
      `CREATE TABLE import_mappings (
        project_id TEXT NOT NULL REFERENCES projects(id),
        source_kind TEXT NOT NULL,
        source_key TEXT NOT NULL,
        source_hash TEXT NOT NULL,
        memory_id TEXT NOT NULL REFERENCES memories(id),
        PRIMARY KEY (project_id, source_kind, source_key, source_hash)
      )`,
    ],
  },
];

function toRecord(row: RawMemoryRow): MemoryRecord {
  return {
    id: row.id,
    projectId: row.project_id,
    revision: row.revision,
    kind: row.kind,
    title: row.title,
    body: row.body,
    status: row.status,
    verification: row.verification,
    pathPrefix: row.path_prefix ?? undefined,
    branch: row.branch ?? undefined,
    pinned: row.pinned === 1,
    tags: JSON.parse(row.tags_json) as string[],
    evidence: JSON.parse(row.evidence_json) as MemoryRecord["evidence"],
    createdAt: row.created_at,
    updatedAt: row.updated_at,
    lastVerifiedAt: row.last_verified_at ?? undefined,
    reviewAfter: row.review_after ?? undefined,
    expiresAt: row.expires_at ?? undefined,
    supersededBy: row.superseded_by ?? undefined,
    contentHash: row.content_hash,
  };
}

function applySchema(db: Database, target: number, backup: (version: number) => string | undefined): void {
  const current = Number((db.query("PRAGMA user_version").get() as { user_version: number }).user_version);
  if (current === target) return;
  if (current > target) {
    throw new ProjectMemoryError(
      "SCHEMA_TOO_NEW",
      `memory database schema v${current} is newer than this build (v${target}); refusing to reset it`,
      1,
    );
  }
  for (const migration of MIGRATIONS) {
    if (migration.version <= current) continue;
    if (migration.version === target) backup(migration.version);
    const run = db.transaction(() => {
      for (const statement of migration.statements) db.exec(statement);
      db.exec(`PRAGMA user_version = ${migration.version}`);
    });
    try {
      run.immediate();
    } catch (error) {
      throw new ProjectMemoryError(
        "MIGRATION_FAILED",
        `memory schema migration to v${migration.version} failed: ${error instanceof Error ? error.message : String(error)}`,
        1,
      );
    }
  }
}

export interface StoreOptions {
  path: string;
  /** Skip migrations and open read-only. */
  readOnly?: boolean;
  busyTimeoutMs?: number;
  backupDir?: string;
}

export class ProjectMemoryStore {
  readonly path: string;
  private readonly db: Database;
  readonly readOnly: boolean;

  private constructor(path: string, db: Database, readOnly: boolean) {
    this.path = path;
    this.db = db;
    this.readOnly = readOnly;
  }

  /**
   * Open for writing, creating and migrating the store as needed.
   * Mutations initialize the database explicitly.
   */
  static open(options: StoreOptions): ProjectMemoryStore {
    const path = options.path;
    mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
    const db = new Database(path, { create: true });
    db.exec("PRAGMA foreign_keys = ON");
    db.exec(`PRAGMA busy_timeout = ${options.busyTimeoutMs ?? 3_000}`);
    db.exec("PRAGMA journal_mode = WAL");
    db.exec("PRAGMA synchronous = FULL");
    applySchema(db, SCHEMA_VERSION, (version) => snapshotBeforeMigration(db, path, version, options.backupDir));
    return new ProjectMemoryStore(path, db, false);
  }

  /**
   * Open for reading without creating or migrating anything.
   * Returns undefined when the store does not exist or is not current.
   */
  static openRead(options: StoreOptions): ProjectMemoryStore | undefined {
    if (!existsSync(options.path)) return undefined;
    const db = new Database(options.path, { readonly: true });
    db.exec(`PRAGMA busy_timeout = ${options.busyTimeoutMs ?? 250}`);
    const version = Number((db.query("PRAGMA user_version").get() as { user_version: number }).user_version);
    if (version !== SCHEMA_VERSION) {
      db.close();
      return undefined;
    }
    return new ProjectMemoryStore(options.path, db, true);
  }

  close(): void {
    this.db.close();
  }

  /** Run a write transaction with BEGIN IMMEDIATE so writers serialize. */
  private write<T>(fn: () => T): T {
    if (this.readOnly) throw new ProjectMemoryError("STORE_READ_ONLY", "memory store is open read-only", 1);
    return this.db.transaction(fn).immediate();
  }

  // ---------------------------------------------------------------- projects

  createProject(project: ProjectRecord): ProjectRecord {
    return this.write(() => {
      this.db.run(
        "INSERT INTO projects (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)",
        [project.id, project.name, project.createdAt, project.updatedAt],
      );
      return project;
    });
  }

  getProject(id: string): ProjectRecord | undefined {
    const row = this.db.query("SELECT id, name, created_at, updated_at FROM projects WHERE id = ?")
      .get(id) as { id: string; name: string; created_at: string; updated_at: string } | null;
    return row ? { id: row.id, name: row.name, createdAt: row.created_at, updatedAt: row.updated_at } : undefined;
  }

  listProjects(): ProjectRecord[] {
    return (this.db.query("SELECT id, name, created_at, updated_at FROM projects ORDER BY created_at, id")
      .all() as Array<{ id: string; name: string; created_at: string; updated_at: string }>)
      .map((row) => ({ id: row.id, name: row.name, createdAt: row.created_at, updatedAt: row.updated_at }));
  }

  touchProject(id: string, at: string): void {
    this.write(() => {
      this.db.run("UPDATE projects SET updated_at = ? WHERE id = ?", [at, id]);
    });
  }

  projectCount(): number {
    return Number((this.db.query("SELECT COUNT(*) AS n FROM projects").get() as { n: number }).n);
  }

  // ---------------------------------------------------------------- bindings

  addBinding(binding: ProjectBinding): void {
    this.write(() => {
      this.db.run(
        "INSERT INTO project_bindings (kind, canonical_path, project_id, created_at) VALUES (?, ?, ?, ?)",
        [binding.kind, binding.canonicalPath, binding.projectId, binding.createdAt],
      );
    });
  }

  listBindings(): ProjectBinding[] {
    return (this.db.query("SELECT kind, canonical_path, project_id, created_at FROM project_bindings ORDER BY canonical_path")
      .all() as Array<{ kind: ProjectBinding["kind"]; canonical_path: string; project_id: string; created_at: string }>)
      .map((row) => ({ kind: row.kind, canonicalPath: row.canonical_path, projectId: row.project_id, createdAt: row.created_at }));
  }

  findBinding(kind: ProjectBinding["kind"], canonicalPath: string): ProjectBinding | undefined {
    const row = this.db.query(
      "SELECT kind, canonical_path, project_id, created_at FROM project_bindings WHERE kind = ? AND canonical_path = ?",
    ).get(kind, canonicalPath) as { kind: ProjectBinding["kind"]; canonical_path: string; project_id: string; created_at: string } | null;
    return row ? { kind: row.kind, canonicalPath: row.canonical_path, projectId: row.project_id, createdAt: row.created_at } : undefined;
  }

  removeBinding(kind: ProjectBinding["kind"], canonicalPath: string, projectId: string): boolean {
    return this.write(() => {
      const result = this.db.run(
        "DELETE FROM project_bindings WHERE kind = ? AND canonical_path = ? AND project_id = ?",
        [kind, canonicalPath, projectId],
      );
      return result.changes > 0;
    });
  }

  // --------------------------------------------------------------- memories

  private insertMemoryRow(record: MemoryRecord): void {
    this.db.run(
      `INSERT INTO memories (
        id, project_id, revision, kind, title, body, status, verification,
        path_prefix, branch, pinned, tags_json, evidence_json,
        created_at, updated_at, last_verified_at, review_after, expires_at,
        superseded_by, content_hash
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      [
        record.id, record.projectId, record.revision, record.kind, record.title, record.body,
        record.status, record.verification, record.pathPrefix ?? null, record.branch ?? null,
        record.pinned ? 1 : 0, JSON.stringify(record.tags), JSON.stringify(record.evidence),
        record.createdAt, record.updatedAt, record.lastVerifiedAt ?? null,
        record.reviewAfter ?? null, record.expiresAt ?? null, record.supersededBy ?? null,
        record.contentHash,
      ],
    );
  }

  private replaceMemoryRow(record: MemoryRecord): void {
    this.db.run(
      `UPDATE memories SET
        revision = ?, kind = ?, title = ?, body = ?, status = ?, verification = ?,
        path_prefix = ?, branch = ?, pinned = ?, tags_json = ?, evidence_json = ?,
        updated_at = ?, last_verified_at = ?, review_after = ?, expires_at = ?,
        superseded_by = ?, content_hash = ?
       WHERE id = ? AND project_id = ?`,
      [
        record.revision, record.kind, record.title, record.body, record.status, record.verification,
        record.pathPrefix ?? null, record.branch ?? null, record.pinned ? 1 : 0,
        JSON.stringify(record.tags), JSON.stringify(record.evidence), record.updatedAt,
        record.lastVerifiedAt ?? null, record.reviewAfter ?? null, record.expiresAt ?? null,
        record.supersededBy ?? null, record.contentHash, record.id, record.projectId,
      ],
    );
  }

  private insertRevision(record: MemoryRecord, actor: Actor, reason: string, createdAt: string): void {
    this.db.run(
      "INSERT INTO memory_revisions (memory_id, revision, snapshot_json, actor_json, reason, created_at) VALUES (?, ?, ?, ?, ?, ?)",
      [record.id, record.revision, JSON.stringify(record), JSON.stringify(actor), reason, createdAt],
    );
  }

  /**
   * Idempotency inside the caller's transaction: replay a matching prior result
   * or record the new one. The request ID — not fuzzy similarity — is what
   * makes a retry safe.
   */
  private replayOrRemember<T>(
    projectId: string,
    request: WriteRequestRef | undefined,
    at: string,
    produce: () => T,
  ): { result: T; replayed: boolean } {
    if (!request) return { result: produce(), replayed: false };
    const prior = this.db.query(
      "SELECT input_hash, result_json FROM write_requests WHERE project_id = ? AND request_id = ?",
    ).get(projectId, request.id) as { input_hash: string; result_json: string } | null;
    if (prior) {
      if (prior.input_hash !== request.hash) {
        throw new ProjectMemoryError(
          "REQUEST_CONFLICT",
          `request id '${request.id}' was already used with a different payload`,
          3,
        );
      }
      return { result: JSON.parse(prior.result_json) as T, replayed: true };
    }
    const result = produce();
    this.db.run(
      "INSERT INTO write_requests (project_id, request_id, input_hash, result_json, created_at) VALUES (?, ?, ?, ?, ?)",
      [projectId, request.id, request.hash, JSON.stringify(result), at],
    );
    return { result, replayed: false };
  }

  /** Create a record and revision 1 atomically. */
  createMemory(
    record: MemoryRecord,
    actor: Actor,
    reason: string,
    request?: WriteRequestRef,
  ): { result: MemoryRecord; replayed: boolean } {
    return this.write(() => this.replayOrRemember(record.projectId, request, record.createdAt, () => {
      this.insertMemoryRow(record);
      this.insertRevision(record, actor, reason, record.createdAt);
      return record;
    }));
  }

  /** Apply a full record replacement and append its revision snapshot. */
  replaceMemory(
    record: MemoryRecord,
    actor: Actor,
    reason: string,
    at: string,
    request?: WriteRequestRef,
  ): { result: MemoryRecord; replayed: boolean } {
    return this.write(() => this.replayOrRemember(record.projectId, request, at, () => {
      this.replaceMemoryRow(record);
      this.insertRevision(record, actor, reason, at);
      return record;
    }));
  }

  /** Supersede an old record and create its replacement atomically. */
  supersedeMemory(
    oldRecord: MemoryRecord,
    replacement: MemoryRecord,
    actor: Actor,
    reason: string,
    at: string,
    request?: WriteRequestRef,
  ): { result: MemoryRecord; replayed: boolean } {
    return this.write(() => this.replayOrRemember(replacement.projectId, request, at, () => {
      // Insert the replacement first: the old record's superseded_by foreign
      // key has to resolve, and the whole sequence stays inside one transaction.
      this.insertMemoryRow(replacement);
      this.insertRevision(replacement, actor, reason, at);
      this.replaceMemoryRow(oldRecord);
      this.insertRevision(oldRecord, actor, reason, at);
      return replacement;
    }));
  }

  getMemory(projectId: string, id: string): MemoryRecord | undefined {
    const row = this.db.query("SELECT * FROM memories WHERE id = ? AND project_id = ?").get(id, projectId) as RawMemoryRow | null;
    return row ? toRecord(row) : undefined;
  }

  listMemories(projectId: string, options: { status?: MemoryStatus[]; limit?: number } = {}): MemoryRecord[] {
    const statuses = options.status ?? ["active"];
    const placeholders = statuses.map(() => "?").join(", ");
    const sql = `SELECT * FROM memories WHERE project_id = ? AND status IN (${placeholders})
                 ORDER BY pinned DESC, updated_at DESC, id LIMIT ?`;
    const rows = this.db.query(sql).all(projectId, ...statuses, options.limit ?? 1_000) as RawMemoryRow[];
    return rows.map(toRecord);
  }

  /** Every record for a project, any status — used by export. */
  allMemories(projectId: string): MemoryRecord[] {
    const rows = this.db.query("SELECT * FROM memories WHERE project_id = ? ORDER BY created_at, id")
      .all(projectId) as RawMemoryRow[];
    return rows.map(toRecord);
  }

  revisions(memoryId: string): RevisionRecord[] {
    return (this.db.query("SELECT memory_id, revision, snapshot_json, actor_json, reason, created_at FROM memory_revisions WHERE memory_id = ? ORDER BY revision")
      .all(memoryId) as Array<{ memory_id: string; revision: number; snapshot_json: string; actor_json: string; reason: string; created_at: string }>)
      .map((row) => ({
        memoryId: row.memory_id,
        revision: row.revision,
        snapshot: JSON.parse(row.snapshot_json) as MemoryRecord,
        actor: JSON.parse(row.actor_json) as Actor,
        reason: row.reason,
        createdAt: row.created_at,
      }));
  }

  /** Incoming supersession links pointing at this record. */
  supersededByCount(memoryId: string): number {
    return Number((this.db.query("SELECT COUNT(*) AS n FROM memories WHERE superseded_by = ?").get(memoryId) as { n: number }).n);
  }

  /** Irreversible removal of a record and its history. */
  purgeMemory(projectId: string, id: string): { deletedRevisions: number } {
    return this.write(() => {
      const incoming = this.supersededByCount(id);
      if (incoming > 0) {
        throw new ProjectMemoryError(
          "SUPERSESSION_CONFLICT",
          `${incoming} record(s) still point at this memory; supersede or purge those first`,
          3,
        );
      }
      const revisions = this.db.run("DELETE FROM memory_revisions WHERE memory_id = ?", [id]);
      this.db.run("DELETE FROM import_mappings WHERE project_id = ? AND memory_id = ?", [projectId, id]);
      this.db.run("DELETE FROM write_requests WHERE project_id = ? AND result_json LIKE ?", [projectId, `%${id}%`]);
      const removed = this.db.run("DELETE FROM memories WHERE id = ? AND project_id = ?", [id, projectId]);
      if (removed.changes === 0) throw new ProjectMemoryError("MEMORY_NOT_FOUND", `no memory '${id}' in this project`, 4);
      return { deletedRevisions: revisions.changes };
    });
  }

  // ------------------------------------------------------------ idempotency

  getWriteRequest(projectId: string, requestId: string): { inputHash: string; result: unknown } | undefined {
    const row = this.db.query(
      "SELECT input_hash, result_json FROM write_requests WHERE project_id = ? AND request_id = ?",
    ).get(projectId, requestId) as { input_hash: string; result_json: string } | null;
    return row ? { inputHash: row.input_hash, result: JSON.parse(row.result_json) as unknown } : undefined;
  }

  putWriteRequest(projectId: string, requestId: string, inputHash: string, result: unknown, at: string): void {
    this.write(() => {
      this.db.run(
        "INSERT INTO write_requests (project_id, request_id, input_hash, result_json, created_at) VALUES (?, ?, ?, ?, ?)",
        [projectId, requestId, inputHash, JSON.stringify(result), at],
      );
    });
  }

  // -------------------------------------------------------------- transfers

  /** Batch insert used by import; one transaction for the whole batch. */
  insertImported(
    records: MemoryRecord[],
    mappings: Array<{ projectId: string; sourceKind: string; sourceKey: string; sourceHash: string; memoryId: string }>,
  ): void {
    this.write(() => {
      // Insert every row first, then wire supersession links: a snapshot may
      // list the old record before the replacement it points at, and the
      // foreign key has to resolve by the end of the transaction.
      for (const record of records) {
        this.insertMemoryRow({ ...record, supersededBy: undefined });
      }
      for (const record of records) {
        if (record.supersededBy) {
          this.db.run("UPDATE memories SET superseded_by = ? WHERE id = ?", [record.supersededBy, record.id]);
        }
        this.insertRevision(record, { harness: "manual" }, "import", record.createdAt);
      }
      for (const mapping of mappings) {
        this.db.run(
          "INSERT INTO import_mappings (project_id, source_kind, source_key, source_hash, memory_id) VALUES (?, ?, ?, ?, ?)",
          [mapping.projectId, mapping.sourceKind, mapping.sourceKey, mapping.sourceHash, mapping.memoryId],
        );
      }
    });
  }

  findImportMapping(projectId: string, sourceKind: string, sourceKey: string): { sourceHash: string; memoryId: string } | undefined {
    const row = this.db.query(
      "SELECT source_hash, memory_id FROM import_mappings WHERE project_id = ? AND source_kind = ? AND source_key = ? ORDER BY source_hash DESC",
    ).get(projectId, sourceKind, sourceKey) as { source_hash: string; memory_id: string } | null;
    return row ? { sourceHash: row.source_hash, memoryId: row.memory_id } : undefined;
  }

  deleteImportMapping(projectId: string, sourceKind: string, sourceKey: string): void {
    this.write(() => {
      this.db.run(
        "DELETE FROM import_mappings WHERE project_id = ? AND source_kind = ? AND source_key = ?",
        [projectId, sourceKind, sourceKey],
      );
    });
  }

  // -------------------------------------------------------------- diagnostics

  schemaVersion(): number {
    return Number((this.db.query("PRAGMA user_version").get() as { user_version: number }).user_version);
  }

  integrity(): string {
    return String((this.db.query("PRAGMA integrity_check").get() as { integrity_check: string }).integrity_check);
  }

  memoryCount(projectId: string): number {
    return Number((this.db.query("SELECT COUNT(*) AS n FROM memories WHERE project_id = ?").get(projectId) as { n: number }).n);
  }

  /** Consistent snapshot via SQLite's own VACUUM INTO (WAL-safe). */
  backup(output: string): void {
    mkdirSync(dirname(output), { recursive: true });
    if (existsSync(output)) throw new ProjectMemoryError("BACKUP_EXISTS", `refusing to overwrite '${output}'`, 1);
    this.db.exec(`VACUUM INTO '${output.replace(/'/g, "''")}'`);
  }
}

/** Best-effort pre-migration copy so a failed migration never loses data. */
function snapshotBeforeMigration(db: Database, path: string, version: number, backupDir?: string): string | undefined {
  try {
    if (!existsSync(path)) return undefined;
    const dir = backupDir ?? join(dirname(path), "backups");
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const stamp = new Date().toISOString().replace(/[:.]/g, "-");
    const target = join(dir, `memory-${stamp}-schema${version - 1}.sqlite`);
    db.exec(`VACUUM INTO '${target.replace(/'/g, "''")}'`);
    return target;
  } catch {
    return undefined;
  }
}
