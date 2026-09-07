import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Completion } from "./model-client.ts";

export const DEFAULT_CODEX_QUERY_MODEL = "gpt-5.6-luna";
export const CODEX_QUERY_REASONING = "medium";
const TIMEOUT_MS = 120_000;

export interface CodexDeps {
  /** Alternate executable for subprocess contract tests. */
  command?: string[];
  timeoutMs?: number;
}

/** A single ephemeral completion, shared by recall and future transcript analysis. */
export async function completeViaCodex(
  model: string,
  prompt: string,
  signal?: AbortSignal,
  deps: CodexDeps = {},
): Promise<Completion> {
  if (signal?.aborted) throw new Error("query was cancelled");
  const directory = await mkdtemp(join(tmpdir(), "dejavu-query-"));
  const output = join(directory, "answer.txt");
  const timeoutMs = deps.timeoutMs ?? TIMEOUT_MS;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let abort: (() => void) | undefined;
  try {
    if (signal?.aborted) throw new Error("query was cancelled");
    const command = [
      ...(deps.command ?? ["codex"]), "exec", "--ignore-user-config",
      "--ephemeral", "--skip-git-repo-check", "--sandbox", "read-only",
      "--model", model, "--color", "never", "--json",
      "--output-last-message", output,
      "-c", 'model_provider="openai"',
      "-c", `model_reasoning_effort="${CODEX_QUERY_REASONING}"`,
      "-c", 'approval_policy="never"',
      "-c", "project_doc_max_bytes=0",
      "-c", "skills.include_instructions=false",
      "-c", "skills.bundled.enabled=false",
      "-c", "features.shell_tool=false",
      "-c", "features.apps=false",
      "-c", "features.multi_agent=false",
      "-c", "features.skill_search=false",
      "-c", 'web_search="disabled"',
      "-",
    ];
    let child;
    try {
      child = Bun.spawn(command, {
        cwd: directory,
        stdin: new Blob([prompt]),
        stdout: "pipe",
        stderr: "pipe",
        detached: process.platform !== "win32",
      });
    } catch {
      throw new Error("could not start codex exec; install Codex and make sure codex is on PATH");
    }
    const stop = () => {
      // Only this invocation's process group is owned by Dejavu.
      try {
        if (process.platform !== "win32") process.kill(-child.pid, "SIGKILL");
        else child.kill("SIGKILL");
      } catch { /* The process may already have exited. */ }
    };
    abort = stop;
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) stop();
    let timedOut = false;
    timer = setTimeout(() => { timedOut = true; stop(); }, timeoutMs);
    const [exitCode, stdout] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      // Drain diagnostics, but never echo a prompt-bearing Codex log into errors.
      new Response(child.stderr).text(),
    ]).catch((error) => { stop(); throw error; });
    if (signal?.aborted) throw new Error("query was cancelled");
    if (timedOut) throw new Error(`query model timed out after ${timeoutMs}ms`);
    if (exitCode !== 0) {
      throw new Error(`codex exec failed (exit ${exitCode}); check codex login status and access to ${model}`);
    }
    let usage: Completion["usage"];
    for (const line of stdout.split("\n")) {
      let event;
      try { event = JSON.parse(line); } catch { continue; }
      if (!event || typeof event !== "object") continue;
      if (event.type === "turn.failed" || event.type === "error") {
        throw new Error(`codex query failed; check codex login status and access to ${model}`);
      }
      if (event.type === "turn.completed" && typeof event.usage?.input_tokens === "number"
        && typeof event.usage?.output_tokens === "number") {
        usage = { inputTokens: event.usage.input_tokens, outputTokens: event.usage.output_tokens };
      }
    }
    const answer = await readFile(output, "utf8").catch((error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return "";
      throw error;
    });
    if (!answer.trim()) throw new Error("query model returned an empty response");
    return { answer: answer.trim(), transport: "codex", usage };
  } finally {
    clearTimeout(timer);
    if (abort) signal?.removeEventListener("abort", abort);
    await rm(directory, { recursive: true, force: true });
  }
}
