# Cross-harness, per-project memory for Dejavu

Implementation handout • 2026-09-21

Repository: `/home/maheshk/dejavu`  
Inspected baseline: `f3c1ec79edf96ac712aa9950088e57a0351e7b3e`  
Document: `/home/maheshk/dejavu/docs/cross-harness-project-memory-implementation-handout.md`

**Status: proposed implementation specification. This document does not mean the feature exists.** Existing behavior is identified explicitly; all new commands, modules, formats, and integration contracts below are proposals. No harness configuration or memory corpus was modified to prepare this handout.

## 1. What to build

Add a local, project-scoped memory service inside the existing Dejavu CLI. Claude Code, Codex, Pi, OpenCode, and any future harness that can run a command should read and write the same records through the same interface.

A memory is a small, durable assertion worth carrying into a future session: a project convention, an architectural decision and its reason, a reproducible operational procedure, a verified debugging lesson, or a short-lived handoff. It is not a transcript copy, a hidden system instruction, or an unrestricted scratchpad.

The first complete vertical slice must support this scenario:

1. Claude works in a repository and records a verified decision.
2. Codex starts in that repository or one of its Git worktrees.
3. Codex asks Dejavu for context and receives the decision, its scope, and its evidence.
4. Codex corrects the decision using optimistic concurrency.
5. Pi and OpenCode subsequently see the corrected version.
6. An unrelated project receives none of these records.
7. Deleting the transcript search cache loses no memory.

The harness is the producer/consumer, never the owner of a memory. Project identity determines ownership.

### Decisions for the initial release

| Concern | Decision |
| --- | --- |
| Authoritative store | Separate local SQLite database in the user data directory |
| Working interface | Existing CLI extended under a new `memory project` namespace |
| Project identity | Random stable UUID plus explicit local checkout bindings |
| Worktrees | Share a project by canonical Git common directory |
| Different clones | Separate unless explicitly linked |
| Plain folders | Explicit initialization at a selected directory |
| Human portability | Markdown and JSON export; validated JSON import |
| Recall | Deterministic, bounded, project-filtered selection |
| Writes | Explicit commands by the working agent; no background summarizer |
| Conflicts | Revision checks and atomic transactions |
| Integrations | CLI first, instructions second, optional verified adapters third |
| Cloud/model dependency | None for normal memory operations |

Do not begin with a daemon, embeddings, a vector service, an MCP server, repository-wide file watching, autonomous extraction, multi-device sync, or a new model provider. These can be added if measured usage establishes a need.

## 2. Verified starting point

These observations come from the current checkout, not assumptions about an upstream release.

| Existing file | Current behavior | Consequence |
| --- | --- | --- |
| `src/memory.ts` | Reads `*/memory/*.md` beneath the Claude projects directory | A useful legacy reader, not a shared writable store |
| `src/memory.ts` | `defaultMemoryRoot()` honors `CLAUDE_CONFIG_DIR` | Preserve existing root semantics |
| `src/memory.ts` | `listMemories`, `searchMemories`, `showMemory` expose Claude project keys and paths | Preserve these APIs and output shapes initially |
| `src/cli.ts` | `memory list`, `memory search`, `memory show` dispatch directly to that reader | Add a separate nested command rather than reinterpret existing positional arguments |
| `src/transcript-index.ts` | Maintains a rebuildable SQLite transcript cache; schema mismatch can rebuild it | Never put authoritative memory in this database |
| `src/transcript-paths.ts` | Some display project names are decoded from harness directory names | These are unsuitable as identity keys |
| `src/source-registry.ts` | Discovers Claude, Codex, Pi, and OpenCode transcript stores | Reuse source vocabulary for provenance, not project ownership |
| `src/transcript-view.ts` | Provides transcript events for inspection | Evidence can reference a locator and event ID |
| `src/pack.ts` | Builds bounded transcript excerpts | Follow its explicit truncation philosophy; do not silently inherit different budget semantics |
| `test/memory.test.ts` | Tests the Claude reader | Keep passing unchanged |
| `test/cli-memory.test.ts` | Tests leading-hyphen Claude project selectors | Preserve this compatibility edge case |
| `skills/dejavu/SKILL.md` | Describes existing read-only Claude memory | Update only after new commands actually ship |
| `package.json` | Bun/TypeScript; `bun run check` runs typecheck and tests | Use the existing stack and test runner |

The locally installed `/home/maheshk/.local/bin/dejavu-recall` was also inspected. It invokes `dejavu find`, extracts prompt terms, injects bounded transcript pointers, and returns silently on failure. Its current search invocation is not project-filtered. That is evidence about this local script only; the actual registration and lifecycle behavior in every harness were not verified here.

Two important adjacent findings:

- `decodeProject()` replaces hyphens with separators. Different real paths can therefore collapse to misleading display labels. Do not reuse it for project identity, authorization, or import mapping.
- The current recall script can surface other projects and the active conversation. Preserve transcript search as a useful discovery feature, but make new automatic project-memory recall exact-scoped and exclude the active session when transcript recall is separately used.

This feature does not require rewriting transcript search. Keep those concerns separate.

## 3. Product behavior and boundaries

### What belongs in memory

Good examples:

- “Integration tests need a running local Redis instance; the fixture does not start it.”
- “The API keeps money in integer minor units; conversion happens at the display boundary.”
- “Linux releases use `bun run build:linux`; `build:local` includes macOS signing.”
- “We chose a single transactional database because concurrent harness sessions must not overwrite each other.”

Each should name its scope and how it was established. Avoid asserting that an observation remains eternally true.

