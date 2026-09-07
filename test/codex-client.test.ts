import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { completeViaCodex } from "../src/codex-client.ts";
import { resolveQueryModel } from "../src/model-client.ts";

let directory: string;
let fixture: string;

beforeAll(async () => {
  directory = await mkdtemp(join(tmpdir(), "dejavu-codex-test-"));
  fixture = join(directory, "fake-codex.ts");
  await writeFile(fixture, `
    const mode = process.argv[2];
    const args = process.argv.slice(3);
    const prompt = await Bun.stdin.text();
    const output = args[args.indexOf('--output-last-message') + 1];
    if (mode === 'slow') { await Bun.sleep(10000); process.exit(0); }
    if (mode === 'fail') { console.error('private-transcript-sentinel'); process.exit(3); }
    if (mode === 'missing') process.exit(0);
    if (mode === 'event-fail') {
      await Bun.write(output, 'partial answer');
      console.log(JSON.stringify({ type: 'turn.failed', error: { message: 'private-transcript-sentinel' } }));
      process.exit(0);
    }
    await Bun.write(output, mode === 'empty' ? '   ' : JSON.stringify({ args, prompt, cwd: process.cwd() }));
    console.log('diagnostic noise');
    console.log('null');
    console.log(JSON.stringify({ type: 'item.completed', item: { type: 'agent_message', text: 'not the final output file' } }));
    console.log(JSON.stringify({ type: 'turn.completed', usage: { input_tokens: 120, cached_input_tokens: 60, output_tokens: 8 } }));
  `);
});

afterAll(async () => { await rm(directory, { recursive: true, force: true }); });

describe("Codex query resolution", () => {
  test("defaults to Luna medium without Pi config, even if old settings exist", async () => {
    await writeFile(join(directory, "session-recall.json"), JSON.stringify({ queryModel: { provider: "old", id: "model" } }));
    expect(await resolveQueryModel(directory)).toMatchObject({
      provider: "codex", id: "gpt-5.6-luna", reasoningEffort: "medium",
    });
    expect(await resolveQueryModel(join(directory, "nonexistent"))).toMatchObject({ provider: "codex", id: "gpt-5.6-luna" });
  });

  test("accepts bare and codex-prefixed model overrides and preserves explicit legacy providers", async () => {
    expect(await resolveQueryModel(directory, "gpt-other")).toMatchObject({ provider: "codex", id: "gpt-other" });
    expect(await resolveQueryModel(directory, "codex/gpt-other")).toMatchObject({ provider: "codex", id: "gpt-other" });
    expect(await resolveQueryModel(directory, "example/legacy")).toMatchObject({ provider: "example", id: "legacy" });
    await expect(resolveQueryModel(directory, "codex/")).rejects.toThrow("must not be empty");
  });
});

describe("Codex subprocess contract", () => {
  const run = (mode: string, signal?: AbortSignal, timeoutMs = 2000) => completeViaCodex(
    "gpt-5.6-luna", "private transcript and question", signal,
    { command: [process.execPath, fixture, mode], timeoutMs },
  );

  test("passes context through stdin, reads only the final answer, reports usage, and cleans up", async () => {
    const result = await run("ok");
    expect(result).toMatchObject({ transport: "codex", usage: { inputTokens: 120, outputTokens: 8 } });
    const answer = JSON.parse(result.answer);
    expect(answer.prompt).toBe("private transcript and question");
    expect(answer.args).not.toContain(answer.prompt);
    expect(answer.args).toContain("--ephemeral");
    expect(answer.args).toContain("--ignore-user-config");
    expect(answer.args).toContain('model_reasoning_effort="medium"');
    expect(answer.args).toContain('model_provider="openai"');
    expect(answer.args).toContain("features.shell_tool=false");
    expect(answer.args[answer.args.indexOf("--model") + 1]).toBe("gpt-5.6-luna");
    expect(answer.args[answer.args.indexOf("--sandbox") + 1]).toBe("read-only");
    expect(answer.cwd).not.toBe(process.cwd());
    expect(await stat(answer.cwd).catch(() => undefined)).toBeUndefined();
  });

  test("fails on a nonzero exit without leaking child stderr", async () => {
    await expect(run("fail")).rejects.toThrow("codex exec failed (exit 3)");
  });

  test("rejects missing, empty, and failed final answers", async () => {
    await expect(run("missing")).rejects.toThrow("empty response");
    await expect(run("empty")).rejects.toThrow("empty response");
    await expect(run("event-fail")).rejects.toThrow("codex query failed");
  });

  test("bounds a stalled child", async () => {
    await expect(run("slow", undefined, 100)).rejects.toThrow("timed out after 100ms");
  });

  test("handles cancellation before and during a query", async () => {
    const before = new AbortController();
    before.abort();
    await expect(run("ok", before.signal)).rejects.toThrow("query was cancelled");
    const during = new AbortController();
    const timer = setTimeout(() => during.abort(), 100);
    try { await expect(run("slow", during.signal)).rejects.toThrow("query was cancelled"); }
    finally { clearTimeout(timer); }
  });
});
