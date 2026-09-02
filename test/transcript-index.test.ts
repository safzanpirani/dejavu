import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  refreshTranscriptIndex,
  searchTranscriptIndex,
  searchTranscriptIndexMatches,
  transcriptIndexStatus,
} from "../src/transcript-index.ts";
import type { TranscriptStore } from "../src/transcript-types.ts";

function claudeLine(role: "user" | "assistant", text: string): string {
  return JSON.stringify({
    type: role,
    timestamp: "2026-09-02T10:00:00Z",
    cwd: "/work/dejavu",
    message: { role, content: [{ type: "text", text }] },
  });
}

describe("transcript index", () => {
  test("incrementally replaces changed files and removes deleted files", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-"));
    const storePath = join(root, "claude");
    const transcriptPath = join(storePath, "session.jsonl");
    const indexPath = join(root, "cache", "index.sqlite");
    const stores: TranscriptStore[] = [{ source: "claude", kind: "jsonl", path: storePath }];
    await mkdir(storePath, { recursive: true });
    await writeFile(transcriptPath, `${claudeLine("user", "Needle phrase appears twice: needle phrase")}`);

    try {
      const initial = await refreshTranscriptIndex(stores, indexPath);
      expect(initial).toMatchObject({ files: 1, messages: 1, indexed: 1, removed: 0 });
      expect(searchTranscriptIndex("needle phrase", ["claude"], indexPath)).toEqual([{
        source: "claude", path: transcriptPath, count: 2,
      }]);
      expect(searchTranscriptIndexMatches("needle phrase", ["claude"], indexPath, 10, 1)[0]).toMatchObject({
        source: "claude", path: transcriptPath, count: 2,
        date: "2026-09-02", project: "/work/dejavu",
        snippets: [{ role: "user" }],
      });

      const unchanged = await refreshTranscriptIndex(stores, indexPath);
      expect(unchanged.indexed).toBe(0);

      await writeFile(transcriptPath, `${claudeLine("assistant", "Replacement text only")}`);
      const changed = await refreshTranscriptIndex(stores, indexPath);
      expect(changed.indexed).toBe(1);
      expect(searchTranscriptIndex("needle phrase", ["claude"], indexPath)).toEqual([]);
      expect(searchTranscriptIndex("replacement text", ["claude"], indexPath)[0]?.count).toBe(1);

      await unlink(transcriptPath);
      const deleted = await refreshTranscriptIndex(stores, indexPath);
      expect(deleted).toMatchObject({ files: 0, messages: 0, removed: 1 });
      expect(await transcriptIndexStatus(indexPath)).toMatchObject({ exists: true, files: 0, messages: 0 });
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("verifies exact literal text after trigram candidate lookup", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-literal-"));
    const storePath = join(root, "codex");
    const transcriptPath = join(storePath, "session.jsonl");
    const indexPath = join(root, "index.sqlite");
    await mkdir(storePath, { recursive: true });
    await writeFile(transcriptPath, JSON.stringify({
      type: "response_item",
      payload: { type: "message", role: "user", content: [{ type: "input_text", text: "Alpha beta and alpha-beta" }] },
    }));
    try {
      await refreshTranscriptIndex([{ source: "codex", kind: "jsonl", path: storePath }], indexPath);
      expect(searchTranscriptIndex("alpha beta", ["codex"], indexPath)[0]?.count).toBe(1);
      expect(searchTranscriptIndex("alpha  beta", ["codex"], indexPath)).toEqual([]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
