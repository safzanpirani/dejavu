import { join } from "node:path";
import type { RecallMessage } from "./session-reader.ts";
import { serializeRecallMessages } from "./session-reader.ts";

const SYSTEM_PROMPT = `You are a session context assistant. Given the conversation history from a coding-agent session and a question, provide a concise answer based on the session contents.

Focus on specific facts, decisions, outcomes, file paths, and code changes. If the information is not in the session, say so.`;

interface ModelConfig { queryModel?: { provider?: string; id?: string } }
interface PiSettings { defaultProvider?: string; defaultModel?: string }
interface ModelsConfig {
  providers?: Record<string, { models?: Array<{ id?: string; contextWindow?: number }> }>;
}

export interface ResolvedQueryModel {
  provider: string;
  id: string;
  contextWindow: number;
  serialize: (messages: RecallMessage[]) => string;
  agentDir: string;
}

async function readJson<T>(path: string): Promise<T> {
  try { return await Bun.file(path).json() as T; }
  catch { return {} as T; }
}

export async function resolveQueryModel(agentDir: string, override?: string): Promise<ResolvedQueryModel> {
  const recallConfig = await readJson<ModelConfig>(join(agentDir, "session-recall.json"));
  const settings = await readJson<PiSettings>(join(agentDir, "settings.json"));
  const models = await readJson<ModelsConfig>(join(agentDir, "models.json"));
  const configured = recallConfig.queryModel?.provider && recallConfig.queryModel.id
    ? `${recallConfig.queryModel.provider}/${recallConfig.queryModel.id}` : undefined;
  const fallback = settings.defaultProvider && settings.defaultModel
    ? `${settings.defaultProvider}/${settings.defaultModel}` : undefined;
  const identifier = override ?? configured ?? fallback;
  if (!identifier) throw new Error("no query model configured; pass --model provider/id or set Pi's default model");
  const separator = identifier.indexOf("/");
  if (separator <= 0 || separator === identifier.length - 1) throw new Error(`model must be provider/id (got '${identifier}')`);
  const provider = identifier.slice(0, separator);
  const id = identifier.slice(separator + 1);
  return {
    provider,
    id,
    contextWindow: findContextWindow(models, provider, id),
    serialize: serializeRecallMessages,
    agentDir,
  };
}

export function findContextWindow(models: ModelsConfig, provider: string, id: string): number {
  const configured = models.providers?.[provider]?.models?.find((model) => model.id === id)?.contextWindow;
  return typeof configured === "number" && configured > 0 ? configured : 128_000;
}

export async function completeQuery(
  resolved: ResolvedQueryModel,
  conversation: string,
  question: string,
  signal?: AbortSignal,
): Promise<string> {
  const contextNote = /messages? omitted \.\.\.\]/.test(conversation)
    ? "\n\nNote: This large session was windowed; omitted gaps are marked in the conversation." : "";
  const prompt = `## Session Conversation${contextNote}\n\n${conversation}\n\n## Question\n\n${question}`;
  const command = [
    "pi", "--print", "--no-session", "--no-tools", "--no-skills", "--no-prompt-templates",
    "--no-context-files", "--model", `${resolved.provider}/${resolved.id}`,
    "--system-prompt", SYSTEM_PROMPT,
  ];
  const processHandle = Bun.spawn(command, {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, PI_CODING_AGENT_DIR: resolved.agentDir },
  });
  const abort = () => processHandle.kill();
  signal?.addEventListener("abort", abort, { once: true });
  try {
    processHandle.stdin.write(prompt);
    processHandle.stdin.end();
    const [exitCode, stdout, stderr] = await Promise.all([
      processHandle.exited,
      new Response(processHandle.stdout).text(),
      new Response(processHandle.stderr).text(),
    ]);
    if (signal?.aborted) throw new Error("query was cancelled");
    if (exitCode !== 0) {
      const detail = stderr.trim().split("\n").at(-1) || `pi exited ${exitCode}`;
      throw new Error(`query model failed: ${detail}`);
    }
    const answer = stdout.trim();
    if (!answer) throw new Error("query model returned an empty response");
    return answer;
  } finally {
    signal?.removeEventListener("abort", abort);
  }
}
