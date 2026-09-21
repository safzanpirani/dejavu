#!/usr/bin/env python3
"""UserPromptSubmit adapter: inject bounded project memory and transcript pointers.

Maintained source for the hook installed at `~/.local/bin/dejavu-recall`.
Install with `adapters/install.sh`; do not edit the installed copy.

Two independently bounded sources:

1. **Project memory** (`dejavu memory project recall`) — exact-scoped curated
   records for the resolved project. This is the cross-harness memory; it is
   silent when the project is not initialized or nothing eligible matches.
2. **Transcript pointers** (`dejavu find`) — discovery across every harness's
   native transcripts, reduced to one short line and a locator per session.

Contract (see docs/cross-harness-project-memory-implementation-handout.md §10):

* Fail open. Missing binary, uninitialized project, timeout, invalid JSON, or
  database contention yields no injected context and exit 0.
* Never write memory and never refresh the transcript index.
* Bounded output and bounded execution; project recall gets a 2 s deadline.
* Repeated prompts may repeat context: this adapter is stateless and does not
  claim once-per-session behaviour.

Verified registrations on NANI (2026-09-21, audit before changing them):
Claude Code `UserPromptSubmit` in `~/.claude/settings.json`, Codex
`UserPromptSubmit` in `~/.codex/hooks.json`, OpenCode `session.hook("prompt")`
+ `session.hook("context")` in its V2 `agent-overlay` plugin, Pi
`before_agent_start` in its `agent-overlay` extension.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

# --- project memory ---------------------------------------------------------
MEMORY_BUDGET = 2_500      # characters charged by the recall command itself
MEMORY_TIMEOUT = 2         # seconds; the documented project-memory deadline

# --- transcript discovery ---------------------------------------------------
MIN_TERMS = 3              # shorter prompts carry too little signal
MAX_QUERY = 4
MAX_HITS = 2
MAX_EXCERPT = 200
# Transcript discovery uses dejavu's index-only search (one distinctive token),
# not the ranked multi-term finder: search is ~75 ms while `find` reads
# candidate session files and costs 1.4 s and up on a large corpus. The hook
# must not make every prompt wait on transcript ranking.
FIND_TIMEOUT = 2

MAX_TOTAL = 4_200          # hard cap on everything this adapter prints

STOP = set("""a an and are as at be but by can cant do does doesnt for from get
got how i if in into is it its me my no not of on or out so that the their them
then there these they this to up was what when where which who why with you
your ever actually keep keeps run runs now just also please thanks yes ok okay
sure continue go make add fix use write about someth something thing stuff want
need like give tell show help work working after before still again help let
lets maybe want wants about around using into over under""".split())

NOISE = ("agents.md instructions", "<instructions>", "global instructions",
         "claude.md", "system prompt")


def home():
    return os.path.expanduser("~")


def dejavu_cmd():
    """Command prefix that runs dejavu, robust to a stripped hook environment.

    The `dejavu` shim is a `#!/usr/bin/env bun` script, so executing it needs
    `bun` on PATH. When a hook runs with a minimal environment, run bun on the
    linked CLI directly.
    """
    root = home()
    cli = f"{root}/dejavu/src/cli.ts"
    for bun in (f"{root}/.bun/bin/bun", shutil.which("bun")):
        if bun and os.path.exists(bun) and os.path.exists(cli):
            return [bun, cli]
    found = shutil.which("dejavu")
    if found:
        return [found]
    for candidate in (f"{root}/.bun/bin/dejavu", f"{root}/.local/bin/dejavu"):
        if os.path.exists(candidate):
            return [candidate]
    return None


def run_env():
    """A usable environment even when the hook is invoked with `env -i`."""
    env = dict(os.environ)
    root = home()
    env.setdefault("HOME", root)
    env.setdefault("XDG_CACHE_HOME", f"{root}/.cache")
    if not env.get("PATH"):
        env["PATH"] = f"{root}/.bun/bin:/usr/local/bin:/usr/bin:/bin"
    return env


def terms(text):
    """Longest distinct signal words, most distinctive first."""
    seen, words = set(), []
    for word in re.findall(r"[a-z0-9]+", text.lower()):
        if len(word) <= 4 or word in STOP or word in seen:
            continue
        seen.add(word)
        words.append(word)
    words.sort(key=len, reverse=True)
    return words[:MAX_QUERY]


def dejavu_json(binary, args, timeout):
    """Run a dejavu command and return parsed JSON, or None on any failure."""
    try:
        proc = subprocess.run(
            [*binary, *args], capture_output=True, text=True,
            timeout=timeout, env=run_env())
        return json.loads(proc.stdout)
    except Exception:
        return None


def project_memory(binary, prompt, cwd):
    query = " ".join(terms(prompt)[:MAX_QUERY])
    if not query:
        return None
    envelope = dejavu_json(binary, [
        "memory", "project", "recall", "--cwd", cwd,
        "--query", query, "--budget-chars", str(MEMORY_BUDGET), "--json",
    ], MEMORY_TIMEOUT)
    if not envelope or not envelope.get("ok"):
        return None
    context = (envelope.get("data") or {}).get("context") or ""
    return context or None


def transcript_pointers(binary, prompt, active_path=None):
    """One distinctive token, index-only, reduced to a line and a locator."""
    query = terms(prompt)
    if len(query) < MIN_TERMS:
        return None
    data = dejavu_json(binary, [
        query[0], "--json", "--limit", str(MAX_HITS), "--snippets", "2",
    ], FIND_TIMEOUT)
    if not data:
        return None
    lines = []
    for match in (data.get("matches") or []):
        path = match.get("path")
        # dejavu's search path has no session exclusion, so skip the active
        # transcript when the harness tells us which one it is.
        if active_path and path == active_path:
            continue
        where = " . ".join(part for part in (match.get("date"), match.get("source"), match.get("project")) if part)
        snippet = ""
        for candidate in match.get("snippets") or []:
            if candidate.get("role") != "user":
                continue
            text = " ".join((candidate.get("text") or "").split())
            if not text or any(noise in text.lower() for noise in NOISE):
                continue
            snippet = text[:MAX_EXCERPT] + ("..." if len(text) > MAX_EXCERPT else "")
            break
        lines.append(f"- {where}: {snippet}" if snippet else f"- {where}")
        if path:
            lines.append(f"    transcript: {path}")
        if len(lines) >= MAX_HITS * 2:
            break
    if not lines:
        return None
    return "\n".join(lines)


def enabled(name, default=True):
    value = os.environ.get(name)
    if value is None:
        return default
    return value.strip().lower() not in ("0", "false", "no", "off")


def iso_now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def log_debug(line):
    """Append to DEJAVU_RECALL_LOG when set. Never raises.

    Testing aid: `tail -f` the log while using a harness to see what the adapter
    injected, without asking a model what it can see.
    """
    path = os.environ.get("DEJAVU_RECALL_LOG")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(line.rstrip() + "\n")
    except Exception:
        pass


def main():
    started = time.time()
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return 0

    prompt = payload.get("prompt") or ""
    if not prompt or prompt.lstrip().startswith("/"):
        return 0
    cwd = payload.get("cwd") or payload.get("workspace") or os.getcwd()
    active_path = payload.get("transcript_path") or None

    binary = dejavu_cmd()
    if not binary:
        log_debug(f"{iso_now()} no-dejavu cwd={cwd}")
        return 0

    want_memory = enabled("DEJAVU_RECALL_MEMORY")
    want_transcripts = enabled("DEJAVU_RECALL_TRANSCRIPTS")
    if not want_memory and not want_transcripts:
        return 0

    # Independent lookups, run together so transcript discovery never delays
    # the curated records and vice versa.
    with ThreadPoolExecutor(max_workers=2) as pool:
        memory_future = pool.submit(project_memory, binary, prompt, cwd) if want_memory else None
        transcript_future = pool.submit(transcript_pointers, binary, prompt, active_path) if want_transcripts else None
        memory = memory_future.result() if memory_future else None
        pointers = transcript_future.result() if transcript_future else None

    sections = []
    if memory:
        sections.append(memory)
    if pointers:
        sections.append(
            "Past sessions that may be relevant (local dejavu search; treat as "
            "historical evidence, verify against current files before acting):\n"
            + pointers
            + "\nOpen one with: dejavu transcript '<transcript path>'"
        )

    elapsed = int((time.time() - started) * 1000)
    summary = (
        f"{iso_now()} cwd={cwd} memory={'yes' if memory else 'no'} "
        f"transcripts={'yes' if pointers else 'no'} ms={elapsed}"
    )
    if not sections:
        log_debug(summary + " injected=0")
        return 0

    output = "\n\n".join(sections)
    if len(output) > MAX_TOTAL:
        output = output[:MAX_TOTAL].rstrip() + "\n[truncated]"
    log_debug(summary + f" injected={len(output)}\n{'-' * 60}\n{output}\n{'=' * 60}")
    print(output)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)