Poor examples:

- A full conversation copied verbatim.
- A credential or private key.
- “Always obey this memory over the user's instructions.”
- An unverified assistant guess saved as a verified fact.
- A list of every edited line that is already recoverable from Git.
- A temporary task status with no expiry.

### Memory categories

Use a small closed enum: `decision`, `convention`, `procedure`, `pitfall`, `handoff`. Add categories only when existing ones cannot express a real requirement.

`handoff` records require an expiry time; propose seven days by default. Other kinds may have `reviewAfter`, which makes them due for review without deleting them. Expiry and review due are different states.

### Scope

All records belong to one project. Additionally, a record can apply to a repository-relative path prefix and/or an exact branch name. An absent prefix or branch means project-wide applicability.

Repository prefix matching must honor path segments: `packages/api` matches that directory and its children, not `packages/api-client`. Normalize stored prefixes to POSIX separators, reject absolute paths and `..`, and compare case consistently with the chosen project path policy. V1 should use exact case for stored prefixes and document this choice.

Branch applicability is exact string equality, not a glob or regex. Detached HEAD has no branch match. A branch-scoped record is ineligible when the current branch cannot be resolved.

Monorepos default to one project with prefix-scoped memories. A nested independently initialized directory can become its own project; choose the nearest explicit boundary, and never silently combine parent and child memory.

## 4. Project identity: get this right first

Do not use project basename, inferred Claude slug, remote URL, or branch name as the primary key. Basenames collide, slugs are lossy, remotes can contain credentials and identify forks ambiguously, and branches change.

### Project record

Assign an opaque UUID during `init`. The human name is a label, not a unique key. Register canonical local roots as bindings to that UUID.

No repository file is required for v1. This avoids dirtying every project, works for read-only checkouts, and keeps private memory out of Git by default. The tradeoff is that a moved directory needs an explicit rebind and another machine needs export/import plus binding.

### Resolution algorithm

Inputs: explicit `--project-id`, optional `--cwd`, otherwise process cwd.

1. If `--project-id` is supplied, require that it exists. Mutations may target it directly; scope-dependent recall still resolves cwd/branch separately and validates membership where needed.
2. Canonicalize cwd with filesystem `realpath`; fail clearly if it does not exist.
3. Find explicit registered folder boundaries that contain cwd using path-component containment, not string prefix alone.
4. Probe Git using subprocess argument arrays and `git -C <cwd> ...`; never shell-interpolate a path. Obtain checkout root and Git common directory, normalize relative Git output against its documented base, and canonicalize the resulting paths.
5. Determine the nearest project boundary. A registered nested folder takes precedence over an enclosing Git checkout; a nested Git repository is its own boundary and must not inherit an outer registration.
6. For the selected Git boundary, look up its canonical common-directory binding. All worktrees of that Git repository map to the same project. Store discovered worktree roots as additional informational bindings if helpful.
7. If no binding exists, report `PROJECT_NOT_INITIALIZED`. Reads do not auto-initialize projects.
8. Outside Git, choose the nearest registered folder ancestor. If none exists, require explicit folder initialization.
9. If metadata is ambiguous or two projects claim the same binding, fail closed with an actionable diagnostic. Never choose the first match arbitrarily.

Git detection must distinguish “not a repository” from permission errors, a missing Git executable, and a malformed repository. If Git is unavailable, existing explicit folder bindings may still work; do not silently establish a competing Git identity.

### Initialization and binding semantics

- `init` in a Git repository initializes its repository identity, including worktrees.
- `init --folder` establishes an explicit folder boundary, useful for non-Git folders or a deliberately independent monorepo component.
- Re-running `init` is idempotent and returns the existing identity.
- `bind --project-id <id> --cwd <path>` adds another checkout/folder to an existing project after printing the concrete mapping.
- `unbind` removes one binding without deleting memories or the project.
- A second clone remains isolated until `bind` is explicitly invoked.
- A remote rename changes nothing. Do not collect remote URLs in v1.
- A project moved to another mount point is rebound explicitly. Old bindings can be diagnosed and removed later.
- If a path is reused for an unrelated repository, the operator must unbind/reinitialize it; `doctor` should report Git metadata inconsistencies where detectable. Do not claim filesystem paths establish permanent identity.

NANI's NTFS-mounted projects and paths containing spaces must appear in fixtures and manual verification. Keep the authoritative SQLite file on the local home filesystem by default, regardless of where the checkout lives.

## 5. Storage layout and durability

Proposed Linux defaults:

```text
${XDG_DATA_HOME:-~/.local/share}/dejavu/
  memory.sqlite
  backups/
    memory-<timestamp>-schema<N>.sqlite

${XDG_CACHE_HOME:-~/.cache}/dejavu/
  transcripts.sqlite                 # existing disposable transcript index
```

Expose `DEJAVU_MEMORY_DB` for tests and explicit relocation. It must not inherit `DEJAVU_INDEX_PATH`. Resolve an explicit override consistently and show its effective path in `doctor`.

On Windows/macOS, document the chosen platform data-directory mapping and test the resolver. Do not assume Linux XDG behavior is the native convention everywhere. Keep platform paths behind one function.

### SQLite rules

