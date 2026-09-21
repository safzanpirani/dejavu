import { describe, expect, test } from "bun:test";
import { clipToChars, isEligible, rank, recall, selectRanked, tokenize, type RecallQuery } from "../src/project-memory-recall.ts";
import type { MemoryRecord } from "../src/project-memory-types.ts";
import { DEFAULT_RECALL_BUDGET } from "../src/project-memory-types.ts";

const now = new Date("2026-06-01T00:00:00.000Z");

function memory(overrides: Partial<MemoryRecord> = {}): MemoryRecord {
  return {
    id: "00000000-0000-4000-8000-000000000000",
    projectId: "p1",
    revision: 1,
    kind: "convention",
    title: "Integration tests need Redis",
    body: "The fixture does not start Redis; start it before running the suite.",
    status: "active",
    verification: "user_confirmed",
    tags: ["testing"],
    evidence: [],
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z",
    pinned: false,
    contentHash: "sha256:x",
    ...overrides,
  };
}

function query(overrides: Partial<RecallQuery> = {}): RecallQuery {
  return { now, budgetChars: DEFAULT_RECALL_BUDGET, limit: 12, ...overrides };
}

describe("eligibility", () => {
  test("unverified imports are excluded from automatic recall", () => {
    const record = memory({ verification: "unverified" });
    expect(isEligible(record, query())).toBe(false);
    expect(isEligible(record, query(), { includeUnverified: true })).toBe(true);
  });

  test("expiry is strict at the injected clock", () => {
    expect(isEligible(memory({ expiresAt: "2026-06-01T00:00:01.000Z" }), query())).toBe(true);
    expect(isEligible(memory({ expiresAt: "2026-06-01T00:00:00.000Z" }), query())).toBe(false);
    expect(isEligible(memory({ expiresAt: "2026-05-31T23:59:59.000Z" }), query())).toBe(false);
  });

  test("archived and superseded records never reach recall", () => {
    expect(isEligible(memory({ status: "archived" }), query())).toBe(false);
    expect(isEligible(memory({ status: "superseded" }), query())).toBe(false);
  });

  test("branch scope is exact and detached HEAD matches nothing", () => {
    expect(isEligible(memory({ branch: "main" }), query({ branch: "main" }))).toBe(true);
    expect(isEligible(memory({ branch: "main" }), query({ branch: "ma" }))).toBe(false);
    expect(isEligible(memory({ branch: "main" }), query({ branch: undefined }))).toBe(false);
  });

  test("path prefixes honour segments and are omitted at the repository root", () => {
    const record = memory({ pathPrefix: "packages/api" });
    expect(isEligible(record, query({ targetPath: "packages/api/src/x.ts" }))).toBe(true);
    expect(isEligible(record, query({ targetPath: "packages/api-client/x.ts" }))).toBe(false);
    expect(isEligible(record, query({ targetPath: undefined }))).toBe(false);
  });
});

