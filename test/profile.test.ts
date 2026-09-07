import { describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { explainProfile, measureTranscript, nestedCallSites, profileSessions, type ProfileReport } from "../src/profile.ts";
import { listIndexedSessions, refreshTranscriptIndex } from "../src/transcript-index.ts";
import { countEvents, type TranscriptEvent } from "../src/transcript-view.ts";

const events: TranscriptEvent[] = [
  { index: 5, kind: "tool_call", name: "Read", callId: "a", input: { file: "private-file", limit: 5 }, timestamp: "2026-09-08T10:00:00Z" },
  { index: 6, kind: "tool_result", callId: "a", output: "private transcript data", isError: false, timestamp: "2026-09-08T10:00:02Z" },
  { index: 8, kind: "tool_call", name: "Read", callId: "b", input: { limit: 5, file: "private-file" } },
  { index: 9, kind: "tool_result", callId: "b", output: "error", isError: true },
  { index: 10, kind: "tool_result", callId: "b", output: "second chunk", isError: false },
  { index: 11, kind: "tool_result", callId: "unknown", output: "orphan", isError: false },
  { index: 12, kind: "tool_call", name: "functions.exec", input: 'await tools.write_stdin({session_id: 1}); await tools.exec_command({cmd: "private shell"})' },
  { index: 13, kind: "tool_call", name: "functions.wait", input: { cell_id: "x" } },
];
const view = { path: "/test/session.jsonl", source: "claude" as const, project: "example", events, counts: countEvents(events) };
const measured = () => measureTranscript(view, 10);
const report = (): ProfileReport => ({ version: 1, sessions: [measured()], oversizedThreshold: 10, matchedSessions: 1, omittedSessions: 0, diagnostics: [], limitations: [] });

describe("profile measurements", () => {
  test("matches all result chunks, preserves event IDs, and reports uncertainty separately", () => {
    const result = measured();
    expect(result.metrics).toMatchObject({ toolCalls: 4, toolResults: 4, repeatedCalls: 1, errorFlaggedResults: 1, unmatchedResults: 1, callsWithoutResults: 2, oversizedResults: 2, observationCalls: 1, wrapperCalls: 1, nestedCallSites: 2 });
    expect(result.calls[0]).toMatchObject({ eventId: 5, firstResultLatencyMs: 2000, resultEventIds: [6] });
    expect(result.calls[1]).toMatchObject({ eventId: 8, repeatOf: 5, resultEventIds: [9, 10], errorFlagged: true, outputCharacters: 17 });
    expect(result.repeats).toEqual([{ tool: "Read", eventIds: [5, 8] }]);
    expect(JSON.stringify(result)).not.toContain("private-file");
    expect(JSON.stringify(result)).not.toContain("private transcript");
    expect(JSON.stringify(result)).not.toContain("private shell");
  });

  test("does not mistake comments or string examples for nested call sites", () => {
    expect(nestedCallSites('/* tools.fake() */ const x = "tools.nope()"; // tools.no()\n await tools.real({cmd: "a"}); `tools.template()`; /tools.regex()/;')).toEqual(["real"]);
    expect(nestedCallSites({ command: "tools.notCode()" })).toEqual([]);
  });

  test("invalid options fail before reading transcript stores", async () => {
    await expect(profileSessions([], { limit: 0 })).rejects.toThrow("limit");
    await expect(profileSessions([], { threshold: -1 })).rejects.toThrow("threshold");
    await expect(profileSessions(["x"], { project: "y" })).rejects.toThrow("not both");
  });
});

describe("profile explanations", () => {
  test("sends bounded measurements without raw context and validates evidence references", async () => {
    let prompt = "";
    const result = await explainProfile(report(), undefined, async (model, text) => {
      expect(model).toBe("gpt-5.6-luna"); prompt = text;
      return { answer: JSON.stringify({ observations: [{ summary: "Review the repeated read before deciding it is redundant.", evidence: ["S1#8"] }] }), transport: "codex" };
    });
    expect(result.observations?.length).toBe(1);
    expect(prompt).not.toContain("private-file");
    expect(prompt).not.toContain("private transcript");
    expect(prompt.length).toBeLessThan(16000);
  });

  test("keeps model errors separate and rejects invented event IDs", async () => {
    const result = await explainProfile(report(), undefined, async () => ({ answer: '{"observations":[{"summary":"Unsupported","evidence":["S1#999"]}]}', transport: "codex" }));
    expect(result.error).toContain("unknown or missing evidence");
    expect(report().sessions[0]!.metrics.toolCalls).toBe(4);
  });
});

test("project selection reports limits and filters visible-message dates", async () => {
  const root = await mkdtemp(join(tmpdir(), "dejavu-profile-index-"));
  const store = join(root, "claude");
  const index = join(root, "index.sqlite");
  await mkdir(store);
  try {
    for (const [name, day, project] of [["old", "01", "/work/reel"], ["new", "08", "/work/reel"], ["other", "09", "/work/other"]]) {
      await writeFile(join(store, `${name}.jsonl`), JSON.stringify({ type: "user", timestamp: `2026-09-${day}T10:00:00Z`, cwd: project, message: { role: "user", content: "test context" } }));
    }
    await refreshTranscriptIndex([{ source: "claude", kind: "jsonl", path: store }], index);
    const all = listIndexedSessions({ project: "reel", limit: 1 }, index);
    expect(all.total).toBe(2);
    expect(all.paths).toEqual([join(store, "new.jsonl")]);
    expect(listIndexedSessions({ project: "reel", since: "2026-09-05", limit: 10 }, index).total).toBe(1);
  } finally { await rm(root, { recursive: true, force: true }); }
});
