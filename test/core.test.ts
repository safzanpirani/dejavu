import { describe, expect, test } from "bun:test";
import {
  buildWindowedContext,
  prepareRecallMessages,
  projectFromClaudePath,
  projectFromPiPath,
  querySession,
  searchSessions,
} from "../src/core.ts";
import { extractVisibleMessage, loadRecallMessages } from "../src/session-reader.ts";
import type { RecallMessage, StoreSearchMatch, TranscriptStore } from "../src/transcript-types.ts";

describe("multi-source search", () => {
  test("bounds reverse-completing store and file work while preserving store order", async () => {
    const stores: TranscriptStore[] = Array.from({ length: 6 }, (_, index) => ({
      source: "claude" as const,
      kind: "jsonl" as const,
      path: `/store-${index}`,
    }));
    const storeCompletions: number[] = [];
    const fileCompletions: number[] = [];
    let activeStores = 0;
    let activeFiles = 0;
    let peakStores = 0;
    let peakFiles = 0;
    const delays = [120, 90, 60, 30, 10, 5];
    const result = await searchSessions("Needle", { limit: 6 }, {
      discoverStores: async () => stores,
      countFiles: async (_query, root) => {
        const index = Number(root.split("-").at(-1));
        activeStores++;
        peakStores = Math.max(peakStores, activeStores);
        await Bun.sleep(delays[index]!);
        storeCompletions.push(index);
        activeStores--;
        return [{ path: `${root}/session.jsonl`, count: 1 }];
      },
      findLines: async (_query, path) => {
        const index = Number(path.match(/store-(\d+)/)?.[1]);
        activeFiles++;
        peakFiles = Math.max(peakFiles, activeFiles);
        await Bun.sleep(delays[index]!);
        fileCompletions.push(index);
        activeFiles--;
        return [JSON.stringify({ type: "user", timestamp: "2026-08-01T00:00:00Z", message: { role: "user", content: [{ type: "text", text: "Needle" }] } })];
      },
      readProject: async () => "project",
      now: () => 0,
    });

    expect(storeCompletions).toEqual([3, 4, 5, 2, 1, 0]);
    expect(fileCompletions).toEqual([3, 4, 5, 2, 1, 0]);
    expect(peakStores).toBe(4);
    expect(peakFiles).toBe(4);
    expect(result.matches.map((match) => match.path)).toEqual(stores.map((store) => `${store.path}/session.jsonl`));
  });

  test("merges and ranks file and SQLite stores", async () => {
    const stores: TranscriptStore[] = [
      { source: "claude", kind: "jsonl", path: "/claude" },
      { source: "pi", kind: "jsonl", path: "/pi" },
      { source: "opencode", kind: "sqlite", path: "/opencode.db" },
    ];
    const openCode: StoreSearchMatch = {
      source: "opencode", path: "opencode:///opencode.db#ses", count: 7,
      date: "2026-08-03", project: "/work/open", snippets: [{ role: "user", text: "Needle" }],
    };
    const result = await searchSessions("Needle", { limit: 2 }, {
      discoverStores: async () => stores,
      countFiles: async (_query, root) => root === "/claude"
        ? [{ path: "/claude/project/a.jsonl", count: 2 }]
        : [{ path: "/pi/project/b.jsonl", count: 5 }],
      findLines: async (_query, path) => path.includes("claude")
        ? [JSON.stringify({ type: "assistant", timestamp: "2026-08-01T00:00:00Z", cwd: "/work/claude", message: { role: "assistant", content: [{ type: "text", text: "Needle in Claude" }] } })]
        : [JSON.stringify({ type: "message", timestamp: "2026-08-02T00:00:00Z", message: { role: "user", content: [{ type: "text", text: "Needle in Pi" }] } })],
      searchOpenCode: async () => [openCode],
      readProject: async () => "fallback",
      now: () => 10,
    });
    expect(result.sources).toEqual(["claude", "pi", "opencode"]);
    expect(result.matches.map((match) => [match.source, match.count])).toEqual([
      ["opencode", 7], ["pi", 5],
    ]);
  });

  test("skips an unreadable OpenCode store and searches the readable stores", async () => {
    const readableMatch: StoreSearchMatch = {
      source: "opencode", path: "opencode:///readable.db#ses", count: 2,
      date: "2026-08-04", project: "/work/readable", snippets: [{ role: "user", text: "Needle" }],
    };
    const runSearch = (maxParallel: number) => searchSessions("Needle", { source: "opencode", maxParallel }, {
      discoverStores: async () => [
        { source: "opencode", kind: "sqlite", path: "/unreadable.db" },
        { source: "opencode", kind: "sqlite", path: "/readable.db" },
      ],
      searchOpenCode: async (_query, path) => {
        if (path === "/unreadable.db") throw new Error("unable to open database file");
        return [readableMatch];
      },
      now: () => 0,
    });
    const result = await runSearch(4);

    expect(result.matches).toEqual([readableMatch]);
    expect(result.sources).toEqual(["opencode"]);
    expect(result.skippedStores).toEqual([{
      source: "opencode", path: "/unreadable.db", error: "unable to open database file",
    }]);
    expect(await runSearch(1)).toEqual(result);
  });

  test("rejects an empty literal before discovering stores", async () => {
    await expect(searchSessions("  ", {}, {
      discoverStores: async () => { throw new Error("should not run"); },
    })).rejects.toThrow("must not be empty");
  });
});