describe("ranking", () => {
  test("tokenization keeps identifiers and digits", () => {
    expect(tokenize("Fix __path_prefix in run_2")).toEqual(["fix", "__path_prefix", "in", "run_2"]);
  });

  test("title outweighs tag, tag outweighs body, and repeats cannot inflate", () => {
    const title = memory({ id: "a", title: "deploy hook", body: "nothing" });
    const tag = memory({ id: "b", title: "nothing", tags: ["deploy"], body: "nothing" });
    const body = memory({ id: "c", title: "nothing", body: "deploy" });
    const noisy = memory({ id: "d", title: "nothing", body: "deploy deploy deploy deploy" });
    const ranked = rank([body, tag, title, noisy], "deploy");
    expect(ranked.map((entry) => entry.record.id)).toEqual(["a", "b", "c", "d"]);
    expect(ranked[2]!.score).toBe(ranked[3]!.score);
  });

  test("pinned records lead regardless of score", () => {
    const pinned = memory({ id: "pinned", pinned: true, title: "unrelated" });
    const relevant = memory({ id: "relevant", title: "redis" });
    const ranked = rank([relevant, pinned], "redis");
    expect(selectRanked(ranked, "redis", 10)[0]!.id).toBe("pinned");
  });

  test("with a query, non-pinned records need positive relevance", () => {
    const relevant = memory({ id: "hit", title: "redis", body: "start redis first" });
    const irrelevant = memory({ id: "miss", title: "unrelated", body: "no such token here" });
    const chosen = selectRanked(rank([relevant, irrelevant], "redis"), "redis", 10);
    expect(chosen.map((record) => record.id)).toEqual(["hit"]);
  });

  test("ties fall back to verification time then id", () => {
    const older = memory({ id: "zzz", lastVerifiedAt: "2026-01-01T00:00:00.000Z" });
    const newer = memory({ id: "aaa", lastVerifiedAt: "2026-05-01T00:00:00.000Z" });
    const chosen = selectRanked(rank([older, newer], undefined), undefined, 10);
    expect(chosen.map((record) => record.id)).toEqual(["aaa", "zzz"]);
  });
});

describe("budgets", () => {
  test("the whole rendered context obeys the budget and the char accounting", () => {
    const records = Array.from({ length: 20 }, (_, index) => memory({
      id: `00000000-0000-4000-8000-0000000000${String(index).padStart(2, "0")}`,
      title: `Record ${index} redis`,
      body: "x".repeat(400),
    }));
    const outcome = recall("demo", records, query({ query: "redis", budgetChars: 2_000 }));
    expect(outcome.usedChars).toBe(outcome.context.length);
    expect(outcome.context.length).toBeLessThanOrEqual(2_000);
    expect(outcome.omitted).toBeGreaterThan(0);
  });

  test("clipping is marked and never emits a partial record as complete", () => {
    const records = [memory({ body: "y".repeat(5_000) })];
    const outcome = recall("demo", records, query({ budgetChars: 500 }));
    expect(outcome.context).toContain("clipped");
    expect(outcome.context).toContain("dejavu memory project get");
    expect(outcome.selected[0]!.clipped).toBe(true);
    expect(outcome.context.length).toBeLessThanOrEqual(500);
  });

  test("Unicode bodies are clipped without splitting a surrogate pair", () => {
    expect(clipToChars("a😀b", 2)).toBe("a");
    expect(clipToChars("a😀b", 3)).toBe("a😀");
    const records = [memory({ body: "😀".repeat(400) })];
    const outcome = recall("demo", records, query({ budgetChars: 300 }));
    expect(outcome.context.length).toBeLessThanOrEqual(300);
    expect([...outcome.context].every((char) => char.isWellFormed?.() ?? true)).toBe(true);
    // Re-encoding must not invent a replacement character.
    expect(outcome.context).not.toContain("\uFFFD");
  });

  test("a tiny budget returns empty context with a diagnostic", () => {
    const records = [memory()];
    const outcome = recall("demo", records, query({ budgetChars: 10 }));
    expect(outcome.context).toBe("");
    expect(outcome.usedChars).toBe(0);
    expect(outcome.diagnostics.length).toBeGreaterThan(0);
    expect(outcome.omitted).toBe(1);
  });

  test("no query still returns pinned records and stays deterministic", () => {
    const pinned = memory({ id: "p", pinned: true });
    const other = memory({ id: "o" });
    const first = recall("demo", [other, pinned], query());
    const second = recall("demo", [pinned, other], query());
    expect(first.context).toBe(second.context);
    expect(first.context).toContain("p");
  });

  test("no matches yields empty context, not a header-only string", () => {
    const outcome = recall("demo", [memory({ title: "unrelated" })], query({ query: "zzz" }));
    expect(outcome.context).toBe("");
    expect(outcome.selected).toEqual([]);
  });
});
