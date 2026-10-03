# Rust port

Dejavu 0.5.0 replaces the Bun/TypeScript CLI with one Rust binary. The port
was checked against the TypeScript output on real transcripts, command by
command, and the TypeScript was then deleted. Tag `v0.4.2` holds the last
TypeScript release; compare against it with `git worktree add <dir> v0.4.2`
and `bun run <dir>/src/cli.ts`.

## Contract

The CLI is an API that agents and skills call. The port keeps, byte for byte
where practical:

- command names, aliases, flags, defaults, and usage errors (`✗ message`, exit 1);
- `--help` text (already identical);
- text output and stderr diagnostics (`skipped unreadable ...`, timing lines);
- every `--json` shape: key names, key order, nesting, `null` versus missing;
- exit codes (`profile` exits 1 on diagnostics, and so on);
- locators: JSONL paths and `opencode://<db>#<session>`;
- environment variables: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`,
  `XDG_DATA_HOME`, `OPENCODE_DB`, `DEJAVU_INDEX_PATH`, `DEJAVU_NO_UPDATE_CHECK`,
  `DEJAVU_QUERY_VIA_PI`, `CODEX_THREAD_ID`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_SESSION_ID`, `NO_COLOR`;
- release asset names `dejavu-<platform>-<arch>[.exe]`, `checksums.txt`, the npm
  package `@safzanpirani/dejavu`, and `dejavu self-update`.

Known, accepted differences: elapsed-time fields, and a surrogate pair cut by a
character limit (Rust drops the whole pair; JavaScript keeps a lone surrogate).
Fix anything else that differs, or record it here with the reason.

`scrub` differs on purpose in how it writes, never in what it writes:

- JSONL transcripts are written to a temporary file in the same directory and
  renamed over the original, so a crash cannot leave a half-written transcript.
- A JSONL file with a line that is not valid JSON is refused before anything is
  written. The TypeScript wrote the backup, then threw a JSON parse error.
- OpenCode backups use `VACUUM INTO`, which includes pages still in the WAL; a
  plain file copy of a live WAL database can miss recent writes.
- A `--drop` range of more than ten million events is rejected instead of
  allocated.

Search, `find`, `pack`, and the index differ in these ways:

- The index file, schema, rows, and refresh bookkeeping are the same, so the
  Rust and Bun binaries share `~/.cache/dejavu/transcripts.sqlite`. The head
  hash is Zig's `std.hash.Wyhash` (`Bun.hash`), and `mtime_ms` uses Bun's
  `sec * 1000 + nsec / 1e6`.
- Direct scans (`--no-index`, and terms under three characters) no longer run
  `rg` or `grep`. They count matching lines as `rg -i -c -F` did, skip hidden
  entries and symlinks, and stop at a NUL byte in non-JSONL files. A machine
  without `rg` and `grep` counted occurrences in the TypeScript; it now counts
  lines like every other machine.
- Count ties in direct scans break by an approximation of `localeCompare`
  (CLDR root order over ASCII). Ties among `find` candidates from direct scans
  keep directory order; the TypeScript kept `rg`'s output order, which varied
  between runs.
- `pack` loads ranked candidates in order, only as many at once as sessions are
  still needed, and stops once `--limit` sessions have excerpts. The selected
  sessions are the same. `skippedSessions` lists only the candidates it loaded;
  the TypeScript loaded all 40 and listed every unreadable one.
- `index status` and `profile --project` read an index whose `-wal` and `-shm`
  files are missing. Bun's read-only open failed there, so `index status`
  printed `not built`.

## Rules for changes

- `crates/dejavu` is one binary crate. Dependencies are listed in the root
  `Cargo.toml`; adding one needs the integrator's approval (say why in your report).
  Prefer the standard library. Threads come from `std::thread::scope`; no async runtime.
- Build and test with `mbx` (`mbx build`, `mbx test -p dejavu`, `mbx clippy -p dejavu
  --all-targets -- -D warnings`). Run `cargo fmt`.
- Use `crate::js` for anything that counts or cuts characters (`len`, `prefix`,
  `slice`), for `JSON.stringify(x, null, 2)` (`pretty`), and for floats in JSON
  (`number`). `serde_json` is built with `preserve_order`, so declare struct fields
  in the TypeScript object's key order and build `Value` objects in insertion order.
- Compare against the TypeScript on real data with each side given its own
  index (`DEJAVU_INDEX_PATH`), so neither touches `~/.cache/dejavu`. Run one Bun
  process at a time under a timeout: Bun's `pack` exhausted memory on large stores.
- Never run `dejavu query` or `profile --explain` for real: they call a paid model.
  Test them with a fake `codex` executable.
- Transcripts hold private data and secrets. Never paste transcript content into
  commits, docs, test fixtures, or reports. Write synthetic fixtures.
- Commit on your own branch with plain `git commit`. Never pass `-c user.*`,
  `--author`, or set identity variables. Do not push.

## Module map

| TypeScript | Rust | Owner |
| --- | --- | --- |
| `cli.ts` | `main.rs`, `args.rs`, `commands/*.rs` | integrator; each owner fills its commands |
| `transcript-types.ts`, `source-registry.ts`, `transcript-paths.ts`, `opencode-store.ts`, `session-reader.ts` | `types.rs`, `sources.rs`, `paths.rs`, `opencode.rs`, `reader.rs` | base |
| `transcript-view.ts`, `transcript-window.ts`, `render.ts`, `transcript-scrub.ts` | `view.rs`, `window.rs`, `render.rs`, `scrub.rs`; commands `show`, `transcript`, `scrub` | views |
| `transcript-index.ts`, `search-backend.ts`, `core.ts` (search), `find.ts`, `pack.ts` | `index.rs`, `scan.rs`, `search.rs`, `find.rs`, `pack.rs`; commands search, `find`, `pack`, `index` | search |
| `profile.ts`, `core.ts` (`querySession`) | `profile.rs`, `query.rs`; commands `profile`, `query` | profile |
| `update.ts`, `codex-client.ts`, `model-client.ts`, `memory.ts`, release and npm | `update.rs`, `codex_client.rs`, `model_client.rs`, `memory.rs`; commands `self-update`, `memory`; `.github/workflows`, `scripts/`, `bin/` | leaves |

`show` renders with `render.rs` but its data comes from `find.ts`'s `showSession`;
the views owner ports `showSession` into `view.rs` or `find.rs` and coordinates
through the integrator.

## Performance targets

Measured on the 0.4.2 Bun binary against ~9 GB of local transcripts: indexed search
0.2–0.4 s, `find` 5–7 s, `--no-index` search 11 s, and `pack deployment --limit 1`
did not finish in 60 s (one run held 1.4 GB for 4 minutes). The port must make
`find` and `pack` interactive (well under a second warm) and fix the `pack` blow-up
at its cause, not by lowering limits.