describe("visible message extraction", () => {
  test("reads Pi messages without thinking or tool output", () => {
    const line = JSON.stringify({ type: "message", message: { role: "assistant", content: [
      { type: "thinking", thinking: "hidden" }, { type: "text", text: "visible" },
    ] } });
    expect(extractVisibleMessage(line, "pi")).toMatchObject({ role: "assistant", text: "visible" });
  });

  test("reads Claude user and assistant envelopes", () => {
    const line = JSON.stringify({ type: "user", cwd: "/work", message: { role: "user", content: [{ type: "text", text: "Claude text" }] } });
    expect(extractVisibleMessage(line, "claude")).toMatchObject({ role: "user", text: "Claude text", project: "/work" });
  });

  test("reads Codex response messages but ignores developer context", () => {
    const user = JSON.stringify({ type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "Codex text" }] } });
    const developer = JSON.stringify({ type: "response_item", payload: { type: "message", role: "developer", content: [{ type: "input_text", text: "rules" }] } });
    expect(extractVisibleMessage(user, "codex")).toMatchObject({ role: "user", text: "Codex text" });
    expect(extractVisibleMessage(developer, "codex")).toBeNull();
  });
});

describe("transcript loading", () => {
  test("loads the active Pi branch", async () => {
    const path = `/tmp/deja-pi-${crypto.randomUUID()}.jsonl`;
    await writeRows(path, [
      { type: "session", version: 3, id: "session", cwd: "/tmp" },
      message("message", "a", null, "user", "root"),
      message("message", "old", "a", "assistant", "abandoned"),
      message("message", "new", "a", "assistant", "active"),
    ]);
    const messages = await loadRecallMessages(path, "pi");
    expect(texts(messages)).toEqual(["root", "active"]);
  });

  test("loads Claude's recorded leaf branch", async () => {
    const path = `/tmp/deja-claude-${crypto.randomUUID()}.jsonl`;
    await writeRows(path, [
      claudeMessage("a", null, "user", "root"),
      claudeMessage("old", "a", "assistant", "abandoned"),
      claudeMessage("new", "a", "assistant", "active"),
      { type: "last-prompt", leafUuid: "new" },
    ]);
    expect(texts(await loadRecallMessages(path, "claude"))).toEqual(["root", "active"]);
  });

  test("loads only user and assistant Codex response items", async () => {
    const path = `/tmp/deja-codex-${crypto.randomUUID()}.jsonl`;
    await writeRows(path, [
      { type: "response_item", payload: { type: "message", role: "developer", content: [{ type: "input_text", text: "rules" }] } },
      { type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "question" }] } },
      { type: "response_item", payload: { type: "reasoning", summary: [] } },
      { type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "answer" }] } },
    ]);
    expect(texts(await loadRecallMessages(path, "codex"))).toEqual(["question", "answer"]);
  });
});

describe("query preparation", () => {
  test("drops non-conversation roles and windows around question terms", () => {
    const prepared = prepareRecallMessages([
      { role: "toolResult", content: [{ type: "text", text: "large output" }] },
      { role: "user", content: [{ type: "text", text: "question" }] },
    ]);
    expect(texts(prepared)).toEqual(["question"]);
    const messages = Array.from({ length: 12 }, (_, index) => ({
      role: index % 2 ? "assistant" : "user",
      text: index === 6 ? "the distinctive marmalade decision" : `filler ${index}`,
      charCount: 500,
    }));
    expect(buildWindowedContext(messages, "What was the marmalade decision?", 100)).toMatch(/messages? omitted/);
  });

  test("reports the detected source in query results", async () => {
    const result = await querySession("/tmp/a.jsonl", "What database?", { agentDir: "/pi" }, {
      detectSource: () => "codex",
      pathExists: async () => true,
      loadMessages: async () => [{ role: "user", content: [{ type: "text", text: "We chose SQLite." }] }],
      resolveModel: async () => ({
        provider: "test", id: "tiny", contextWindow: 1000,
        serialize: (messages) => JSON.stringify(messages), agentDir: "/pi",
      }),
      complete: async (_model, conversation, question) => `${question} ${conversation.includes("SQLite")}`,
      now: () => 20,
    });
    expect(result).toMatchObject({ source: "codex", answer: "What database? true" });
  });
});

describe("project path decoding", () => {
  test("decodes Pi and Claude project directories", () => {
    expect(projectFromPiPath(
      "/Users/dev/.pi/agent/sessions/--Users-dev-Development-projects-dejavu--/x.jsonl", "/Users/dev",
    )).toBe("Development/projects/dejavu");
    expect(projectFromClaudePath(
      "/Users/dev/.claude/projects/-Users-dev-Development-projects-dejavu/x.jsonl", "/Users/dev",
    )).toBe("Development/projects/dejavu");
  });
});

async function writeRows(path: string, rows: unknown[]): Promise<void> {
  await Bun.write(path, rows.map((row) => JSON.stringify(row)).join("\n"));
}

function message(type: string, id: string, parentId: string | null, role: string, text: string) {
  return { type, id, parentId, message: { role, content: [{ type: "text", text }] } };
}

function claudeMessage(uuid: string, parentUuid: string | null, role: string, text: string) {
  return { type: role, uuid, parentUuid, message: { role, content: [{ type: "text", text }] } };
}

function texts(messages: RecallMessage[]): Array<string | undefined> {
  return messages.map((message) => message.content[0]?.text);
}
