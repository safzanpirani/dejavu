import { expect, test } from "bun:test";
import { countEvents, type TranscriptEvent, type TranscriptView } from "../src/transcript-view.ts";
import { eventBody, windowTranscript } from "../src/transcript-window.ts";

function view(events: TranscriptEvent[]): TranscriptView {
  return { source: "claude", path: "/fixture", project: "/project", counts: countEvents(events), events };
}

test("pagination follows stable IDs after filtering and can recover the complete clipped event", () => {
  const original = view([
    { kind: "user", index: 0, text: "hello" },
    { kind: "assistant", index: 4, text: "a long answer", ref: { line: 3 } },
    { kind: "user", index: 8, text: "next" },
  ]);
  const page = windowTranscript(original, { fromEvent: 1, limit: 1, maxChars: 4 });
  expect(page.events).toEqual([{ kind: "assistant", index: 4, text: "a l…", ref: { line: 3 } }]);
  expect(page.window.nextEvent).toBe(8);
  expect(page.window.clipped[0]).toMatchObject({ index: 4, field: "text", originalChars: 13, returnedChars: 4 });
  expect(windowTranscript(original, { fromEvent: 4, limit: 1 }).events[0]).toEqual(original.events[1]);
  expect(windowTranscript(original, { fromEvent: 8 }).window.nextEvent).toBeNull();
  expect(windowTranscript(original, { fromEvent: 99 }).events).toEqual([]);
});

test("tool caps affect only tool bodies, mark input previews, and leave source objects intact", () => {
  const input = { command: "run a long command" };
  const original = view([
    { kind: "user", index: 0, text: "unlimited dialogue" },
    { kind: "tool_call", index: 1, name: "shell", input },
    { kind: "tool_result", index: 2, output: "long output", isError: false },
  ]);
  const page = windowTranscript(original, { toolChars: 6 });
  expect(eventBody(page.events[0]!)).toBe("unlimited dialogue");
  expect(eventBody(page.events[1]!)).toHaveLength(6);
  expect(eventBody(page.events[2]!)).toHaveLength(6);
  expect(page.window.clipped.map((item) => item.field)).toEqual(["input", "output"]);
  expect(original.events[1]).toMatchObject({ input });
  expect(windowTranscript(original).events).toEqual(original.events);
});

test("tiny budgets include markers in the cap and always advance", () => {
  const original = view([{ kind: "user", index: 3, text: "large" }, { kind: "assistant", index: 7, text: "other" }]);
  const first = windowTranscript(original, { budgetChars: 1 });
  expect(first.window.usedChars).toBe(1);
  expect(first.window.nextEvent).toBe(7);
  expect(eventBody(first.events[0]!)).toBe("…");
  expect(windowTranscript(original, { fromEvent: 7, budgetChars: 1 }).window.nextEvent).toBeNull();
});

test("invalid bounds fail before processing and zero is valid only for offsets", () => {
  const original = view([]);
  for (const value of [0, -1, 1.5, Infinity, NaN, Number.MAX_SAFE_INTEGER + 1]) {
    expect(() => windowTranscript(original, { budgetChars: value })).toThrow();
  }
  expect(windowTranscript(original, { fromEvent: 0 }).events).toEqual([]);
});
