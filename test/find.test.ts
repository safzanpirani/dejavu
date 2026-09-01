import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { findSessions, parseSince, resumeCommand, showSession } from "../src/find.ts";

describe("parseSince", () => {
  test("passes absolute dates through", () => {
    expect(parseSince("2026-08-01")).toBe("2026-08-01");
  });
  test("resolves relative windows", () => {
    const today = new Date("2026-08-28T12:00:00Z");
    expect(parseSince("7d", today)).toBe("2026-08-21");
    expect(parseSince("2w", today)).toBe("2026-08-14");
  });
  test("rejects malformed values", () => {
    expect(() => parseSince("yesterday")).toThrow("--since");
  });
});

describe("resumeCommand", () => {
  test("claude uuid filenames", () => {
    expect(resumeCommand("claude", "/x/-proj/2d695265-2734-49cd-be54-3ac3d6480ce9.jsonl", "proj"))
      .toBe("claude --resume 2d695265-2734-49cd-be54-3ac3d6480ce9");
  });
  test("codex rollout filenames", () => {
    expect(resumeCommand("codex", "/x/rollout-2026-08-21T00-49-55-01a0209d-9c33-7e43-95f2-458ff4fdad0f.jsonl", "proj"))
      .toBe("codex resume 01a0209d-9c33-7e43-95f2-458ff4fdad0f");
  });
  test("pi resumes by session file", () => {
    expect(resumeCommand("pi", "/x/2026-05-01T17-00-53-250Z_x.jsonl", "proj")).toBe("pi --session /x/2026-05-01T17-00-53-250Z_x.jsonl");
  });
  test("opencode has no resume command", () => {
    expect(resumeCommand("opencode", "opencode:///x/opencode.db#ses_1", "proj")).toBeUndefined();
  });
});

function claudeLine(role: string, text: string, timestamp = "2026-08-25T08:00:00Z"): string {
  return JSON.stringify({ uuid: "u", message: { role, content: [{ type: "text", text }] }, timestamp });
}

