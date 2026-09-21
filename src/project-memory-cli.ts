/**
 * `dejavu memory project <verb>` — nested argument parsing, exit-code mapping,
 * and output. Additive: the legacy `memory list|search|show` path is untouched.
 *
 * Exit codes: 0 success, 1 operational failure, 2 invalid arguments,
 * 3 revision conflict, 4 missing project or record.
 */

import { readFile } from "node:fs/promises";
import { ProjectMemoryStore, resolveMemoryDbPath } from "./project-memory-store.ts";
import { ProjectMemoryService } from "./project-memory.ts";
import {
  DEFAULT_LIST_LIMIT,
  MAX_LIST_LIMIT,
  MAX_RECALL_BUDGET,
  ProjectMemoryError,
  type Actor,
  type Envelope,
  type Harness,
  type MemoryRecord,
  type ProjectContext,
} from "./project-memory-types.ts";
import { projectMemoryHelp } from "./project-memory-help.ts";
import {
  applyClaudeImport,
  buildSnapshot,
  importSnapshot,
  planClaudeImport,
  renderMarkdown,
  writeExport,
} from "./project-memory-transfer.ts";

const HARNESSES: Harness[] = ["claude", "codex", "pi", "opencode", "manual", "unknown"];

class UsageError extends ProjectMemoryError {
  constructor(message: string) {
    super("INVALID_ARGUMENT", message, 2);
  }
}

function pullFlag(args: string[], ...names: string[]): boolean {
  let found = false;
  for (const name of names) {
    let index: number;
    while ((index = args.indexOf(name)) >= 0) {
      args.splice(index, 1);
      found = true;
    }
  }
  return found;
}

function pullValue(args: string[], names: string[]): string | undefined {
  for (const name of names) {
    const index = args.findIndex((arg) => arg === name || arg.startsWith(`${name}=`));
    if (index < 0) continue;
    const arg = args[index]!;
    if (arg.includes("=")) {
      args.splice(index, 1);
      return arg.slice(arg.indexOf("=") + 1);
    }
    const value = args[index + 1];
    if (value === undefined || (value.startsWith("-") && value !== "-")) throw new UsageError(`${name} needs a value`);
    args.splice(index, 2);
    return value;
  }
  return undefined;
}

function integer(value: string | undefined, flag: string, fallback: number, min = 1, max = Number.MAX_SAFE_INTEGER): number {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < min || parsed > max) throw new UsageError(`${flag} needs an integer between ${min} and ${max}`);
  return parsed;
}

function required(value: string | undefined, message: string): string {
  if (value === undefined || !value.trim()) throw new UsageError(message);
  return value;
}

function rejectUnknownFlags(args: string[]): void {
  const unknown = args.find((arg) => arg.startsWith("-") && arg !== "-");
  if (unknown) throw new UsageError(`unknown flag: ${unknown}`);
}

function actorFrom(args: string[]): Actor {
  const harnessRaw = pullValue(args, ["--harness"]) ?? "manual";
  if (!HARNESSES.includes(harnessRaw as Harness)) throw new UsageError(`--harness must be one of ${HARNESSES.join(", ")}`);
  const sessionId = pullValue(args, ["--session-id"]);
  return { harness: harnessRaw as Harness, sessionId };
}

async function readInputFile(path: string): Promise<unknown> {
  let text: string;
  if (path === "-") text = await new Response(Bun.stdin.stream()).text();
  else {
    try {
      text = await readFile(path, "utf8");
    } catch (error) {
      throw new ProjectMemoryError("INPUT_UNREADABLE", `cannot read '${path}': ${error instanceof Error ? error.message : String(error)}`, 2);
    }
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    throw new ProjectMemoryError("INPUT_INVALID_JSON", `'${path}' is not valid JSON: ${error instanceof Error ? error.message : String(error)}`, 2);
  }
}

interface EmitOptions {
  json: boolean;
  quiet: boolean;
}