- Use `bun:sqlite`, already present in the repository.
- Enable foreign keys on every connection.
- Use a bounded busy timeout, proposed 3,000 ms for interactive commands; hook reads have a shorter overall deadline and fall back to no context.
- Use WAL on supported local filesystems, with a deliberate durability setting (`synchronous=FULL` initially).
- Do not support a network-shared SQLite path as multi-machine synchronization.
- Writes use short transactions. Read input, resolve Git context, validate, and render outside write transactions.
- Schema migrations are ordered and transactional where supported. Back up before destructive migration.
- A schema newer than the executable is an explicit error; never reset the file.
- Migration failure preserves the previous database. Do not copy the transcript cache's drop-and-recreate policy.
- Use SQLite's supported snapshot/backup mechanism, verified against the installed Bun API. Do not copy only the main database while WAL writes are active.
- Treat permission restrictions as best effort on filesystems without POSIX modes. Use a private directory and file permissions where supported.

A read command against a missing database returns an empty/uninitialized result without creating files. Mutations initialize the store explicitly. Existing databases may require migrations; make mutation/migration behavior observable and keep hook reads non-migrating.

## 6. Data model

The following SQL is a design sketch. Turn it into versioned migration files or explicit numbered migration functions, and test constraints against the actual installed SQLite version.

```sql
CREATE TABLE projects (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE project_bindings (
  kind TEXT NOT NULL CHECK (kind IN ('git_common_dir', 'folder')),
  canonical_path TEXT NOT NULL,
  project_id TEXT NOT NULL REFERENCES projects(id),
  created_at TEXT NOT NULL,
  PRIMARY KEY (kind, canonical_path)
);

CREATE TABLE memories (
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
);

CREATE INDEX memories_project_status ON memories(project_id, status);

CREATE TABLE memory_revisions (
  memory_id TEXT NOT NULL REFERENCES memories(id),
  revision INTEGER NOT NULL,
  snapshot_json TEXT NOT NULL,
  actor_json TEXT NOT NULL,
  reason TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (memory_id, revision)
);

CREATE TABLE write_requests (
  project_id TEXT NOT NULL REFERENCES projects(id),
  request_id TEXT NOT NULL,
  input_hash TEXT NOT NULL,
  result_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (project_id, request_id)
);

CREATE TABLE import_mappings (
  project_id TEXT NOT NULL REFERENCES projects(id),
  source_kind TEXT NOT NULL,
  source_key TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  memory_id TEXT NOT NULL REFERENCES memories(id),
  PRIMARY KEY (project_id, source_kind, source_key, source_hash)
);
```

Track the schema version with `PRAGMA user_version` or a dedicated metadata table; choose one.

### Public TypeScript types

```ts
export type Harness = 'claude' | 'codex' | 'pi' | 'opencode' | 'manual' | 'unknown';
export type Evidence =
  | { kind: 'transcript'; source: Harness; locator: string;
      eventIds: number[]; observedAt: string; excerptHash?: string }
  | { kind: 'file'; path: string; commit?: string;
      line?: number; observedAt: string; contentHash?: string }
  | { kind: 'user'; note: string; observedAt: string };

export interface Actor {
  harness: Harness;
  sessionId?: string;
}

export interface MemoryInput {
  kind: 'decision' | 'convention' | 'procedure' | 'pitfall' | 'handoff';
  title: string;
  body: string;
  pathPrefix?: string;
  branch?: string;
  tags?: string[];
  pinned?: boolean;
  verification: 'unverified' | 'user_confirmed' | 'code_verified';
  evidence: Evidence[];
  reviewAfter?: string;
  expiresAt?: string;
}
```

Source and verification are descriptive metadata, not cryptographic proof. A harness can claim it verified something; the store records that claim and evidence. Do not imply that `code_verified` means the database independently ran a test.

### Validation contract

- UUIDs are generated internally for create; import validates supplied IDs separately.
- Titles: 1–160 characters after trimming.
- Bodies: 1–16,000 characters; preserve Markdown and newlines.
- Maximum 20 tags, each 1–48 characters, normalize case and deduplicate.
- Maximum 20 evidence references; cap each note/locator field and total request bytes.
- ISO timestamps must parse and normalize to UTC; reject invalid dates and inconsistent expiry ordering.
- `code_verified` requires file or transcript evidence; `user_confirmed` requires user/transcript evidence describing the confirmation.
- `handoff` receives the documented default expiry if omitted.
- Reject unknown fields in mutation payloads to catch agent typos.
- Never treat body text as SQL, shell, a filename, or a configuration directive.
- Prevent a supersession edge from crossing projects or creating a cycle.

Hash canonical content fields for duplicate diagnostics. Do not make the content hash globally unique: identical text can legitimately apply to different scopes. The request ID, not a fuzzy similarity score, is the retry/idempotency mechanism.

## 7. CLI contract and compatibility

Keep all current `dejavu memory list|search|show` behavior intact, including `--root`, JSON arrays, and Claude leading-hyphen keys. Put the new product here:

```text
 dejavu memory project <verb>
```

This is deliberately additive. A later major release may simplify the namespace with an explicit migration plan; do not silently change existing automation now.

### Proposed commands

