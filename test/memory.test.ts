import { describe, expect, test } from "bun:test";
import { listMemories, searchMemories, showMemory } from "../src/memory.ts";

const paths = [
  "/mem/-Users-me-Development-a/memory/MEMORY.md",
  "/mem/-Users-me-Development-a/memory/deploy.md",
  "/mem/-Users-me-Development-b/memory/MEMORY.md",
];
const contents: Record<string, string> = {
  [paths[0]!]: "# Index\n- production deploy notes",
  [paths[1]!]: "Production deploy passed.\nproduction stayed healthy.",
  [paths[2]!]: "# Index\n- unrelated",
};
const deps = { glob: async () => paths, read: async (path: string) => contents[path]! };

describe("Claude memory corpus", () => {
  test("lists projects and file counts", async () => {
    expect(await listMemories("/mem", deps)).toEqual([
      { project: "-Users-me-Development-a", path: "/mem/-Users-me-Development-a/memory", files: 2 },
      { project: "-Users-me-Development-b", path: "/mem/-Users-me-Development-b/memory", files: 1 },
    ]);
  });

  test("searches every project and ranks by occurrence count", async () => {
    const result = await searchMemories("production", { root: "/mem" }, deps);
    expect(result.map((match) => [match.name, match.count])).toEqual([["deploy.md", 2], ["MEMORY.md", 1]]);
  });

  test("shows the project index by a unique project substring", async () => {
    const result = await showMemory("Development-a", "/mem", deps);
    expect(result.file.name).toBe("MEMORY.md");
    expect(result.content).toContain("production deploy notes");
  });
});