/** Readable non-JSON output: scalars inline, arrays as bullets, objects as keys. */
function renderHuman(value: unknown, indent = ""): string {
  if (value === null || value === undefined) return `${indent}-`;
  if (typeof value === "string") return value.split("\n").map((line) => indent + line).join("\n");
  if (typeof value === "number" || typeof value === "boolean") return `${indent}${String(value)}`;
  if (Array.isArray(value)) {
    if (value.length === 0) return `${indent}(none)`;
    return value.map((item) => {
      const rendered = renderHuman(item);
      const [first, ...rest] = rendered.split("\n");
      const tail = rest.map((line) => `  ${line}`).join("\n");
      return `${indent}- ${first}${tail ? `\n${tail}` : ""}`;
    }).join("\n");
  }
  const entries = Object.entries(value as Record<string, unknown>).filter(([, item]) => item !== undefined);
  if (entries.length === 0) return `${indent}(empty)`;
  return entries.map(([key, item]) => {
    const rendered = renderHuman(item);
    if (rendered.includes("\n")) return `${indent}${key}:\n${rendered.split("\n").map((line) => `${indent}  ${line}`).join("\n")}`;
    return `${indent}${key}: ${rendered.trim()}`;
  }).join("\n");
}

function emit<T>(options: EmitOptions, data: T, project?: { id: string; name: string }, diagnostics: string[] = []): number {
  const envelope: Envelope<T> = { version: 1, ok: true, data, diagnostics };
  if (project) envelope.project = project;
  if (options.json) console.log(JSON.stringify(envelope, null, 2));
  else if (typeof data === "string") console.log(data);
  else console.log(renderHuman(data));
  for (const diagnostic of diagnostics) if (!options.quiet && !options.json) console.error(`· ${diagnostic}`);
  return 0;
}

function fail(error: unknown, options: EmitOptions): number {
  const known = error instanceof ProjectMemoryError
    ? error
    : new ProjectMemoryError("OPERATIONAL", error instanceof Error ? error.message : String(error), 1);
  if (options.json) {
    const envelope: Envelope<never> = {
      version: 1,
      ok: false,
      error: { code: known.code, message: known.message },
      diagnostics: [],
    };
    console.log(JSON.stringify(envelope, null, 2));
  } else {
    console.error(`✗ ${known.message}`);
  }
  return known.exitCode;
}

function describeRecord(record: MemoryRecord) {
  return {
    id: record.id,
    revision: record.revision,
    kind: record.kind,
    title: record.title,
    status: record.status,
    verification: record.verification,
    pathPrefix: record.pathPrefix ?? null,
    branch: record.branch ?? null,
    pinned: record.pinned,
    tags: record.tags,
    updatedAt: record.updatedAt,
    expiresAt: record.expiresAt ?? null,
    supersededBy: record.supersededBy ?? null,
  };
}

function describeContext(context: ProjectContext) {
  return {
    projectId: context.projectId,
    cwd: context.cwd,
    root: context.root,
    relativePath: context.relativePath ?? null,
    branch: context.branch ?? null,
  };
}

function contextFromArgs(args: string[]): { cwd: string; projectId?: string } {
  const cwd = pullValue(args, ["--cwd"]) ?? process.cwd();
  const projectId = pullValue(args, ["--project-id"]);
  return { cwd, projectId };
}

/** Resolve a context for a read command, tolerating a missing store. */
function missingStore(projectId: string | undefined, options: EmitOptions): number {
  return fail(
    new ProjectMemoryError(
      "PROJECT_NOT_INITIALIZED",
      projectId
        ? `no project '${projectId}' is initialized (memory store is empty or missing)`
        : "no project memory store exists yet; run 'dejavu memory project init --cwd <path>'",
      4,
    ),
    options,
  );
}

