import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  availableUpdate, checksumFor, compareVersions, releaseAssetName, selfUpdate, VERSION, versionFromReleaseUrl,
} from "../src/update.ts";

const RELEASE_URL = "https://github.com/safzanpirani/dejavu/releases";

function releaseFetch(latest: string, files: Record<string, string> = {}, calls: string[] = []): typeof fetch {
  return (async (input: string | URL | Request) => {
    const url = String(input);
    calls.push(url);
    if (url.endsWith("/releases/latest")) return new Response(null, { status: 302, headers: { location: `${RELEASE_URL}/tag/v${latest}` } });
    const name = url.slice(url.lastIndexOf("/") + 1);
    return name in files ? new Response(files[name]) : new Response("missing", { status: 404 });
  }) as typeof fetch;
}

function bumped(version: string): string {
  const [major, minor] = version.split(".").map(Number);
  return `${major}.${minor! + 1}.0`;
}

describe("release versions", () => {
  test("compares dotted versions and sorts unparsable values lowest", () => {
    expect(compareVersions("0.10.0", "0.9.9")).toBe(1);
    expect(compareVersions("v1.2", "1.2.0")).toBe(0);
    expect(compareVersions("1.2.3-beta", "1.2.4")).toBe(-1);
    expect(compareVersions("junk", "0.0.1")).toBe(-1);
  });

  test("reads the tag from the latest-release redirect", () => {
    expect(versionFromReleaseUrl(`${RELEASE_URL}/tag/v0.4.0`)).toBe("0.4.0");
    expect(() => versionFromReleaseUrl(RELEASE_URL)).toThrow(/no published release/);
    expect(() => versionFromReleaseUrl(`${RELEASE_URL}/tag/nightly`)).toThrow(/unexpected release tag/);
  });

  test("names assets per platform and finds their checksums", () => {
    expect(releaseAssetName("darwin", "arm64")).toBe("dejavu-darwin-arm64");
    expect(releaseAssetName("win32", "x64")).toBe("dejavu-windows-x64.exe");
    expect(checksumFor("ABC  dejavu-linux-x64\ndef *dejavu-darwin-arm64\n", "dejavu-darwin-arm64")).toBe("def");
    expect(checksumFor("abc  dejavu-linux-x64\n", "dejavu-darwin-arm64")).toBeUndefined();
  });
});

describe("selfUpdate", () => {
  test("replaces the executable with the checksum-verified release binary", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-update-"));
    const executable = join(root, "dejavu");
    await writeFile(executable, "old");
    const latest = bumped(VERSION);
    const binary = "new binary";
    const digest = createHash("sha256").update(binary).digest("hex");
    try {
      const fetch = releaseFetch(latest, { "checksums.txt": `${digest}  dejavu-darwin-arm64\n`, "dejavu-darwin-arm64": binary });
      const deps = { fetch, executable, compiled: true, platform: "darwin" as const, arch: "arm64", checkPath: join(root, "check.json"), log: () => {} };
      expect(await selfUpdate({ checkOnly: true }, deps)).toEqual({ current: VERSION, latest, updated: false });
      expect(await readFile(executable, "utf8")).toBe("old");
      expect(await selfUpdate({}, deps)).toMatchObject({ latest, updated: true });
      expect(await readFile(executable, "utf8")).toBe(binary);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("refuses a binary whose checksum does not match and leaves the executable alone", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-update-"));
    const executable = join(root, "dejavu");
    await writeFile(executable, "old");
    try {
      const fetch = releaseFetch(bumped(VERSION), { "checksums.txt": `${"0".repeat(64)}  dejavu-linux-x64\n`, "dejavu-linux-x64": "tampered" });
      const deps = { fetch, executable, compiled: true, platform: "linux" as const, arch: "x64", checkPath: join(root, "check.json"), log: () => {} };
      await expect(selfUpdate({}, deps)).rejects.toThrow(/checksum mismatch/);
      expect(await readFile(executable, "utf8")).toBe("old");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("sends a source checkout to git pull", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-update-"));
    try {
      const deps = { fetch: releaseFetch(bumped(VERSION)), compiled: false, checkPath: join(root, "check.json") };
      await expect(selfUpdate({}, deps)).rejects.toThrow(/git pull/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("availableUpdate", () => {
  test("looks up releases at most once a day and keeps the last answer through an outage", async () => {
    const root = await mkdtemp(join(tmpdir(), "dejavu-update-"));
    const checkPath = join(root, "check.json");
    const latest = bumped(VERSION);
    const calls: string[] = [];
    try {
      const day = 24 * 60 * 60 * 1000;
      expect(await availableUpdate({ fetch: releaseFetch(latest, {}, calls), checkPath, now: () => 1_000 })).toBe(latest);
      expect(await availableUpdate({ fetch: releaseFetch(latest, {}, calls), checkPath, now: () => 1_000 + day / 2 })).toBe(latest);
      expect(calls).toHaveLength(1);
      const failing = (async () => { throw new Error("offline"); }) as unknown as typeof fetch;
      expect(await availableUpdate({ fetch: failing, checkPath, now: () => 2_000 + day })).toBe(latest);
      expect(JSON.parse(await readFile(checkPath, "utf8"))).toMatchObject({ checkedAt: 2_000 + day, latest });
      expect(await availableUpdate({ fetch: releaseFetch(VERSION), checkPath, now: () => 3_000 + 2 * day })).toBeNull();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
