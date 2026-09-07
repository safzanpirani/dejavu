import { expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const cli = new URL("../src/cli.ts", import.meta.url).pathname;
async function run(args: string[]) {
  const process = Bun.spawn([Bun.which("bun")!, cli, ...args], { stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, code] = await Promise.all([new Response(process.stdout).text(), new Response(process.stderr).text(), process.exited]);
  return { stdout, stderr, code };
}

test("CLI removes tool-only turns and applies explicit transcript JSON bounds with resumable IDs", async () => {
  const root = await mkdtemp(join(tmpdir(), "dejavu-bounds-"));
  try {
    const directory = join(root, ".claude/projects/demo");
    await mkdir(directory, { recursive: true });
    const path = join(directory, "fixture.jsonl");
    const messages = [
      { role: "user", content: "hello needle" },
      { role: "assistant", content: [{ type: "tool_use", id: "t1", name: "shell", input: { command: "echo synthetic" } }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "t1", content: "synthetic result" }] },
      { role: "assistant", content: [{ type: "text", text: "answer" }, { type: "tool_use", id: "t2", name: "shell", input: {} }] },
      { role: "assistant", content: "done" },
    ];
    await writeFile(path, messages.map((message, index) => JSON.stringify({ type: message.role, uuid: `m${index}`, parentUuid: index ? `m${index - 1}` : null, message })).join("\n") + "\n");
    const clean = await run(["show", path, "--no-toolcalls", "--around", "needle", "--json"]);
    expect(clean.code).toBe(0);
    expect(JSON.parse(clean.stdout).messages).toEqual([{ role: "user", text: "hello needle" }, { role: "assistant", text: "answer" }, { role: "assistant", text: "done" }]);
    const page = await run(["transcript", path, "--no-tools", "--from-event", "1", "--limit", "1", "--max-chars", "3", "--json"]);
    expect(page.code).toBe(0);
    expect(JSON.parse(page.stdout)).toMatchObject({ events: [{ kind: "assistant", index: 3, text: "an…" }], window: { nextEvent: 5, usedChars: 3 } });
    const full = await run(["transcript", path, "--full", "--from-event", "3", "--limit", "1", "--json"]);
    expect(JSON.parse(full.stdout).events[0].text).toBe("answer");
    const cap = await run(["show", path, "--max-chars", "3", "--json"]);
    expect(JSON.parse(cap.stdout).messages[0].text).toBe("hel [...]");
    for (const flags of [["--budget-chars=0"], ["--max-chars="], ["--from-event=-1"], ["--full", "--tool-chars=3"]]) {
      const invalid = await run(["transcript", path, ...flags]);
      expect(invalid.code).toBe(1);
      expect(invalid.stdout).toBe("");
    }
  } finally { await rm(root, { recursive: true, force: true }); }
});
