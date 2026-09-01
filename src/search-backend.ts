export interface FileMatchCount { path: string; count: number }
interface CommandResult { exitCode: number; stdout: string; stderr: string }
export interface SearchBackendDeps {
  run?: (command: string[], timeoutMs?: number) => Promise<CommandResult>;
  glob?: (directory: string) => Promise<string[]>;
  readText?: (path: string) => Promise<string>;
}

async function runCommand(command: string[], timeoutMs = 10_000): Promise<CommandResult> {
  const processHandle = Bun.spawn(command, { stdout: "pipe", stderr: "pipe" });
  const timeout = setTimeout(() => processHandle.kill(), timeoutMs);
  try {
    const [exitCode, stdout, stderr] = await Promise.all([
      processHandle.exited,
      new Response(processHandle.stdout).text(),
      new Response(processHandle.stderr).text(),
    ]);
    return { exitCode, stdout, stderr };
  } finally { clearTimeout(timeout); }
}

async function globJsonl(directory: string): Promise<string[]> {
  const glob = new Bun.Glob("**/*.jsonl");
  const paths: string[] = [];
  for await (const path of glob.scan({ cwd: directory, absolute: true, onlyFiles: true })) paths.push(path);
  return paths;
}

const defaultReadText = (path: string) => Bun.file(path).text();
async function available(command: "rg" | "grep", run: NonNullable<SearchBackendDeps["run"]>): Promise<boolean> {
  try { return (await run([command, "--version"], 5_000)).exitCode === 0; }
  catch { return false; }
}

export function parseCountOutput(output: string): FileMatchCount[] {
  return output.trim().split("\n").filter(Boolean).flatMap((line) => {
    const separator = line.lastIndexOf(":");
    if (separator < 0) return [];
    const count = Number.parseInt(line.slice(separator + 1), 10);
    return Number.isInteger(count) && count > 0 ? [{ path: line.slice(0, separator), count }] : [];
  });
}

export async function searchFileCounts(query: string, sessionsDir: string, deps: SearchBackendDeps = {}): Promise<FileMatchCount[]> {
  const run = deps.run ?? runCommand;
  if (await available("rg", run)) {
    const result = await run(["rg", "-i", "-c", "-F", query, sessionsDir]);
    if (result.exitCode === 0) return parseCountOutput(result.stdout);
    if (result.exitCode === 1) return [];
  }
  if (await available("grep", run)) {
    const result = await run(["grep", "-r", "-i", "-c", "-F", "--include=*.jsonl", query, sessionsDir]);
    if (result.exitCode === 0) return parseCountOutput(result.stdout);
    if (result.exitCode === 1) return [];
  }
  return nodeSearchFileCounts(query, sessionsDir, deps);
}

export async function searchMatchingLines(query: string, filePath: string, maxMatches: number, deps: SearchBackendDeps = {}): Promise<string[]> {
  const run = deps.run ?? runCommand;
  if (await available("rg", run)) {
    const result = await run(["rg", "-i", "-F", "-m", String(maxMatches), query, filePath], 5_000);
    if (result.exitCode === 0) return splitLines(result.stdout);
    if (result.exitCode === 1) return [];
  }
  if (await available("grep", run)) {
    const result = await run(["grep", "-i", "-F", "-m", String(maxMatches), query, filePath], 5_000);
    if (result.exitCode === 0) return splitLines(result.stdout);
    if (result.exitCode === 1) return [];
  }
  const text = await (deps.readText ?? defaultReadText)(filePath);
  const lower = query.toLowerCase();
  return text.split("\n").filter((line) => line.toLowerCase().includes(lower)).slice(0, maxMatches);
}

async function nodeSearchFileCounts(query: string, sessionsDir: string, deps: SearchBackendDeps): Promise<FileMatchCount[]> {
  const paths = await (deps.glob ?? globJsonl)(sessionsDir);
  const readText = deps.readText ?? defaultReadText;
  const lowerQuery = query.toLowerCase();
  const matches: FileMatchCount[] = [];
  for (const path of paths) {
    try {
      const text = (await readText(path)).toLowerCase();
      let count = 0;
      let index = 0;
      while ((index = text.indexOf(lowerQuery, index)) >= 0) {
        count++;
        index += lowerQuery.length;
      }
      if (count > 0) matches.push({ path, count });
    } catch { /* One unreadable session must not abort the corpus search. */ }
  }
  return matches;
}

function splitLines(output: string): string[] {
  return output.trim().split("\n").filter(Boolean);
}
