import { describe, expect, test } from "bun:test";
import { parseCountOutput, searchFileCounts } from "../src/search-backend.ts";
import { findContextWindow } from "../src/model-client.ts";

describe("search backend", () => {
  test("parses paths containing colons from count output", () => {
    expect(parseCountOutput("C:\\sessions\\one.jsonl:12\n/tmp/two.jsonl:2\n")).toEqual([
      { path: "C:\\sessions\\one.jsonl", count: 12 },
      { path: "/tmp/two.jsonl", count: 2 },
    ]);
  });

  test("uses node fallback when rg and grep are unavailable", async () => {
    const result = await searchFileCounts("needle", "/sessions", {
      run: async () => ({ exitCode: 127, stdout: "", stderr: "missing" }),
      glob: async () => ["/sessions/a.jsonl", "/sessions/b.jsonl"],
      readText: async (path) => path.endsWith("a.jsonl") ? "Needle needle none" : "none",
    });
    expect(result).toEqual([{ path: "/sessions/a.jsonl", count: 2 }]);
  });

  test("uses Pi's configured model context and a conservative fallback", () => {
    const models = { providers: { deepseek: { models: [{ id: "flash", contextWindow: 1_000_000 }] } } };
    expect(findContextWindow(models, "deepseek", "flash")).toBe(1_000_000);
    expect(findContextWindow(models, "anthropic", "unknown")).toBe(128_000);
  });
});