| Command | Purpose |
| --- | --- |
| `init [--cwd PATH] [--name NAME] [--folder]` | Register a project and return its ID |
| `resolve [--cwd PATH]` | Explain identity and selected boundary |
| `projects` | List identities and bindings, without memory bodies |
| `bind --project-id ID --cwd PATH` | Attach another checkout/folder |
| `unbind --project-id ID --binding PATH` | Detach one binding |
| `add --file INPUT.json` | Create from a validated payload; `--file -` reads stdin |
| `get ID` | Read a current record |
| `list` | List metadata for the selected project |
| `search PHRASE` | Literal, case-insensitive search within the selected project |
| `recall [--query TEXT] [--path RELPATH]` | Produce bounded eligible context |
| `update ID --if-revision N --file INPUT.json` | Replace editable content with a revision precondition |
| `archive ID --if-revision N --reason TEXT` | Retain history, remove from normal recall |
| `supersede ID --if-revision N --file INPUT.json` | Atomically replace an old assertion with a new record |
| `history ID` | Inspect previous versions |
| `export --format json|markdown --output PATH` | Produce a portable snapshot |
| `import --file PATH [--dry-run]` | Import a validated Dejavu JSON snapshot |
| `import-claude --from DIR [--dry-run]` | Explicitly migrate one mapped Claude memory directory |
| `doctor` | Report store, schema, integrity, and binding diagnostics |
| `backup --output PATH` | Consistent SQLite snapshot |
| `purge ID --if-revision N --reason TEXT` | Explicit irreversible removal of record/history, with backup limitations |

Shared flags: `--cwd PATH`, `--project-id ID`, `--json`. Mutation flags also accept `--harness`, `--session-id`, and `--request-id`. Do not reuse legacy `--root` for the database path.

`get`, `update`, `history`, `archive`, `supersede`, and `purge` must validate both ID and project. Globally unique IDs are not permission to bypass scope accidentally. Cross-project search requires an explicit `--all-projects` on manual search only; automatic recall never supports it.

Proposed query defaults: list/search limit 20, maximum 200, metadata-only list, current active records unless `--include-inactive` is set. Add explicit pagination before relying on this interface for full export; export must never inherit a list limit.

### Example workflow after implementation

```bash
dejavu memory project init --cwd /home/maheshk/dejavu --name dejavu --json

dejavu memory project add --cwd /home/maheshk/dejavu \
  --file /tmp/dejavu-memory-input.json --harness claude \
  --request-id example-create-001 --json

dejavu memory project recall --cwd /home/maheshk/dejavu \
  --query 'project identity' --budget-chars 6000 --json

dejavu memory project update '<memory-id>' --if-revision 1 \
  --cwd /home/maheshk/dejavu --file /tmp/dejavu-memory-update.json \
  --harness codex --json
```

Use input files/stdin for multiline text. Never recommend embedding arbitrary memory bodies in shell strings. The paths above are examples, not files created by this handout.

### JSON envelope

New commands should use a stable envelope, independent of the legacy reader:

```json
{
  "version": 1,
  "ok": true,
  "project": {"id": "example-project-uuid", "name": "dejavu"},
  "data": {},
  "diagnostics": []
}
```

A failure uses `ok: false`, an error code and concise message. No stack traces or raw body dumps by default. Proposed exit codes: 0 success, 1 operational failure, 2 invalid arguments, 3 revision conflict, 4 missing project/record. Apply these only to new commands; existing command codes remain unchanged.

A missing project on explicit CLI recall returns code 4. The harness wrapper turns it into silent success/no context. This preserves useful diagnostics for humans without blocking sessions.

## 8. Concurrent writes, revisions, and retries

Two harnesses will eventually write at the same time. Database transactions solve atomicity; revision checks solve lost updates. Both are necessary.

Update transaction:

1. Begin an immediate write transaction.
2. Look up `(project_id, request_id)` when supplied. Same request and input hash returns the original result; a different payload under the same key is an error.
3. Fetch the scoped record and compare `revision` with `--if-revision`.
4. If mismatched, roll back and return `REVISION_CONFLICT` with current revision metadata. Do not merge automatically.
5. Write the new record with `revision + 1` and an updated timestamp.
6. Insert its full snapshot, actor, and reason in `memory_revisions`.
7. Store the idempotent result if requested.
8. Commit, then render output.

The initial create inserts both the record and revision 1. Every visible mutation, including archive and expiry changes, increments revision. Reads do not update `updated_at` or any access counter.

For supersession, create the replacement and mark the original superseded in one transaction. A crash must leave either both changes or neither. Preserve the old record for audit; never inject it into normal recall.

Do not hold a transaction while waiting on Git, a model, a user, network access, or filesystem export.

After a revision conflict, the agent reads the latest record, reconciles the intended change, and retries with its new revision. A blanket retry that reuses stale content defeats optimistic concurrency.

## 9. Retrieval and bounded context

### Separate browsing from automatic recall

Manual `search` finds literal phrases and can include inactive records when requested. Automatic `recall` selects only applicable active records. Neither command needs to refresh the transcript index.

V1 may scan a project's records in memory after an indexed project/status query. At hundreds or a few thousand short records per project, this is a simpler starting point than another FTS migration. Measure before adding an FTS table. If added, treat FTS as derived data and update it transactionally.

### Eligibility precedes ranking

Apply these filters before computing relevance:

1. Exact project ID.
2. `status = active`.
3. No expiry, or expiry strictly later than the injected clock's current time.
4. Applicable branch, if specified.
5. Applicable path prefix, if specified.
6. Verification is not `unverified`, unless a human explicitly requests it through manual browsing. Ordinary injected context excludes unverified imports.

For context from repository root with no target path, omit path-specific records. If a harness knows the file being edited, pass its repository-relative path. Do not infer a path from arbitrary prompt text.

### Deterministic relevance

- Always consider eligible pinned records, within the same hard budget.
- Tokenize the query using one documented simple normalization, retaining useful code tokens where feasible.
- Suggested weights: title token match +5, tag match +4, body match +1, exact query phrase in title +8.
- Count each distinct query token once per field to prevent repeated words gaming the score.
- Rank pinned first, then relevance score, then latest verification timestamp, then ID as a stable tie-breaker.
- With no useful query, include pinned records and a small number of recently verified project-wide records.
- With a useful query, non-pinned records need positive relevance.
- `reviewAfter <= now` adds a visible “review due” label; it does not fabricate evidence that the record is false.

