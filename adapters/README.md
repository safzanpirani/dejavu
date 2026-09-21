# Adapters

Harness integration for the project memory service. The baseline is always the
CLI: any agent with shell access can run `dejavu memory project …` and get the
same records. Adapters only add automatic injection on top of that baseline.

| File | Purpose |
| --- | --- |
| `recall-hook.py` | `UserPromptSubmit`-style adapter: bounded project memory + transcript pointers |
| `install.sh` | Installs the adapter to `~/.local/bin/dejavu-recall` (backs up any existing file) |

## Install

```bash
adapters/install.sh
```

Override the destination with `DEJAVU_RECALL_TARGET`. The installed file is a
build artifact — edit `adapters/recall-hook.py` and re-run the installer.

## What the adapter does

1. **Project memory** — `dejavu memory project recall --cwd <workspace>
   --query <terms> --budget-chars 2500 --json`, with a 2 s deadline. Exact-scoped
   to the project resolved from the workspace path; silent when the project is
   not initialized or nothing eligible matches.
2. **Transcript pointers** — `dejavu find` across Claude, Codex, Pi, and
   OpenCode transcripts, reduced to one short line and a locator per session.
   Requires at least three signal terms so short prompts cost nothing.

The two sources are budgeted independently, and the whole injected string is
capped at 4,200 characters.

## Switches

| Variable | Default | Effect |
| --- | --- | --- |
| `DEJAVU_RECALL_MEMORY` | on | include project memory |
| `DEJAVU_RECALL_TRANSCRIPTS` | on | include transcript pointers |
| `DEJAVU_RECALL_LOG` | unset | append a timestamped record of each run (cwd, which sources fired, milliseconds, and the injected text) to this file |

`DEJAVU_RECALL_LOG` is the testing aid: `tail -f` it while using a harness to
see exactly what was injected, without asking a model what it can see. Anything
other than `0`/`false`/`no`/`off` keeps a source enabled.

Typical timings on this machine (1,000 indexed transcripts): project memory
~35 ms, transcript pointers ~75 ms, both ~240 ms end to end.

## Contract

- Fail open: missing binary, uninitialized project, timeout, invalid JSON, or
  database contention yields no context and exit 0.
- Never writes memory and never refreshes the transcript index.
- Never infers a path from prompt text; path scope only applies when a harness
  supplies the file being edited.
- Stateless: repeated prompts may repeat context. It does not claim
  once-per-session deduplication.

## Registered harness hooks

Audited on NANI 2026-09-21. Verify against installed versions before changing
anything; these are the observed registrations, not a claim about upstream APIs.

| Harness | Registration | Event |
| --- | --- | --- |
| Claude Code | `~/.claude/settings.json` | `UserPromptSubmit` |
| Codex | `~/.codex/hooks.json` | `UserPromptSubmit` (also trust-hashed in `~/.codex/config.toml`) |
| OpenCode | `~/.config/opencode/plugins/agent-overlay.ts` | V2 plugin: `session.hook("prompt")` → `session.hook("context")` |
| Pi | `~/.pi/agent/extensions/agent-overlay.ts` | `before_agent_start` |

The OpenCode and Pi files also carry unrelated agent-overlay HUD reporting;
only the recall half is documented here.

OpenCode V2 rejects V1 plugin implementations outright
(`Plugin must export a default definition with an id and an effect or setup
function`). The plugin must default-export `{ id, setup(ctx) }`; the V1
`export const X = async () => ({...})` shape silently fails to load. Check with
`opencode api get /api/plugin` and look for `"status": "active"`. The file
watcher reloads a changed plugin without restarting the service.

For OpenCode, `DEJAVU_RECALL_*` switches must be present in the environment of
the background service, not just the shell you launch the TUI from.

Changing the adapter's contents does **not** require re-registering hooks. Codex
records a hash of the hook *command string* in `[hooks.state]`, so even a hook
path change needs its `trusted_hash` refreshed (read the current value with the
app-server `hooks/list` request).

## Uninstall

Remove the `UserPromptSubmit` entry from the harness config that references
`dejavu-recall`, then delete `~/.local/bin/dejavu-recall`. No memory data lives
in the adapter.