export async function runProjectMemory(args: string[], options: EmitOptions): Promise<number> {
  const verb = args.shift();
  if (!verb || verb === "help" || verb === "-h" || verb === "--help") {
    console.log(projectMemoryHelp());
    return 0;
  }
  const path = resolveMemoryDbPath();

  const writeStore = () => ProjectMemoryStore.open({ path });
  const readStore = () => ProjectMemoryStore.openRead({ path });

  try {
    switch (verb) {
      case "init": {
        const name = pullValue(args, ["--name"]);
        const folder = pullFlag(args, "--folder");
        const cwd = pullValue(args, ["--cwd"]) ?? process.cwd();
        rejectUnknownFlags(args);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const result = await service.init({ cwd, name, folder });
          return emit(options, {
            project: result.project,
            binding: result.binding,
            created: result.created,
          }, { id: result.project.id, name: result.project.name }, [
            result.created ? `initialized project ${result.project.id}` : "project already initialized; returning the existing identity",
          ]);
        } finally {
          store.close();
        }
      }

      case "resolve": {
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          return emit(options, {
            matched: resolved.matched.kind === "git_common_dir" ? "git worktree (shared)" : "explicit folder",
            binding: resolved.matched,
            worktrees: resolved.worktrees,
            context: describeContext(resolved.context),
          }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "projects": {
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return emit(options, { projects: [] }, undefined, ["memory store does not exist yet"]);
        try {
          const service = new ProjectMemoryService({ store });
          return emit(options, { projects: service.projects() });
        } finally {
          store.close();
        }
      }

      case "bind": {
        const projectId = required(pullValue(args, ["--project-id"]), "bind needs --project-id");
        const cwd = required(pullValue(args, ["--cwd"]), "bind needs --cwd");
        rejectUnknownFlags(args);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const binding = await service.bind(projectId, cwd);
          return emit(options, { binding }, { id: projectId, name: store.getProject(projectId)?.name ?? projectId }, [
            `bound ${binding.kind} ${binding.canonicalPath} -> ${projectId}`,
          ]);
        } finally {
          store.close();
        }
      }

      case "unbind": {
        const projectId = required(pullValue(args, ["--project-id"]), "unbind needs --project-id");
        const bindingPath = required(pullValue(args, ["--binding"]), "unbind needs --binding PATH");
        rejectUnknownFlags(args);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const binding = await service.unbind(projectId, bindingPath);
          return emit(options, { removed: binding }, { id: projectId, name: store.getProject(projectId)?.name ?? projectId }, [
            "binding removed; memories were not deleted",
          ]);
        } finally {
          store.close();
        }
      }

      case "doctor": {
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) {
          return emit(options, {
            path,
            exists: false,
            schemaVersion: null,
            integrity: null,
            projects: 0,
            memories: 0,
            bindings: 0,
            stale: [],
          }, undefined, ["memory store does not exist yet"]);
        }
        try {
          const service = new ProjectMemoryService({ store });
          return emit(options, { exists: true, ...service.doctor() });
        } finally {
          store.close();
        }
      }

      case "add": {
        const file = required(pullValue(args, ["--file"]), "add needs --file INPUT.json (or --file - for stdin)");
        const requestId = pullValue(args, ["--request-id"]);
        const { cwd, projectId } = contextFromArgs(args);
        const actor = actorFrom(args);
        rejectUnknownFlags(args);
        const payload = await readInputFile(file);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const result = service.add(resolved.context, payload, actor, requestId);
          return emit(options, { ...describeRecord(result.record), replayed: result.replayed }, { id: resolved.project.id, name: resolved.project.name }, [
            result.replayed ? `request '${requestId}' replayed an earlier result` : `created ${result.record.id} at revision ${result.record.revision}`,
          ]);
        } finally {
          store.close();
        }
      }

      case "get": {
        const id = required(args.shift(), "get needs a memory id");
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const record = service.get(resolved.context, id);
          return emit(options, record, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "list": {
        const includeInactive = pullFlag(args, "--include-inactive");
        const limit = integer(pullValue(args, ["--limit"]), "--limit", DEFAULT_LIST_LIMIT, 1, MAX_LIST_LIMIT);
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const records = service.list(resolved.context, { includeInactive, limit });
          return emit(options, { memories: records.map(describeRecord), count: records.length }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "search": {
        const includeInactive = pullFlag(args, "--include-inactive");
        const allProjects = pullFlag(args, "--all-projects");
        const limit = integer(pullValue(args, ["--limit"]), "--limit", DEFAULT_LIST_LIMIT, 1, MAX_LIST_LIMIT);
        const snippets = integer(pullValue(args, ["--snippets"]), "--snippets", 3, 1);
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const phrase = args.join(" ").trim();
        if (!phrase) throw new UsageError("search needs one token or exact phrase");
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const results = service.search(resolved.context, phrase, { includeInactive, limit, snippets, allProjects });
          return emit(options, {
            matches: results.map((result) => ({
              ...describeRecord(result.record),
              project: allProjects ? { id: result.projectId, name: result.projectName } : undefined,
              count: result.count,
              snippets: result.snippets,
            })),
          }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "recall": {
        const query = pullValue(args, ["--query"]);
        const targetPath = pullValue(args, ["--path"]);
        const includeUnverified = pullFlag(args, "--include-unverified");
        const budgetChars = integer(pullValue(args, ["--budget-chars"]), "--budget-chars", 6_000, 1, MAX_RECALL_BUDGET);
        const limit = integer(pullValue(args, ["--limit"]), "--limit", 12, 1, 200);
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const outcome = service.recall(resolved.context, resolved.project.name, { query, targetPath, budgetChars, limit, includeUnverified });
          return emit(options, {
            context: outcome.context,
            selected: outcome.selected,
            omitted: outcome.omitted,
            budgetChars: outcome.budgetChars,
            usedChars: outcome.usedChars,
          }, { id: resolved.project.id, name: resolved.project.name }, outcome.diagnostics);
        } finally {
          store.close();
        }
      }

      case "update": {
        const id = required(args.shift(), "update needs a memory id");
        const ifRevision = integer(pullValue(args, ["--if-revision"]), "--if-revision", Number.NaN);
        if (!Number.isInteger(ifRevision)) throw new UsageError("update needs --if-revision N");
        const file = required(pullValue(args, ["--file"]), "update needs --file INPUT.json");
        const requestId = pullValue(args, ["--request-id"]);
        const { cwd, projectId } = contextFromArgs(args);
        const actor = actorFrom(args);
        rejectUnknownFlags(args);
        const payload = await readInputFile(file);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const result = service.update(resolved.context, id, ifRevision, payload, actor, requestId);
          return emit(options, { ...describeRecord(result.record), replayed: result.replayed }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "archive": {
        const id = required(args.shift(), "archive needs a memory id");
        const ifRevision = integer(pullValue(args, ["--if-revision"]), "--if-revision", Number.NaN);
        if (!Number.isInteger(ifRevision)) throw new UsageError("archive needs --if-revision N");
        const reason = required(pullValue(args, ["--reason"]), "archive needs --reason TEXT");
        const { cwd, projectId } = contextFromArgs(args);
        const actor = actorFrom(args);
        rejectUnknownFlags(args);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const record = service.archive(resolved.context, id, ifRevision, reason, actor);
          return emit(options, { ...describeRecord(record), reason }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "supersede": {
        const id = required(args.shift(), "supersede needs a memory id");
        const ifRevision = integer(pullValue(args, ["--if-revision"]), "--if-revision", Number.NaN);
        if (!Number.isInteger(ifRevision)) throw new UsageError("supersede needs --if-revision N");
        const file = required(pullValue(args, ["--file"]), "supersede needs --file INPUT.json");
        const requestId = pullValue(args, ["--request-id"]);
        const { cwd, projectId } = contextFromArgs(args);
        const actor = actorFrom(args);
        rejectUnknownFlags(args);
        const payload = await readInputFile(file);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const result = service.supersede(resolved.context, id, ifRevision, payload, actor, requestId);
          return emit(options, {
            created: describeRecord(result.record),
            superseded: describeRecord(result.superseded),
          }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "history": {
        const id = required(args.shift(), "history needs a memory id");
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const revisions = service.history(resolved.context, id);
          return emit(options, { revisions }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "purge": {
        const id = required(args.shift(), "purge needs a memory id");
        const ifRevision = integer(pullValue(args, ["--if-revision"]), "--if-revision", Number.NaN);
        if (!Number.isInteger(ifRevision)) throw new UsageError("purge needs --if-revision N");
        const reason = required(pullValue(args, ["--reason"]), "purge needs --reason TEXT");
        const { cwd, projectId } = contextFromArgs(args);
        const actor = actorFrom(args);
        void actor;
        rejectUnknownFlags(args);
        const store = writeStore();
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const result = service.purge(resolved.context, id, ifRevision, reason);
          return emit(options, { purged: result.purged.id, deletedRevisions: result.deletedRevisions, reason }, { id: resolved.project.id, name: resolved.project.name }, [
            "purge does not erase copies in backups, exports, or prior snapshots; rotate any secret that was stored",
          ]);
        } finally {
          store.close();
        }
      }

      case "export": {
        const format = (pullValue(args, ["--format"]) ?? "json").toLowerCase();
        if (format !== "json" && format !== "markdown") throw new UsageError("--format must be json or markdown");
        const output = required(pullValue(args, ["--output"]), "export needs --output PATH");
        const includeLocalMetadata = pullFlag(args, "--include-local-metadata");
        const { cwd, projectId } = contextFromArgs(args);
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(projectId, options);
        try {
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const snapshot = buildSnapshot(store, resolved.context.projectId, { includeLocalMetadata });
          await writeExport(output, format === "json" ? JSON.stringify(snapshot, null, 2) : renderMarkdown(snapshot));
          return emit(options, {
            output,
            format,
            records: snapshot.records.length,
            includeLocalMetadata,
          }, { id: resolved.project.id, name: resolved.project.name });
        } finally {
          store.close();
        }
      }

      case "import": {
        const file = required(pullValue(args, ["--file"]), "import needs --file PATH");
        const dryRun = pullFlag(args, "--dry-run");
        const projectId = pullValue(args, ["--project-id"]);
        const createProjectName = pullValue(args, ["--create-project"]);
        rejectUnknownFlags(args);
        const raw = await readFile(file, "utf8");
        const store = writeStore();
        try {
          const result = importSnapshot(store, raw, { projectId, createProjectName, dryRun });
          return emit(options, { ...result, dryRun }, undefined, result.diagnostics);
        } finally {
          store.close();
        }
      }

      case "import-claude": {
        const from = required(pullValue(args, ["--from"]), "import-claude needs --from DIR");
        const projectId = required(pullValue(args, ["--project-id"]), "import-claude needs --project-id");
        const kindRaw = pullValue(args, ["--kind"]) ?? "convention";
        const dryRun = pullFlag(args, "--dry-run");
        const cwd = pullValue(args, ["--cwd"]) ?? process.cwd();
        rejectUnknownFlags(args);
        const store = writeStore();
        try {
          const planned = await planClaudeImport(store, from, projectId, kindRaw as MemoryRecord["kind"]);
          if (dryRun) {
            return emit(options, { source: planned.plan.source, entries: planned.plan.entries, dryRun: true }, { id: projectId, name: store.getProject(projectId)?.name ?? projectId }, planned.plan.diagnostics);
          }
          const service = new ProjectMemoryService({ store });
          const resolved = await service.resolve(cwd, projectId);
          const applied = applyClaudeImport(store, service, planned, resolved.context);
          return emit(options, {
            source: planned.plan.source,
            imported: applied.imported,
            entries: planned.plan.entries,
            dryRun: false,
          }, { id: resolved.project.id, name: resolved.project.name }, [
            ...planned.plan.diagnostics,
            "imported records are unverified and excluded from automatic recall until confirmed",
          ]);
        } finally {
          store.close();
        }
      }

      case "backup": {
        const output = required(pullValue(args, ["--output"]), "backup needs --output PATH");
        rejectUnknownFlags(args);
        const store = readStore();
        if (!store) return missingStore(undefined, options);
        try {
          store.backup(output);
          return emit(options, { output, integrity: null });
        } finally {
          store.close();
        }
      }

      default:
        throw new UsageError(`unknown memory project command '${verb}' (run 'dejavu memory project help')`);
    }
  } catch (error) {
    return fail(error, options);
  }
}