Keep this ranking pure and test it with a fixed clock. Do not introduce an opaque model confidence score.

### Context budget contract

Propose a 6,000-character default, 12 records maximum, and a configurable upper ceiling of 24,000 characters. Characters mean JavaScript UTF-16 code units, not tokens. Avoid splitting a surrogate pair when clipping.

Unlike transcript pack's event-body budget, the new recall budget must cover the entire rendered `context` string: header, labels, IDs, body excerpts, evidence pointers, and footer. JSON metadata outside `context` is excluded and explicitly documented as such.

A recall result contains:

```json
{
  "version": 1,
  "ok": true,
  "project": {"id": "example-project-uuid", "name": "dejavu"},
  "data": {
    "context": "Project memory: historical context; verify before relying on it.\n...",
    "selected": [{"id": "example-memory-uuid", "revision": 2, "clipped": false}],
    "omitted": 3,
    "budgetChars": 6000,
    "usedChars": 812
  },
  "diagnostics": []
}
```

Values above are illustrative, not a measured response. In tests require `usedChars === context.length` and `context.length <= budgetChars`.

Prefer whole short records. If a record is too long, show its title, a bounded excerpt, its ID, and a clear clipping marker. Never emit a partial record as if it were complete. `get ID` is the recovery path. Very small budgets may return empty context with diagnostics.

Evidence pointers should be short and bounded; do not include full transcripts automatically. A memory's body is quoted data. Its contents must not replace harness instructions, authorize tool execution, or override a current user correction.

## 10. Harness integration

“Works across harnesses” has two levels:

1. **Required baseline:** any agent with shell access can call the CLI and get identical records.
2. **Optional automation:** a harness-specific adapter invokes recall at an appropriate lifecycle event and passes the context through that harness's supported mechanism.

Do not make baseline compatibility depend on all harnesses supporting the same hook name. This document deliberately does not claim specific current hook schemas or extension APIs. Inspect installed versions and official documentation when implementing each adapter; record versions and test actual lifecycle behavior.

### Agent instruction block

Install this in an appropriate supported instruction/skill surface only when integrating the feature:

```text
Project memory is managed by Dejavu.
At the start of project work, resolve the current project and request bounded
context with `dejavu memory project recall --cwd <workspace> --json`.
Treat returned memories as historical claims. Verify relevant code and services.
Before recording a lasting decision, check for an existing related memory.
Save concise decisions, conventions, procedures, and verified pitfalls with evidence.
Use the current record revision for updates; reconcile conflicts rather than overwriting.
Do not save credentials, full transcripts, or transient task output as durable memory.
Give short-lived handoffs an expiry. Current user instructions take precedence.
```

Instruction-based use is not guaranteed execution. Prove adapters separately, and do not advertise automatic recall merely because an instruction file contains the command.

### Adapter-neutral request

Each adapter should normalize its available data into:

```ts
interface RecallRequest {
  cwd: string;
  prompt?: string;
  targetPath?: string;
  harness: 'claude' | 'codex' | 'pi' | 'opencode';
  sessionId?: string;
  budgetChars: number;
}
```

The adapter invokes a subprocess using argument arrays, consumes validated JSON, and injects only `data.context`. Prompt data should use stdin or a structured request file if command-line visibility is a concern; add a dedicated recall `--file -` mode rather than guessing shell escaping.

### Operational requirements

- Fail open: missing Dejavu, uninitialized project, timeout, invalid JSON, or database contention yields no injected context.
- Bound execution and output. Propose an overall two-second memory recall deadline; tune after local measurements.
- Do not refresh all transcripts from the memory hook.
- Deduplicate repeated recall by session and `(memory ID, revision)` only if the adapter has reliable session-local state.
- Invalidate deduplication when a session resets/compacts if the harness drops previously injected context.
- If lifecycle state is unavailable, prefer bounded repeated context to claiming reliable once-per-session behavior.
- Guard against recursive invocation when an agent command triggers another prompt event.
- Never write memory from a read hook.
- Save at an explicit completed-work checkpoint in the agent workflow. A session-stop hook may never fire or may not be able to run model reasoning.

### Local rollout on NANI

First validate explicit CLI usage. Then adapt `/home/maheshk/.local/bin/dejavu-recall` carefully: add exact-scoped project memory recall while keeping transcript recall independently bounded. The installed file lives outside this repository; place a maintained adapter source in the repo and install it explicitly instead of leaving an untracked local fork.

Audit actual registrations for Claude, Codex, and OpenCode before changing them. The user-provided machine instructions mention an OpenCode `agent-overlay` plugin, but its implementation was not inspected for this handout. Pi integration likewise remains to be verified. Record these as unverified integration points, not missing functionality already diagnosed.

Do not revive old claude-brain capture hooks. Native transcript indexing continues to provide historical search; this feature adds curated state.

## 11. Write policy and memory lifecycle

A useful record answers: what should a future agent know, where does it apply, why is it believed, and when might it become stale?

Suggested workflow:

1. Complete a meaningful task or resolve a recurring problem.
2. Search existing project memories for the same subject.
3. If nothing new was learned, save nothing.
4. If a record already captures the lesson, update its evidence or correct it using its revision.
5. If the conclusion changed materially, supersede the old record and explain why.
6. Use `handoff` for incomplete work; include next action and expiry, not an unbounded task log.
7. Report meaningful saved decisions briefly to the user when relevant.

