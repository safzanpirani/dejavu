import { expect, test } from "bun:test";
import type { FindHit, FindResult } from "../src/find.ts";
import { packSessions } from "../src/pack.ts";
import { countEvents, type TranscriptEvent, type TranscriptView } from "../src/transcript-view.ts";
import { eventBody } from "../src/transcript-window.ts";

const hit = (path: string): FindHit => ({ path, project: "/project", source: "claude", date: "2026-09-08", score: 1, termCounts: {}, openingPrompt: "", matches: [] });
const found = (paths: string[]): FindResult => ({ terms: ["needle"], requiredTerms: ["needle"], hits: paths.map(hit), sources: ["claude"], skippedStores: [], elapsedMs: 0, storeTimings: {} });
const view = (path: string, events: TranscriptEvent[]): TranscriptView => ({ path, project: "/project", source: "claude", counts: countEvents(events), events });

test("pack deduplicates neighbors and keeps late matches visible within a shared budget", async () => {
  const result = await packSessions(["needle"], { budgetChars: 300, maxChars: 100, context: 1 }, {
    find: async () => found(["/a", "/b"]),
    view: async (path, options) => {
      expect(options?.tools).toBe(false);
      return view(path, [
        { kind: "user", index: 0, text: "x".repeat(3000) },
        { kind: "assistant", index: 3, text: `${"y".repeat(3000)} needle decision` },
        { kind: "user", index: 6, text: "needle followup" },
        { kind: "assistant", index: 9, text: "done" },
      ]);
    },
  });
  expect(result.sessions).toHaveLength(2);
  expect(result.usedChars).toBeLessThanOrEqual(300);
  for (const session of result.sessions) {
    expect(session.events.map((event) => event.index)).toEqual([0, 3, 6, 9]);
    expect(eventBody(session.events[1]!)).toContain("needle");
    expect(session.window.clipped.find((event) => event.index === 3)?.startChar).toBeGreaterThan(0);
  }
  expect(result.usedChars).toBe(result.sessions.flatMap((session) => session.events).reduce((sum, event) => sum + eventBody(event).length, 0));
});

test("exclusions happen before the session limit and unreadable or stale candidates do not prevent useful excerpts", async () => {
  const result = await packSessions(["needle"], { limit: 1, context: 0, excludeSessions: ["exclude"] }, {
    find: async () => found(["/exclude.jsonl", "/broken", "/stale", "/good"]),
    view: async (path) => {
      if (path === "/exclude.jsonl") throw new Error("excluded session was loaded");
      if (path === "/broken") throw new Error("unreadable");
      return view(path, [{ kind: "user", index: 9, text: path === "/good" ? "needle" : "another branch" }]);
    },
  });
  expect(result.excludedCount).toBe(1);
  expect(result.skippedSessions).toEqual([{ path: "/broken", error: "unreadable" }]);
  expect(result.sessions.map((session) => session.path)).toEqual(["/good"]);
  expect(result.sessions[0]!.events.map((event) => event.index)).toEqual([9]);
});

test("empty results and relaxed terms remain explicit", async () => {
  const result = await packSessions(["needle", "missing"], {}, {
    find: async () => ({ ...found([]), terms: ["needle", "missing"] }),
    view: async () => { throw new Error("should not load"); },
  });
  expect(result.sessions).toEqual([]);
  expect(result.requiredTerms).toEqual(["needle"]);
  expect(result.usedChars).toBe(0);
});
