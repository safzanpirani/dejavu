#!/usr/bin/env bun
import { homedir } from "node:os";
import { join } from "node:path";
import { DEFAULT_MAX_PARALLEL } from "./concurrency.ts";
import { DEFAULT_SEARCH_LIMIT, DEFAULT_SNIPPET_LIMIT, querySession, searchSessions } from "./core.ts";
import { DEFAULT_FIND_LIMIT, findSessions, showSession } from "./find.ts";
import { defaultMemoryRoot, listMemories, memoryFiles, searchMemories, showMemory } from "./memory.ts";
import { renderFind, renderQuery, renderSearch, renderShow } from "./render.ts";
import { discoverTranscriptStores, parseSource } from "./source-registry.ts";
import { refreshTranscriptIndex, transcriptIndexStatus } from "./transcript-index.ts";
import type { StoreDiagnostic } from "./transcript-types.ts";

const colors = {
  red: (text: string) => `\x1b[31m${text}\x1b[0m`,
  dim: (text: string) => `\x1b[90m${text}\x1b[0m`,
  bold: (text: string) => `\x1b[1m${text}\x1b[0m`,
};

const HELP = `${colors.bold("dejavu")}: search and query coding-agent transcripts

  ${colors.bold("dejavu")} <token-or-exact-phrase> [flags]
  ${colors.bold("dejavu find")} <term> [term...] [flags]
  ${colors.bold("dejavu show")} <transcript-locator> [flags]
  ${colors.bold("dejavu query")} <transcript-locator> <question> [flags]
  ${colors.bold("dejavu memory list")} [--files] [--root DIR] [--json]
  ${colors.bold("dejavu memory search")} <phrase> [--limit N] [--snippets N] [--root DIR] [--json]
  ${colors.bold("dejavu memory show")} <project-or-file> [--root DIR] [--json]
  ${colors.bold("dejavu index")} <status|update|rebuild> [--json]

search flags
  -s, --source NAME      all, claude, codex, pi, or opencode (default all)
  -n, --limit N          transcripts to return (default ${DEFAULT_SEARCH_LIMIT})
      --snippets N       snippets per transcript (default ${DEFAULT_SNIPPET_LIMIT})
      --max-parallel N   local store/file workers (default ${DEFAULT_MAX_PARALLEL})
      --no-index         bypass the transcript index and scan files directly

find flags (multi-term session finder, ranked, user messages weighted)
  -s, --source NAME      restrict to one source
  -n, --limit N          sessions to return (default ${DEFAULT_FIND_LIMIT})
  -p, --project SUBSTR   only sessions whose project path contains SUBSTR
      --since WHEN       YYYY-MM-DD or 7d / 2w / 3m
      --user             require every term to appear in user messages
      --paths            print matching transcript locators only, one per line
      --max-parallel N   local store/candidate workers (default ${DEFAULT_MAX_PARALLEL})
      --no-index         bypass the transcript index and scan files directly

show flags
      --full             do not truncate long messages
      --around TERM      only messages containing TERM, with 3 turns of context

query flags
      --model P/ID       Pi model used to answer the question
      --agent-dir P      Pi config directory (default ~/.pi/agent)

common flags
      --json             emit the complete structured result
  -q, --quiet            suppress stderr diagnostics
  -h, --help             show this help

Search covers detected Claude, Codex, Pi, and OpenCode stores by default.
Memory commands read Claude's cross-project Markdown memory corpus.
It is case-insensitive literal fixed-string search, not semantic search.
Use one distinctive token or exact phrase per call.`;

function die(message: string): never {
  console.error(colors.red(`✗ ${message}`));
  process.exit(1);
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
    if (value === undefined || value.startsWith("-")) die(`${name} needs a value`);
    args.splice(index, 2);
    return value;
  }
  return undefined;
}

function integer(value: string | undefined, flag: string, fallback: number): number {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) die(`${flag} needs an integer >= 1 (got '${value}')`);
  return parsed;
}

function rejectUnknownFlags(args: string[]): void {
  const unknown = args.find((arg) => arg.startsWith("-") && arg !== "-");
  if (unknown) die(`unknown flag: ${unknown}`);
}

