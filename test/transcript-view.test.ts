import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openCodeLocator } from "../src/opencode-store.ts";
import { renderTranscript } from "../src/render.ts";
import { loadTranscriptEvents, viewTranscript, type TranscriptEvent } from "../src/transcript-view.ts";

/** Drops provenance fields so fixtures can be compared by content. */
function bare(events: TranscriptEvent[]): Record<string, unknown>[] {
  return events.map(({ index: _index, ref: _ref, ...rest }) => rest);
}

async function withJsonl(relativePath: string, lines: unknown[], run: (path: string) => Promise<void>): Promise<void> {
  const root = await mkdtemp(join(tmpdir(), "dejavu-transcript-"));
  const path = join(root, relativePath);
  await mkdir(join(path, ".."), { recursive: true });
  await writeFile(path, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
  try { await run(path); }
  finally { await rm(root, { recursive: true, force: true }); }
}

describe("loadTranscriptEvents", () => {
  test("claude: follows the branch and pairs tool_use with tool_result", async () => {
    const lines = [
      { uuid: "u1", parentUuid: null, type: "user", cwd: "/work/demo", timestamp: "2026-08-01T08:00:00Z", message: { role: "user", content: "hello" } },
      { uuid: "a1", parentUuid: "u1", type: "assistant", timestamp: "2026-08-01T08:00:01Z", message: { role: "assistant", content: [
        { type: "thinking", thinking: "plan" },
        { type: "text", text: "looking" },
        { type: "tool_use", id: "t1", name: "Bash", input: { command: "ls" } },
      ] } },
      { uuid: "r1", parentUuid: "a1", type: "user", message: { role: "user", content: [{ type: "tool_result", tool_use_id: "t1", content: "a\nb", is_error: false }] } },
      { uuid: "a2", parentUuid: "r1", type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "done" }] } },
      { uuid: "dead", parentUuid: "u1", type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "abandoned branch" }] } },
      { type: "last-prompt", leafUuid: "a2" },
    ];
    await withJsonl(".claude/projects/-work-demo/s.jsonl", lines, async (path) => {
      const { project, events } = await loadTranscriptEvents(path, "claude");
      expect(project).toBe("/work/demo");
      expect(events.map((event) => [event.index, event.ref])).toEqual([
        [0, { line: 1 }], [1, { line: 2, block: 0 }], [2, { line: 2, block: 1 }], [3, { line: 2, block: 2 }], [4, { line: 3, block: 0 }], [5, { line: 4, block: 0 }],
      ]);
      expect(bare(events)).toEqual([
        { kind: "user", text: "hello", timestamp: "2026-08-01T08:00:00Z" },
        { kind: "thinking", text: "plan", timestamp: "2026-08-01T08:00:01Z" },
        { kind: "assistant", text: "looking", timestamp: "2026-08-01T08:00:01Z" },
        { kind: "tool_call", name: "Bash", input: { command: "ls" }, callId: "t1", timestamp: "2026-08-01T08:00:01Z" },
        { kind: "tool_result", name: "Bash", callId: "t1", output: "a\nb", isError: false, timestamp: undefined },
        { kind: "assistant", text: "done", timestamp: undefined },
      ]);
    });
  });

  test("codex: reads function, custom, and shell calls with their outputs", async () => {
    const lines = [
      { type: "session_meta", payload: { cwd: "/work/codex" } },
      { type: "response_item", timestamp: "2026-08-02T09:00:00Z", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "fix it" }] } },
      { type: "response_item", payload: { type: "message", role: "developer", content: [{ type: "input_text", text: "system stuff" }] } },
      { type: "response_item", payload: { type: "reasoning", summary: [{ type: "summary_text", text: "**Thinking**" }] } },
      { type: "response_item", payload: { type: "function_call", name: "wait", arguments: "{\"ms\":5}", call_id: "c1" } },
      { type: "response_item", payload: { type: "function_call_output", call_id: "c1", output: "ok" } },
      { type: "response_item", payload: { type: "custom_tool_call", name: "exec", input: "ls", call_id: "c2" } },
      { type: "response_item", payload: { type: "custom_tool_call_output", call_id: "c2", output: [{ type: "input_text", text: "Script failed\n" }, { type: "input_text", text: "boom" }] } },
      { type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "fixed" }] } },
      { type: "event_msg", payload: { type: "token_count" } },
    ];
    await withJsonl(".codex/sessions/2026/08/02/rollout.jsonl", lines, async (path) => {
      const { project, events } = await loadTranscriptEvents(path, "codex");
      expect(project).toBe("/work/codex");
      expect(events.map((event) => event.kind)).toEqual(["user", "thinking", "tool_call", "tool_result", "tool_call", "tool_result", "assistant"]);
      expect(events[2]).toMatchObject({ name: "wait", input: { ms: 5 }, callId: "c1" });
      expect(events[3]).toMatchObject({ name: "wait", output: "ok", isError: false });
      expect(events[4]).toMatchObject({ name: "exec", input: "ls" });
      expect(events[5]).toMatchObject({ name: "exec", output: "Script failed\n\nboom", isError: true });
    });
  });

  test("pi: reads toolCall blocks and toolResult messages", async () => {
    const lines = [
      { type: "session", id: "s" },
      { type: "message", id: "m1", parentId: null, timestamp: "2026-08-03T10:00:00Z", message: { role: "user", content: [{ type: "text", text: "hi" }] } },
      { type: "message", id: "m2", parentId: "m1", message: { role: "assistant", content: [{ type: "thinking", thinking: "hm" }, { type: "toolCall", id: "call1", name: "bash", arguments: { command: "pwd" } }] } },
      { type: "message", id: "m3", parentId: "m2", message: { role: "toolResult", toolCallId: "call1", toolName: "bash", content: [{ type: "text", text: "/work" }], isError: false } },
      { type: "message", id: "m4", parentId: "m3", message: { role: "assistant", content: [{ type: "text", text: "you are in /work" }] } },
    ];
    await withJsonl(".pi/agent/sessions/--work--/s.jsonl", lines, async (path) => {
      const { events } = await loadTranscriptEvents(path, "pi");
      expect(events[3]?.ref).toEqual({ line: 4 });
      expect(bare(events)).toEqual([
        { kind: "user", text: "hi", timestamp: "2026-08-03T10:00:00Z" },
        { kind: "thinking", text: "hm", timestamp: undefined },
        { kind: "tool_call", name: "bash", input: { command: "pwd" }, callId: "call1", timestamp: undefined },
        { kind: "tool_result", name: "bash", callId: "call1", output: "/work", isError: false, timestamp: undefined },
        { kind: "assistant", text: "you are in /work", timestamp: undefined },
      ]);
    });
  });

  test("opencode: expands tool parts into a call and a result", async () => {
    const databasePath = `/tmp/dejavu-transcript-${crypto.randomUUID()}.db`;
    const database = new Database(databasePath, { create: true });
    database.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER)");
    database.run("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)");
    database.run("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT)");
    database.run("INSERT INTO session VALUES (?, ?, ?, ?)", ["ses", "/work/oc", "OC", 1]);
    database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["m1", "ses", 1, JSON.stringify({ role: "user", time: { created: Date.UTC(2026, 7, 4) } })]);
    database.run("INSERT INTO message VALUES (?, ?, ?, ?)", ["m2", "ses", 2, JSON.stringify({ role: "assistant" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p1", "m1", "ses", 1, JSON.stringify({ type: "text", text: "question" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p2", "m2", "ses", 2, JSON.stringify({ type: "reasoning", text: "why" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p3", "m2", "ses", 3, JSON.stringify({ type: "tool", tool: "grep", callID: "g1", state: { status: "completed", input: { pattern: "x" }, output: "1 match" } })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p4", "m2", "ses", 4, JSON.stringify({ type: "tool", tool: "read", callID: "g2", state: { status: "error", input: { path: "/nope" }, error: "ENOENT" } })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p5", "m2", "ses", 5, JSON.stringify({ type: "step-finish" })]);
    database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?)", ["p6", "m2", "ses", 6, JSON.stringify({ type: "text", text: "answer" })]);
    database.close();
    try {
      const { project, events } = await loadTranscriptEvents(openCodeLocator(databasePath, "ses"), "opencode");
      expect(project).toBe("/work/oc");
      expect(events[2]?.ref).toEqual({ partId: "p3", messageId: "m2" });
      expect(bare(events)).toEqual([
        { kind: "user", text: "question", timestamp: "2026-08-04T00:00:00.000Z" },
        { kind: "thinking", text: "why", timestamp: undefined },
        { kind: "tool_call", name: "grep", input: { pattern: "x" }, callId: "g1", timestamp: undefined },
        { kind: "tool_result", name: "grep", callId: "g1", output: "1 match", isError: false, timestamp: undefined },
        { kind: "tool_call", name: "read", input: { path: "/nope" }, callId: "g2", timestamp: undefined },
        { kind: "tool_result", name: "read", callId: "g2", output: "ENOENT", isError: true, timestamp: undefined },
        { kind: "assistant", text: "answer", timestamp: undefined },
      ]);
    } finally {
      await rm(databasePath, { force: true });
    }
  });
});

describe("viewTranscript", () => {
  const events = [
    { kind: "user" as const, text: "q" },
    { kind: "thinking" as const, text: "t" },
    { kind: "tool_call" as const, name: "Bash", input: { command: "ls -la" } },
    { kind: "tool_result" as const, name: "Bash", output: Array.from({ length: 20 }, (_, i) => `line ${i}`).join("\n"), isError: false },
    { kind: "assistant" as const, text: "a" },
  ];
  const deps = { detectSource: () => "claude" as const, loadEvents: async () => ({ project: "p", events }) };

  test("hides thinking by default, includes it on request, and can drop tools", async () => {
    const byDefault = await viewTranscript("x", {}, deps);
    expect(byDefault.events.map((event) => event.kind)).toEqual(["user", "tool_call", "tool_result", "assistant"]);
    expect(byDefault.counts).toEqual({ user: 1, assistant: 1, thinking: 1, toolCalls: 1, toolResults: 1 });
    const withThinking = await viewTranscript("x", { thinking: true, tools: false }, deps);
    expect(withThinking.events.map((event) => event.kind)).toEqual(["user", "thinking", "assistant"]);
  });

  test("renders labeled turns, tool calls, and truncated results", async () => {
    const view = await viewTranscript("x", {}, deps);
    const text = renderTranscript(view);
    expect(text).toContain("USER");
    expect(text).toContain("ASSISTANT");
    expect(text).toContain("▶ Bash ls -la");
    expect(text).toContain("◀ Bash result");
    expect(text).toContain("[... 20 lines,");
    expect(renderTranscript(view, { full: true })).toContain("line 19");
    expect(renderTranscript(view, { full: true })).not.toContain("[...");
  });
});
