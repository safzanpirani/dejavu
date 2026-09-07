import { describe, expect, test } from "bun:test";
import { completeViaHttp, directTransportFor, estimateCost, resolveApiKey } from "../src/model-client.ts";
import type { FetchLike, ResolvedQueryModel } from "../src/model-client.ts";

const models = {
  providers: {
    groq: {
      baseUrl: "https://api.groq.com/openai/v1/",
      api: "openai-completions",
      compat: { maxTokensField: "max_tokens" },
      models: [{ id: "qwen/qwen3.8-27b", contextWindow: 131042, maxTokens: 16384, cost: { input: 0.8, output: 4 } }],
    },
    inline: { baseUrl: "https://crof.ai/v1", api: "openai-completions", apiKey: "literal-key" },
    envkey: { baseUrl: "https://x.example/v1", apiKey: "X_TOKEN" },
    anthropic: { baseUrl: "https://api.anthropic.com", api: "anthropic-messages" },
    oauth: { baseUrl: "https://oauth.example/v1", api: "openai-completions" },
  },
};
const auth = {
  groq: { type: "api_key", key: "gsk_test" },
  fireworks: { type: "api-key", key: "fw_test" },
  oauth: { type: "oauth", key: "should-not-be-used" },
};

describe("resolveApiKey", () => {
  test("prefers the inline provider key, then auth.json, then the environment", () => {
    expect(resolveApiKey("inline", models.providers.inline, auth, {})).toBe("literal-key");
    expect(resolveApiKey("groq", models.providers.groq, auth, {})).toBe("gsk_test");
    expect(resolveApiKey("fireworks", undefined, auth, {})).toBe("fw_test");
    expect(resolveApiKey("mistral", undefined, auth, { MISTRAL_API_KEY: "env-key" })).toBe("env-key");
  });

  test("treats an upper-case inline value as an environment variable name", () => {
    expect(resolveApiKey("envkey", models.providers.envkey, auth, { X_TOKEN: "from-env" })).toBe("from-env");
    expect(resolveApiKey("envkey", models.providers.envkey, auth, {})).toBeUndefined();
  });

  test("ignores oauth entries", () => {
    expect(resolveApiKey("oauth", models.providers.oauth, auth, {})).toBeUndefined();
  });
});

describe("directTransportFor", () => {
  test("builds a transport for OpenAI-compatible providers with a key", () => {
    expect(directTransportFor("groq", "qwen/qwen3.8-27b", models, auth, {})).toEqual({
      baseUrl: "https://api.groq.com/openai/v1",
      apiKey: "gsk_test",
      headers: {},
      maxTokensField: "max_tokens",
      maxTokens: 4096,
    });
  });

  test("falls back to Pi for other APIs, missing keys, and unknown providers", () => {
    expect(directTransportFor("anthropic", "claude", models, auth, {})).toBeUndefined();
    expect(directTransportFor("oauth", "m", models, auth, {})).toBeUndefined();
    expect(directTransportFor("missing", "m", models, auth, {})).toBeUndefined();
  });
});

describe("completeViaHttp", () => {
  const resolved: ResolvedQueryModel = {
    provider: "groq", id: "qwen/qwen3.8-27b", contextWindow: 1000, serialize: () => "", agentDir: "/pi",
  };
  const transport = { baseUrl: "https://api.example/v1", apiKey: "k", headers: { "X-Extra": "1" }, maxTokensField: "max_completion_tokens", maxTokens: 512 };

  test("posts a chat completion and returns the answer with usage", async () => {
    let seen: { url: string; init: RequestInit } | undefined;
    const fetchImpl: FetchLike = async (url, init) => {
      seen = { url, init };
      return new Response(JSON.stringify({
        choices: [{ message: { content: "  SQLite with FTS5.  " } }],
        usage: { prompt_tokens: 120, completion_tokens: 8 },
      }), { status: 200 });
    };
    const result = await completeViaHttp(resolved, transport, "prompt", undefined, fetchImpl);
    expect(result).toEqual({ answer: "SQLite with FTS5.", transport: "http", usage: { inputTokens: 120, outputTokens: 8 } });
    expect(seen!.url).toBe("https://api.example/v1/chat/completions");
    const headers = seen!.init.headers as Record<string, string>;
    expect(headers.Authorization).toBe("Bearer k");
    expect(headers["X-Extra"]).toBe("1");
    const body = JSON.parse(seen!.init.body as string);
    expect(body.model).toBe("qwen/qwen3.8-27b");
    expect(body.max_completion_tokens).toBe(512);
    expect(body.messages[1]).toEqual({ role: "user", content: "prompt" });
  });

  test("surfaces provider error messages", async () => {
    const fetchImpl: FetchLike = async () => new Response(JSON.stringify({ error: { message: "Request too large", code: "rate_limit_exceeded" } }), { status: 413 });
    await expect(completeViaHttp(resolved, transport, "prompt", undefined, fetchImpl)).rejects.toThrow("query model failed: 413: Request too large");
  });

  test("rejects empty answers", async () => {
    const fetchImpl: FetchLike = async () => new Response(JSON.stringify({ choices: [{ message: { content: "" } }] }), { status: 200 });
    await expect(completeViaHttp(resolved, transport, "prompt", undefined, fetchImpl)).rejects.toThrow("empty response");
  });

  test("reports cancellation", async () => {
    const controller = new AbortController();
    const fetchImpl: FetchLike = async (_url, init) => {
      controller.abort();
      init.signal!.throwIfAborted();
      return new Response("{}");
    };
    await expect(completeViaHttp(resolved, transport, "prompt", controller.signal, fetchImpl)).rejects.toThrow("query was cancelled");
  });
});

describe("estimateCost", () => {
  test("prices per million tokens", () => {
    expect(estimateCost({ input: 0.8, output: 4 }, { inputTokens: 20_000, outputTokens: 500 })).toBeCloseTo(0.018, 6);
    expect(estimateCost(undefined, { inputTokens: 1, outputTokens: 1 })).toBeUndefined();
    expect(estimateCost({ input: 1, output: 1 }, undefined)).toBeUndefined();
  });
});