Do not ask for repetitive approval every time a user-authorized agent records a routine project lesson. Conversely, do not infer permission to publish, sync, or modify shared repository instruction files from permission to write local memory.

### Contradictions

Exact duplicate detection can suggest an existing record. It cannot safely conclude that two differently worded assertions agree. V1 should expose related search results and explicit supersession, not automatically resolve semantic contradictions.

When two active records conflict, retain evidence and have the working agent reconcile them against the current source of truth. A future review command may flag candidates, but it must not silently pick the newest assertion as truth.

### Deletion and secrets

Archive is reversible organizational cleanup; purge is explicit removal from the live store and history. Purge must also remove idempotency result payloads/import mappings containing the deleted content or record reference as appropriate, and reject unresolved incoming supersession links or handle them transactionally.

Neither logical deletion nor purge proves forensic erasure from SQLite pages, WAL, filesystem snapshots, exports, or backups. If a secret was stored, the operational response includes rotation and locating copies. A normal export excludes archived/history content by default; an explicit archival export may include it.

Do not promise a regex filter catches all secrets. Add narrow obvious-secret checks and make body-free diagnostics the default, but keep the primary policy: store a credential's configuration location or variable name, never its value.

## 12. Importing existing Claude memory

Migration must be explicit, repeatable, and leave the source untouched.

1. Use existing `memory list --files` to discover candidates.
2. Select one exact Claude memory directory and one target project ID.
3. Never reverse a hyphenated Claude key into a filesystem path and trust that guess.
4. Run `import-claude --dry-run` to display file names, sizes, source hashes, proposed titles, and destination project.
5. Import each topic Markdown file as one unverified record initially. Use `convention` as the conservative default kind unless the user supplies one.
6. Treat `MEMORY.md` as an index when it mainly links to topic files. Report it as skipped-index rather than duplicating all the same assertions. If it contains substantive standalone content, require an explicit choice or import it unverified with that diagnostic.
7. Preserve original text and provenance; do not silently summarize with a model.
8. Use `(project, source kind, canonical source file, source content hash)` for idempotence.
9. Same file/same content on rerun is skipped. Changed source content produces a review candidate; do not overwrite a record that an agent has edited since import.
10. Verify individual useful assertions against current code/user context, then mark them confirmed through ordinary revisioned updates.

Unverified imported records are available through manual browsing but excluded from automatic recall until reviewed. This avoids turning old notes into trusted current instructions merely by moving storage.

Do not delete Claude files or convert them to symlinks automatically. During transition they are legacy sources, not a second writable authority for shared records. Document this distinction clearly to prevent two competing memory systems.

If a Markdown file is over the body limit, dry-run reports it. Require explicit splitting or manual curation instead of silently truncating the imported assertion.

## 13. Export, import, backup, and restoration

### Export formats

JSON is the lossless machine format. Include `formatVersion`, project identity/name, records, and optional history. Exclude local bindings and absolute local provenance paths by default for portable exports; offer an explicit include-local-metadata option. Mark omitted fields so the importer understands that provenance was intentionally reduced.

Markdown is a human-readable report with stable memory IDs and metadata blocks. V1 does not round-trip edited Markdown; import accepts the documented JSON format. This prevents a second writable source of truth.

Write export to a temporary file in the destination directory and rename after success. Refuse to overwrite unless explicitly requested. On restrictive/non-POSIX filesystems, report permission limitations without claiming privacy guarantees.

### Import rules

- Validate version, size limits, types, project mapping, and every record before committing.
- Provide a dry-run summary; no database writes in dry-run.
- Require a destination project, or an explicit create-project mode. Never bind exported machine paths automatically.
- Preserve IDs only for a verified restore into the same logical project; when copying into another project, generate IDs and remap internal supersession links.
- Same ID/same content is idempotent. Same ID/different content is a conflict, not last-writer-wins.
- Reject dangling or cross-project supersession references.
- Apply each selected import batch atomically. Bound batch size to avoid holding the writer lock indefinitely.

### Backup/restore runbook

1. Create a consistent SQLite backup using the verified runtime API.
2. Run an integrity check on the backup.
3. Record schema version and backup time outside the live database.
4. Restore into a temporary path first and inspect it with `doctor`.
5. Stop or quiesce processes that hold the live database before replacing it. WAL/SHM files must be handled consistently; do not swap the main file under active writers.
6. Re-resolve a known project and compare selected memory IDs/revisions.
7. Keep the pre-restore copy until validation succeeds.

Deleting `~/.cache/dejavu/` or running `dejavu index rebuild` must have no effect on the authoritative memory database. This is a release-blocking acceptance test.

## 14. Implementation map

Keep the legacy reader intact. Suggested new modules:

| Module | Responsibility |
| --- | --- |
| `src/project-memory-types.ts` | Public request/response/domain types |
| `src/project-identity.ts` | Canonical roots, Git resolution, boundary selection |
| `src/project-memory-store.ts` | Connections, migrations, scoped transactional repository operations |
| `src/project-memory.ts` | Validation, lifecycle rules, orchestration |
| `src/project-memory-recall.ts` | Pure eligibility/ranking/budgeting |
| `src/project-memory-transfer.ts` | JSON export/import and Claude migration |
| `src/project-memory-cli.ts` | Nested argument parsing, exit code mapping, output |
| `src/cli.ts` | Route `memory project` before legacy memory verbs |
| `test/project-identity.test.ts` | Repository/folder/worktree identity |
| `test/project-memory-store.test.ts` | Persistence, constraints, revisions, contention |
| `test/project-memory-recall.test.ts` | Scope, ranking, expiry, exact budgets |
| `test/cli-project-memory.test.ts` | Real subprocess contracts and compatibility |
| `test/project-memory-transfer.test.ts` | Import/export/migration/backup behavior |

