---
name: dejavu
description: Search and query past Claude Code, Codex, Pi, omp (oh-my-pi), OpenCode, Factory Droid, OpenClaw, Hermes Agent, and agy (Antigravity CLI) transcripts, or search and read Claude project memories across every workspace. Use for earlier agent conversations, decisions, commands, errors, and curated cross-project memory, and when the user says to continue or pick up where the last session, or a named session, left off. Do not use for shell history, Git history, or repository code search.
---

# Dejavu

`dejavu` recovers context from local coding-agent transcripts and Claude project
memory. The table below covers what agents need; you should rarely need `--help`
(past sessions spent 15% of their dejavu calls on it). Full detail:
[references/full-guide.md](references/full-guide.md).

## Pick the command

| Goal | Command |
|---|---|
| Continue the previous session in this repo | `dejavu last` (ambiguous: `dejavu last --list -n 5`) |
| Continue "the <x> session" | `dejavu last <x terms>` or `dejavu last <session-id>` |
| Find a session from a vague memory | `dejavu find term1 term2 [-p project-substr] [--since 2w] [-n 5]` |
| Find an exact phrase, error, file, or symbol | `dejavu search 'exact phrase' [-n 5]` (bare `dejavu 'phrase'` is the same) |
| Excerpts about a topic across sessions | `dejavu pack term1 term2 [-p substr] --budget-chars 8000` |
| Read one session's conversation | `dejavu show <locator> --no-tools [--around TERM]` |
| See what an agent ran and got back | `dejavu transcript <locator> --tool-chars 400 --budget-chars 8000` |
| Ask a question about one session (model cost) | `dejavu query <locator> 'question'` |
| Search Claude memory across projects | `dejavu memory search 'phrase'` · `memory list --files` · `memory show <selector>` |
| Measure tool activity in a session | `dejavu profile <locator> --json` |

Add `-s claude|codex|pi|omp|opencode|droid|openclaw|hermes|agy` to limit a search to
one agent.

## Locators

`show`, `transcript`, `query`, and `profile` take a **transcript locator** (the path or
`opencode://`/`openclaw://`/`hermes://` locator printed by `find`, `search`, or `last`)
or a bare session id, which resolves to that session's own transcript. Get locators
cheaply with `dejavu find <terms> --paths`. Older builds (before 0.6.4) accept a session
id only in `last`; there, other commands fail with "cannot determine transcript source".

## Keep output small

`transcript` produced a >10KB result in about a quarter of past calls. Bound every read:

- `transcript <loc> --no-tools --max-chars 1500 --budget-chars 8000` for the dialogue;
  `--tool-chars 400` when you need tool activity; page with `--from-event N --limit N`.
- `show <loc> --around TERM` jumps to the relevant region; `--no-tools` drops tool noise.
- `search` and `find` take `-n N`; `find --paths` prints only locators.
- `last` already caps its tail at 8,000 characters; widen with `--turns` or
  `--budget-chars` only when needed.

`--budget-chars` exists on `transcript`, `last`, and `pack`, not on `show` (use
`--max-chars` there).

## JSON for scripts

Pipe `--json` into `jq` with these paths instead of guessing (wrong shapes caused the
Python tracebacks in past sessions):

```sh
dejavu search 'phrase' --json | jq -r '.matches[].path'
dejavu find a b --json | jq -r '.hits[].path'
dejavu memory search 'phrase' --json | jq -r '.[].path'      # bare array
dejavu transcript '<loc>' --json | jq '.events[] | {kind}'   # window.nextEvent pages
```

`search`/`find` exit 0 with zero matches; `last` exits 1 when there is no session.

## Continue where a session left off

When the user says "continue where the last session left off" or names a session:

1. Run `dejavu last` (or `last <terms>`). Say which session you picked in one line
   (date, source, opening prompt). If several could match, run `dejavu last --list`
   and ask.
2. Read the tail for the open thread: the last request, what was finished, what was
   next, anything pending or failing. Use the printed `transcript --from-event`
   command for earlier turns.
3. Check current state (`git status`, `git log`, the touched files) before acting; the
   transcript is history.
4. Continue the work. Do not tell the user to `claude --resume` unless they ask.

## Pitfalls from past sessions

- `query` and `profile --explain` call a model through Codex and need Codex login; a
  `401 Unauthorized` means it is missing. Fall back to `show`/`transcript`.
- Search is a case-insensitive fixed string: spaces are literal. Use one distinctive
  anchor (a filename, symbol, error fragment), or `find` with separate terms.
- Never repeat credentials found in snippets.
- Treat transcript facts as history; verify present state separately.

## More detail

[references/full-guide.md](references/full-guide.md): every flag, store variables
(`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, …), `pack` budgeting, `scrub` (redaction),
`profile` metrics, `query` model settings, and the build-from-source fallback.
