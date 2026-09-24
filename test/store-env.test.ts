import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { discoverTranscriptStores, sourceFromLocator, transcriptStoreRoots } from "../src/source-registry.ts";
import { listIndexedSessions, refreshTranscriptIndex, searchTranscriptIndexMatches } from "../src/transcript-index.ts";
import { projectFromClaudePath } from "../src/transcript-paths.ts";
import type { TranscriptStore } from "../src/transcript-types.ts";

const cli = new URL("../src/cli.ts", import.meta.url).pathname;
const STORE_VARS = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "PI_CODING_AGENT_DIR", "XDG_DATA_HOME", "OPENCODE_DB"];

function claudeLine(cwd: string, text: string): string {
  return `${JSON.stringify({ type: "user", timestamp: "2026-09-20T10:00:00Z", cwd, message: { role: "user", content: [{ type: "text", text }] } })}\n`;
}

async function writeClaude(projects: string, project: string, name: string, text: string): Promise<string> {
  const directory = join(projects, `-work-${project}`);
  await mkdir(directory, { recursive: true });
  const path = join(directory, `${name}.jsonl`);
  await writeFile(path, claudeLine(`/work/${project}`, text));
  return path;
}

async function writeOpenCode(path: string, sessionId: string, text: string): Promise<void> {
  await mkdir(join(path, ".."), { recursive: true });
  const database = new Database(path, { create: true });
  database.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER)");
  database.run("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT)");
  database.run("CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT)");
  database.run("INSERT INTO session VALUES (?, ?, ?, ?)", [sessionId, `/work/${sessionId}`, "t", Date.UTC(2026, 8, 20)]);
  database.run("INSERT INTO message VALUES (?, ?, ?, ?)", [`msg_${sessionId}`, sessionId, 1, JSON.stringify({ role: "user" })]);
  database.run("INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)", [`part_${sessionId}`, `msg_${sessionId}`, sessionId, 1, 1, JSON.stringify({ type: "text", text })]);
  database.close();
}

async function runCli(args: string[], env: Record<string, string>) {
  const base = Object.fromEntries(Object.entries(process.env).filter(([key]) => !STORE_VARS.includes(key)));
  const child = Bun.spawn([process.execPath, cli, ...args], {
    env: { ...base, DEJAVU_NO_UPDATE_CHECK: "1", ...env }, stdout: "pipe", stderr: "pipe",
  });
  const [stdout, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
  return { lines: stdout.trim().split("\n").filter(Boolean), code };
}

describe("store resolution from agent environment variables", () => {
  const home = "/home/owner";

  test("defaults to home-directory stores when no variable is set", () => {
    expect(transcriptStoreRoots({}, home)).toEqual({
      claude: "/home/owner/.claude/projects",
      codex: "/home/owner/.codex/sessions",
      pi: "/home/owner/.pi/agent/sessions",
      opencode: ["opencode.db", "opencode-next.db", "opencode-local.db"].map((name) => `/home/owner/.local/share/opencode/${name}`),
    });
    expect(transcriptStoreRoots({ CLAUDE_CONFIG_DIR: "", XDG_DATA_HOME: "" }, home).claude).toBe("/home/owner/.claude/projects");
  });

  test("each set variable replaces the default store", () => {
    const roots = transcriptStoreRoots({
      CLAUDE_CONFIG_DIR: "/home/owner/.claude-rakhi",
      CODEX_HOME: "/home/owner/.codex-rakhi",
      PI_CODING_AGENT_DIR: "~/.pi-rakhi",
      XDG_DATA_HOME: "/home/owner/.rakhi-data",
    }, home);
    expect(roots).toEqual({
      claude: "/home/owner/.claude-rakhi/projects",
      codex: "/home/owner/.codex-rakhi/sessions",
      pi: "/home/owner/.pi-rakhi/sessions",
      opencode: ["opencode.db", "opencode-next.db", "opencode-local.db"].map((name) => `/home/owner/.rakhi-data/opencode/${name}`),
    });
    expect(transcriptStoreRoots({ XDG_DATA_HOME: "/data", OPENCODE_DB: "custom.db" }, home).opencode).toEqual(["/data/opencode/custom.db"]);
    expect(transcriptStoreRoots({ OPENCODE_DB: "/abs/x.db" }, home).opencode).toEqual(["/abs/x.db"]);
  });

  test("discovery returns the overridden claude and opencode stores instead of the home ones", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-store-env-"));
    try {
      await writeClaude(join(root, ".claude", "projects"), "owner", "owner", "owner text");
      await writeClaude(join(root, ".claude-rakhi", "projects"), "rakhi", "rakhi", "rakhi text");
      await writeOpenCode(join(root, ".local", "share", "opencode", "opencode.db"), "ses_owner", "owner text");
      await writeOpenCode(join(root, ".rakhi-data", "opencode", "opencode.db"), "ses_rakhi", "rakhi text");
      const env = { CLAUDE_CONFIG_DIR: join(root, ".claude-rakhi"), XDG_DATA_HOME: join(root, ".rakhi-data") };
      expect((await discoverTranscriptStores("all", root, env)).map((store) => store.path)).toEqual([
        join(root, ".claude-rakhi", "projects"),
        join(root, ".rakhi-data", "opencode", "opencode.db"),
      ]);
      expect((await discoverTranscriptStores("all", root, {})).map((store) => store.path)).toEqual([
        join(root, ".claude", "projects"),
        join(root, ".local", "share", "opencode", "opencode.db"),
      ]);
    } finally { await rm(root, { recursive: true, force: true }); }
  });

  test("locators and project names resolve under a CLAUDE_CONFIG_DIR store", () => {
    const roots = transcriptStoreRoots({ CLAUDE_CONFIG_DIR: "/home/owner/.claude-rakhi", CODEX_HOME: "/srv/cx" }, home);
    expect(sourceFromLocator("/home/owner/.claude-rakhi/projects/-work-app/s.jsonl", roots)).toBe("claude");
    expect(sourceFromLocator("/srv/cx/sessions/2026/09/20/rollout-x.jsonl", roots)).toBe("codex");
    expect(projectFromClaudePath("/home/owner/.claude-rakhi/projects/-home-owner-work-app/s.jsonl", home, roots.claude)).toBe("work/app");
  });
});

