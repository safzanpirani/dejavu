import { expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const cli = new URL("../src/cli.ts", import.meta.url).pathname;

test("memory show accepts the exact leading-hyphen key from memory list", async () => {
  const root = await mkdtemp(join(tmpdir(), "dejavu-memory-cli-"));
  try {
    const dir = join(root, "-Users-example-project", "memory");
    await mkdir(dir, { recursive: true });
    await writeFile(join(dir, "MEMORY.md"), "# Synthetic project memory");
    const list = Bun.spawn([process.execPath, cli, "memory", "list", "--root", root, "--json"]);
    const projects = await new Response(list.stdout).json() as Array<{ project: string }>;
    expect(await list.exited).toBe(0);
    const show = Bun.spawn([process.execPath, cli, "memory", "show", projects[0]!.project, "--root", root]);
    expect(await new Response(show.stdout).text()).toContain("Synthetic project memory");
    expect(await show.exited).toBe(0);
    const invalid = Bun.spawn([process.execPath, cli, "memory", "show", "--typo", "--root", root], { stderr: "pipe" });
    expect(await new Response(invalid.stderr).text()).toContain("unknown flag");
    expect(await invalid.exited).toBe(1);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