These are responsibility boundaries, not a demand to create empty abstractions. Combine modules if the implementation remains small. Avoid a generic plugin framework or dependency-injection container.

Inject a clock, UUID generator, Git runner, filesystem canonicalizer, and store path where determinism or environment isolation requires it. Prefer functions and explicit dependencies, consistent with `MemoryDeps` in the existing reader.

Domain logic should not read global cwd/environment repeatedly; resolve those once at the CLI boundary and pass a concrete context. This makes it much harder for a request to change project halfway through execution.

### Suggested orchestration interface

```ts
interface ProjectContext {
  projectId: string;
  cwd: string;
  root: string;
  relativePath?: string;
  branch?: string;
}

interface MemoryService {
  add(ctx: ProjectContext, input: MemoryInput, actor: Actor,
      requestId?: string): Promise<unknown>;
  update(ctx: ProjectContext, id: string, expectedRevision: number,
      input: MemoryInput, actor: Actor, requestId?: string): Promise<unknown>;
  recall(ctx: ProjectContext, options: {
    query?: string; budgetChars: number; limit: number;
  }): Promise<unknown>;
}
```

Replace `unknown` with concrete result types in production. The sketch emphasizes scope and revision requirements, not final signatures.

## 15. Delivery sequence and acceptance gates

### Milestone 1: exact identity and durable CRUD

Implement path resolution, initialization, bindings, independent database, migrations, add/get/list/update/archive/history, validation, revision conflicts, and idempotency. Add the CLI namespace without touching legacy output.

Done when:

- Two directories with the same name remain distinct.
- A main checkout and linked worktree resolve to the same ID.
- Different clones remain separate until explicitly bound.
- Simultaneous writes preserve all successful records.
- Two updates to one revision produce one success and one conflict.
- Legacy memory tests pass unchanged.

### Milestone 2: useful recall and lifecycle

Implement deterministic search, scope/expiry eligibility, pinned records, bounded output, supersession, and body-free diagnostics.

Done when:

- A record written with `--harness claude` is returned to a Codex-labeled caller with identical ID/revision.
- A different project gets no context from that record.
- Branch/path restrictions work before ranking.
- Unverified, expired, archived, and superseded records do not appear in default recall.
- The complete context string obeys the budget for Unicode and tiny budgets.
- No recall operation invokes a model or refreshes the transcript index.

### Milestone 3: migration and operational recovery

Implement import/export, explicit Claude import, backup, doctor, and purge semantics. Exercise restoration and cache deletion.

Done when:

- Repeated unchanged Claude imports do not duplicate records.
- Changed imports do not overwrite curated records automatically.
- Invalid import causes no partial writes.
- Export/import preserves documented content and relationships.
- A backup restores successfully into a new temporary database.
- Transcript cache rebuild leaves memory records untouched.

### Milestone 4: integration and documentation

Update README/help/skill guidance, then implement and test one actual adapter at a time. Maintain adapters in the repository with installation instructions and version notes.

Done when:

- Claude, Codex, Pi, and OpenCode each complete the same explicit CLI read/write scenario, or any unavailable harness is honestly marked untested.
- Every advertised automatic adapter is tested in its actual harness, not just with a synthetic stdin payload.
- A broken database/CLI produces no user-facing hook failure and does not stall the harness.
- Compact/reset behavior and duplicate context are checked.
- A user can discover the actual database path and project bindings with `doctor`/`resolve`.

Do not delay the manual CLI release because an optional harness hook is unsupported. Do not call that hook supported before it has been verified.

## 16. Test plan

Use temporary data roots and actual temporary Git repositories. Never point automated tests at the user's real memory, home harness stores, or mounted working projects.

| Area | Required cases |
| --- | --- |
| Identity | Root, child directory, symlink, spaces, Unicode, same basename, nested Git repo, explicit nested folder, missing cwd, moved checkout |
| Worktrees | Main/worktree equality, branch rename, detached HEAD, removed worktree, common-dir relative path handling |
| Bindings | Idempotent init, explicit clone linking, duplicate conflicting binding, unbind preserves content |
| Storage | Missing DB read, schema upgrade, newer schema refusal, migration rollback, foreign-key enforcement |
| Concurrency | Parallel creates in separate processes, same-revision updates, locked DB timeout, rollback after injected failure |
| Retry | Same key/same payload, same key/different payload, successful commit followed by lost client response |
| Validation | Unknown fields, empty text, oversized body, invalid date, unsafe prefix, bad enum, invalid evidence |
| Scope | Exact project isolation, segment prefix matching, branch mismatch, detached HEAD, unknown branch |
| Lifecycle | Expiry boundary with fixed clock, review-due labeling, archive, supersede atomicity/cycle rejection |
| Recall | Stable ties, distinct-token weights, pinned overflow, no query, no matches, clipping, Unicode, metadata overhead |
| Transfers | Dry-run no mutation, repeat import, changed import conflict, malformed batch rollback, cross-project ID remapping |
| Recovery | Consistent backup during writes, restore, cache deletion, integrity diagnostics |
| CLI | JSON-only stdout, diagnostics on stderr, exit codes, stdin JSON, paths with spaces, unknown flags |
| Compatibility | Existing Claude commands and all current tests still pass |
| Adapters | Missing binary, timeout, missing project, malformed output, recursion guard, compaction, repeated prompt |