function reportSkippedStores(diagnostics: StoreDiagnostic[], quiet: boolean): void {
  if (quiet) return;
  for (const diagnostic of diagnostics) {
    console.error(colors.dim(`skipped unreadable ${diagnostic.source} store ${diagnostic.path}: ${diagnostic.error}`));
  }
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  if (args.length === 0 || pullFlag(args, "-h", "--help")) {
    console.log(HELP);
    return;
  }
  const json = pullFlag(args, "--json");
  const quiet = pullFlag(args, "-q", "--quiet");
  if (args[0] === "index") {
    args.shift();
    const verb = args.shift() ?? "status";
    rejectUnknownFlags(args);
    if (args.length > 0) die(`index ${verb} accepts no positional arguments (unexpected: '${args[0]}')`);
    if (verb === "status") {
      const result = await transcriptIndexStatus();
      console.log(json ? JSON.stringify(result, null, 2) : result.exists
        ? `${result.files} files · ${result.messages} messages · ${result.bytes} bytes · schema v${result.schemaVersion} · ${result.path}`
        : `not built · ${result.path}`);
      return;
    }
    if (verb === "update" || verb === "rebuild") {
      const result = await refreshTranscriptIndex(await discoverTranscriptStores("all"), undefined, verb === "rebuild");
      console.log(json ? JSON.stringify(result, null, 2) : `${result.files} files · ${result.messages} messages · ${result.indexed} indexed · ${result.removed} removed · ${result.elapsedMs}ms · ${result.path}`);
      reportSkippedStores(result.skipped, quiet || json);
      return;
    }
    die(`unknown index command '${verb}' (status|update|rebuild)`);
  }
  if (args[0] === "memory") {
    args.shift();
    const verb = args.shift() ?? "list";
    const root = pullValue(args, ["--root"]) ?? defaultMemoryRoot();
    if (verb === "list") {
      const files = pullFlag(args, "--files");
      rejectUnknownFlags(args);
      if (args.length > 0) die(`memory list accepts no positional arguments (unexpected: '${args[0]}')`);
      if (files) {
        const result = await memoryFiles(root);
        console.log(json ? JSON.stringify(result, null, 2) : result.map((file) => `${file.project}/${file.name}\t${file.path}`).join("\n"));
      } else {
        const result = await listMemories(root);
        console.log(json ? JSON.stringify(result, null, 2) : result.map((project) => `${project.project}\t${project.files}\t${project.path}`).join("\n"));
      }
      return;
    }
    if (verb === "search") {
      const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", 20);
      const snippets = integer(pullValue(args, ["--snippets"]), "--snippets", 3);
      rejectUnknownFlags(args);
      const query = args.join(" ").trim() || die("memory search needs one token or exact phrase");
      const result = await searchMemories(query, { root, limit, snippets });
      if (json) console.log(JSON.stringify(result, null, 2));
      else console.log(result.map((match) => `${match.count}\t${match.project}/${match.name}\t${match.path}\n  ${match.snippets.join("\n  ")}`).join("\n"));
      return;
    }
    if (verb === "show") {
      rejectUnknownFlags(args);
      const selector = args.shift() ?? die("memory show needs a project slug, project substring, or memory file path");
      if (args.length > 0) die(`memory show accepts one selector (unexpected: '${args[0]}')`);
      const result = await showMemory(selector, root);
      console.log(json ? JSON.stringify(result, null, 2) : result.content);
      return;
    }
    die(`unknown memory command '${verb}' (list|search|show)`);
  }
  if (args[0] === "find") {
    args.shift();
    const source = parseSource(pullValue(args, ["-s", "--source"]) ?? "all");
    const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", DEFAULT_FIND_LIMIT);
    const project = pullValue(args, ["-p", "--project"]);
    const since = pullValue(args, ["--since"]);
    const userOnly = pullFlag(args, "--user");
    const pathsOnly = pullFlag(args, "--paths");
    const maxParallel = integer(pullValue(args, ["--max-parallel"]), "--max-parallel", DEFAULT_MAX_PARALLEL);
    const noIndex = pullFlag(args, "--no-index");
    rejectUnknownFlags(args);
    if (args.length === 0) die("find needs one or more terms");
    const result = await findSessions(args, { source, limit, project, since, userOnly, maxParallel, noIndex });
    if (pathsOnly) console.log(result.hits.map((hit) => hit.path).join("\n"));
    else console.log(json ? JSON.stringify(result, null, 2) : renderFind(result));
    reportSkippedStores(result.skippedStores, quiet);
    if (!quiet && !json) {
      const timings = Object.entries(result.storeTimings).map(([store, ms]) => `${store} ${ms}ms`).join(" · ");
      console.error(colors.dim(`${timings} · total ${result.elapsedMs}ms`));
    }
    return;
  }
  if (args[0] === "show") {
    args.shift();
    const full = pullFlag(args, "--full");
    const around = pullValue(args, ["--around"]);
    rejectUnknownFlags(args);
    const locator = args.shift() ?? die("show needs a transcript locator from search results");
    if (args.length > 0) die(`show accepts one transcript locator (unexpected argument: '${args[0]}')`);
    const result = await showSession(locator, { full, around });
    console.log(json ? JSON.stringify(result, null, 2) : renderShow(result));
    if (!quiet && !json) console.error(colors.dim(`${result.source} · ${result.messageCount} message${result.messageCount === 1 ? "" : "s"}`));
    return;
  }
  if (args[0] === "query") {
    args.shift();
    const agentDir = pullValue(args, ["--agent-dir"]) ?? join(homedir(), ".pi", "agent");
    const model = pullValue(args, ["--model"]);
    rejectUnknownFlags(args);
    const locator = args.shift() ?? die("query needs a transcript locator from search results");
    const question = args.join(" ").trim() || die("query needs a question");
    const result = await querySession(locator, question, { agentDir, model });
    console.log(json ? JSON.stringify(result, null, 2) : renderQuery(result));
    if (!quiet && !json) {
      console.error(colors.dim(`${result.source} · ${result.model.provider}/${result.model.id} · ${result.messageCount} message${result.messageCount === 1 ? "" : "s"}${result.wasWindowed ? " · windowed" : ""} · ${result.elapsedMs}ms`));
    }
    return;
  }
  const source = parseSource(pullValue(args, ["-s", "--source"]) ?? "all");
  const limit = integer(pullValue(args, ["-n", "--limit"]), "--limit", DEFAULT_SEARCH_LIMIT);
  const snippets = integer(pullValue(args, ["--snippets"]), "--snippets", DEFAULT_SNIPPET_LIMIT);
  const maxParallel = integer(pullValue(args, ["--max-parallel"]), "--max-parallel", DEFAULT_MAX_PARALLEL);
  const noIndex = pullFlag(args, "--no-index");
  rejectUnknownFlags(args);
  const query = args.join(" ").trim() || die("need one token or exact phrase to search");
  const result = await searchSessions(query, { source, limit, snippets, maxParallel, noIndex });
  console.log(json ? JSON.stringify(result, null, 2) : renderSearch(result));
  reportSkippedStores(result.skippedStores, quiet);
  if (!quiet && !json) console.error(colors.dim(`${result.sources.join(",")} · ${result.elapsedMs}ms`));
}

main().catch((error) => die(error instanceof Error ? error.message : String(error)));