describe("transcript index isolation across stores", () => {
  test("one index file never returns rows from a store the caller did not select", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-index-isolation-"));
    const index = join(root, "index.sqlite");
    const owner: TranscriptStore = { source: "claude", kind: "jsonl", path: join(root, "owner", "projects") };
    const rakhi: TranscriptStore = { source: "claude", kind: "jsonl", path: join(root, "rakhi", "projects") };
    try {
      const ownerPath = await writeClaude(owner.path, "app", "owner", "shared zebratoken");
      const rakhiPath = await writeClaude(rakhi.path, "app", "rakhi", "shared zebratoken");
      await refreshTranscriptIndex([owner], index);
      await refreshTranscriptIndex([rakhi], index);
      expect(searchTranscriptIndexMatches("zebratoken", [rakhi], index).map((match) => match.path)).toEqual([rakhiPath]);
      expect(searchTranscriptIndexMatches("zebratoken", [owner], index).map((match) => match.path)).toEqual([ownerPath]);
      expect(listIndexedSessions({ project: "app", limit: 10 }, [rakhi], index).paths).toEqual([rakhiPath]);
      expect(listIndexedSessions({ project: "app", limit: 10 }, [owner], index).paths).toEqual([ownerPath]);
    } finally { await rm(root, { recursive: true, force: true }); }
  });

  test("CLI find with a shared index cache follows the env-selected store", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-cli-isolation-"));
    try {
      const ownerClaude = await writeClaude(join(root, ".claude", "projects"), "owner", "owner", "zebratoken owner");
      const rakhiClaude = await writeClaude(join(root, ".claude-rakhi", "projects"), "rakhi", "rakhi", "zebratoken rakhi");
      await writeOpenCode(join(root, ".local", "share", "opencode", "opencode.db"), "ses_owner", "zebratoken owner");
      await writeOpenCode(join(root, ".rakhi-data", "opencode", "opencode.db"), "ses_rakhi", "zebratoken rakhi");
      const shared = { HOME: root, XDG_CACHE_HOME: join(root, "cache") };
      const rakhi = { ...shared, CLAUDE_CONFIG_DIR: join(root, ".claude-rakhi"), XDG_DATA_HOME: join(root, ".rakhi-data") };
      const args = ["find", "zebratoken", "-n", "5", "--paths"];

      // The owner indexes first, so a store-blind index would hand the owner's rows to Rakhi.
      expect((await runCli([...args, "--source", "claude"], shared)).lines).toEqual([ownerClaude]);
      expect((await runCli([...args, "--source", "claude"], rakhi)).lines).toEqual([rakhiClaude]);
      expect((await runCli([...args, "--source", "opencode"], shared)).lines)
        .toEqual([`opencode://${join(root, ".local", "share", "opencode", "opencode.db")}#ses_owner`]);
      expect((await runCli([...args, "--source", "opencode"], rakhi)).lines)
        .toEqual([`opencode://${join(root, ".rakhi-data", "opencode", "opencode.db")}#ses_rakhi`]);
      expect((await runCli([...args, "--source", "claude"], shared)).lines).toEqual([ownerClaude]);
    } finally { await rm(root, { recursive: true, force: true }); }
  });
});