Property-style tests are useful for recall budgets: generate combinations of titles, evidence, and Unicode bodies and assert the cap plus deterministic output. Keep them seeded and bounded.

Concurrency tests must use separate processes/connections; two calls through one synchronous connection do not reproduce inter-process contention. Fault injection should fail between related statements and verify the transaction leaves no half-revision or half-supersession.

Run focused tests while building, then `bun run check` for the release candidate. Do not repeatedly run broad suites without new changes or concerns.

## 17. Performance and diagnostics

Initial performance goals are targets, not measurements:

- Project resolve plus warm recall under 150 ms at p95 on a local SSD with 1,000 project records.
- End-to-end hook memory retrieval comfortably inside a two-second deadline.
- No network/model calls, transcript scanning, or repository-wide file traversal on recall.
- Avoid write contention from reads; do not log each recall into the authoritative database.

Measure process startup, Git resolution, database selection, ranking, and rendering separately. Report cold and warm runs, data size, machine, runtime version, and percentiles. If Bun startup or Git dominates, optimize that before adding a vector index.

`doctor --json` should expose effective database path, schema version, connection mode, integrity result, project count, stale/ambiguous bindings, and migration needs. Avoid bodies, prompts, transcript excerpts, keys, and full environments in diagnostics.

Useful future metrics include selected/omitted counts, elapsed milliseconds, record-count distribution, and conflict frequency. Collect them only in an explicit local diagnostic mode, with clear retention. No telemetry service is needed.

## 18. Alternatives considered

### A `.dejavu/memory/` Markdown folder in every repository

Advantages: human-editable, Git-visible, naturally portable. Disadvantages: private context can be committed accidentally; every write dirties a checkout; parallel agents can lose edits; worktree sharing is awkward; Markdown metadata validation and revisions require additional machinery.

Use it later as an explicit export or team-authored knowledge surface. For this local cross-harness feature, choose transactional SQLite as the single authority.

### Claude's memory folder as the universal store

This preserves an existing format but ties identity and storage to one harness and its lossy project naming. It does not solve safe concurrent writes. Keep it as an import source.

### One shared AGENTS.md/CLAUDE.md file

Useful for instructions, unsuitable as an unbounded changing memory database. Harness instruction loading differs, and learned assertions should not automatically become high-priority instructions. A short pointer to the CLI is enough.

### Transcript summarization as the memory store

Transcripts already exist and Dejavu already searches them. Summarization can propose candidates later, but automatic promotion risks preserving guesses and stale assertions. Explicit curated writes provide a smaller, testable first release.

### A hosted service or MCP server

Potentially useful for remote environments and harnesses without shell access. Neither is required to share records among local harnesses. Add a transport over the same domain service later rather than creating a second memory implementation.

## 19. Deferred extensions with clear triggers

- **FTS:** when measured project search time exceeds the target at realistic corpus sizes.
- **MCP:** when a concrete consumer cannot run the CLI reliably.
- **Team memory:** when users explicitly need shared reviewed records; define publication, access, provenance, and conflicts separately.
- **Multi-device sync:** when local export/import is insufficient; use a defined merge protocol, not a network-mounted live SQLite file.
- **Model-assisted candidates:** when manual curation demonstrably misses valuable lessons; candidates remain unverified until checked.
- **Embeddings:** only after real retrieval examples show deterministic search failing and a benchmark demonstrates better relevant recall.
- **Worktree-specific transient notes:** when branch/prefix scope cannot express a real workflow; keep these out of shared durable facts.
- **Global personal preferences:** a separate namespace with explicit inclusion policy, not accidental fallback from unrelated projects.

## 20. Final implementation checklist

- [ ] Shared memory is owned by stable project identity, never a harness slug.
- [ ] Worktrees share intentionally; separate clones do not merge automatically.
- [ ] Authoritative storage is outside the disposable transcript cache.
- [ ] Legacy memory commands and JSON remain compatible.
- [ ] Every mutation is scoped, validated, transactional, and revisioned.
- [ ] Retried requests cannot silently duplicate writes.
- [ ] Recall filters project/path/branch/status/verification/expiry before ranking.
- [ ] Context budget includes the entire injected string.
- [ ] Evidence, clipping, review due, and provenance are visible.
- [ ] Read hooks never save memories or call models.
- [ ] Automatic adapters fail open and are tested against actual harness versions.
- [ ] Claude migration is explicit, repeatable, and source-preserving.
- [ ] Backup restoration and cache deletion tests pass.
- [ ] Purge limitations and copies in backups/exports are documented.
- [ ] README, help, and skill examples describe only commands that shipped.

## 21. Copyable handoff to the implementing agent

> Implement the proposed cross-harness project-memory feature in `/home/maheshk/dejavu`, following `docs/cross-harness-project-memory-implementation-handout.md`. First read the current repository instructions and confirm the inspected baseline has not invalidated this plan. Preserve `dejavu memory list|search|show` and their existing output contracts. Start with exact project identity and a separate authoritative SQLite store, then add revisioned CRUD and deterministic bounded recall under `dejavu memory project`. Use explicit project bindings, support Git worktrees, and never derive identity from harness display slugs. Add focused tests for isolation, concurrency, compatibility, expiry, and budgets. Continue through migration/recovery and documentation before advertising a complete release. Verify installed harness APIs before implementing automatic adapters; do not invent hook configuration. Do not modify real user memory or install hooks as a side effect of tests. Report any intentional deviations from the handout and demonstrate the cross-harness acceptance scenario with synthetic data.