describe("findSessions", () => {
  const storeDir = "/fake/.claude/projects";
  const pathA = `${storeDir}/-Users-me-Development-alpha/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl`;
  const pathB = `${storeDir}/-Users-me-Development-beta/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl`;
  const deps = {
    discoverStores: async () => [{ source: "claude" as const, kind: "jsonl" as const, path: storeDir }],
    countFiles: async (term: string) => {
      if (term === "workshop") return [{ path: pathA, count: 10 }, { path: pathB, count: 500 }];
      if (term === "codex") return [{ path: pathA, count: 20 }];
      return [];
    },
    findLines: async (term: string, path: string) => {
      if (path === pathA) return [claudeLine("user", `planning the ${term} session`)];
      return [claudeLine("assistant", `${term} `.repeat(3))];
    },
    readProject: async () => "Development/alpha",
    readPrefix: async () => claudeLine("user", "let us plan the workshop"),
    now: () => 0,
  };

  test("bounds reverse-completing candidate scans and preserves candidate order", async () => {
    const paths = Array.from({ length: 6 }, (_, index) => `${storeDir}/-Users-me-Development-p${index}/${index}aaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl`);
    const completions: number[] = [];
    const delays = [120, 90, 60, 30, 10, 5];
    let active = 0;
    let peak = 0;
    const result = await findSessions(["workshop"], { limit: 6 }, {
      discoverStores: async () => [{ source: "claude", kind: "jsonl", path: storeDir }],
      countFiles: async () => paths.map((path) => ({ path, count: 1 })),
      findLines: async (_term, path) => {
        const index = paths.indexOf(path);
        active++;
        peak = Math.max(peak, active);
        await Bun.sleep(delays[index]!);
        completions.push(index);
        active--;
        return [claudeLine("user", "planning the workshop session")];
      },
      readProject: async (path) => `Development/p${paths.indexOf(path)}`,
      readPrefix: async () => claudeLine("user", "open the workshop"),
      now: () => 0,
    });

    expect(completions).toEqual([3, 4, 5, 2, 1, 0]);
    expect(peak).toBe(4);
    expect(result.hits.map((hit) => hit.path)).toEqual(paths);
  });

  test("skips an unreadable OpenCode store and keeps readable candidates", async () => {
    const runFind = (maxParallel: number) => findSessions(["workshop"], { maxParallel }, {
      ...deps,
      discoverStores: async () => [
        { source: "opencode", kind: "sqlite", path: "/unreadable.db" },
        { source: "claude", kind: "jsonl", path: storeDir },
      ],
      searchOpenCode: async () => { throw new Error("unable to open database file"); },
    });
    const result = await runFind(4);

    expect(result.hits[0]?.path).toBe(pathA);
    expect(result.sources).toEqual(["claude"]);
    expect(result.skippedStores).toEqual([{
      source: "opencode", path: "/unreadable.db", error: "unable to open database file",
    }]);
    expect(await runFind(1)).toEqual(result);
  });

  test("AND across terms wins over one-term spam", async () => {
    const result = await findSessions(["workshop", "codex"], {}, deps);
    expect(result.hits[0]?.path).toBe(pathA);
    expect(result.requiredTerms).toEqual(["workshop", "codex"]);
    expect(result.hits[0]?.openingPrompt).toBe("let us plan the workshop");
    expect(result.hits[0]?.resume).toBe("claude --resume aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
  });

  test("falls back to fewer terms when nothing matches all", async () => {
    const result = await findSessions(["zzznope", "workshop"], {}, deps);
    expect(result.requiredTerms).toEqual(["workshop"]);
    expect(result.hits.length).toBeGreaterThan(0);
  });

  test("deduplicates terms case-insensitively", async () => {
    const result = await findSessions(["workshop", "WORKSHOP"], {}, deps);
    expect(result.terms).toEqual(["workshop"]);
    expect(result.requiredTerms).toEqual(["workshop"]);
  });

  test("falls back when a raw match is not visible in a message", async () => {
    const result = await findSessions(["workshop", "metadata"], {}, {
      ...deps,
      countFiles: async () => [{ path: pathA, count: 1 }],
      findLines: async (term: string) => term === "workshop"
        ? [claudeLine("user", "planning the workshop session")]
        : [],
    });
    expect(result.requiredTerms).toEqual(["workshop"]);
    expect(result.hits).toHaveLength(1);
  });

  test("rejects an invalid programmatic limit", async () => {
    expect(findSessions(["workshop"], { limit: 0 }, deps)).rejects.toThrow("integer >= 1");
  });

  test("--user drops sessions without user-message matches for every term", async () => {
    const result = await findSessions(["workshop"], { userOnly: true }, deps);
    expect(result.hits.every((hit) => Object.values(hit.termCounts).every((count) => count.user > 0))).toBe(true);
  });

  test("project filter applies before the candidate cap", async () => {
    const result = await findSessions(["workshop"], { project: "beta" }, deps);
    expect(result.hits.every((hit) => hit.path === pathB)).toBe(true);
  });
});

async function withClaudeTranscript(
  messages: Array<{ role: "user" | "assistant"; text: string }>,
  run: (path: string) => Promise<void>,
): Promise<void> {
  const temporaryRoot = await mkdtemp(join(tmpdir(), "dejavu-show-"));
  const transcriptDir = join(temporaryRoot, ".claude", "projects", "-tmp-project");
  const transcriptPath = join(transcriptDir, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
  await mkdir(transcriptDir, { recursive: true });
  const lines = messages.map((message, index) => JSON.stringify({
    uuid: `message-${index}`,
    parentUuid: index === 0 ? null : `message-${index - 1}`,
    message: { role: message.role, content: [{ type: "text", text: message.text }] },
    timestamp: `2026-08-${String(index + 1).padStart(2, "0")}T08:00:00Z`,
  }));
  await writeFile(transcriptPath, `${lines.join("\n")}\n`);
  try {
    await run(transcriptPath);
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
}

describe("showSession", () => {
  test("places leading and trailing markers around one real-file window", async () => {
    const messages = Array.from({ length: 10 }, (_, index) => ({
      role: index % 2 === 0 ? "user" as const : "assistant" as const,
      text: index === 5 ? "message 5 target" : `message ${index}`,
    }));
    await withClaudeTranscript(messages, async (path) => {
      const result = await showSession(path, { around: "target" });
      expect(result.messages.map((message) => message.text)).toEqual([
        "[messages omitted]",
        "message 2",
        "message 3",
        "message 4",
        "message 5 target",
        "message 6",
        "message 7",
        "message 8",
        "[messages omitted]",
      ]);
    });
  });

  test("places an internal marker between disjoint real-file windows", async () => {
    const messages = Array.from({ length: 13 }, (_, index) => ({
      role: index % 2 === 0 ? "user" as const : "assistant" as const,
      text: index === 2 || index === 10 ? `message ${index} target` : `message ${index}`,
    }));
    await withClaudeTranscript(messages, async (path) => {
      const result = await showSession(path, { around: "target" });
      expect(result.messages.map((message) => message.text)).toEqual([
        "message 0",
        "message 1",
        "message 2 target",
        "message 3",
        "message 4",
        "message 5",
        "[messages omitted]",
        "message 7",
        "message 8",
        "message 9",
        "message 10 target",
        "message 11",
        "message 12",
      ]);
    });
  });
});
