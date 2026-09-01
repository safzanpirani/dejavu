import { homedir } from "node:os";
import { basename, join } from "node:path";

export interface MemoryFile {
  project: string;
  name: string;
  path: string;
}

export interface MemoryProject {
  project: string;
  path: string;
  files: number;
}

export interface MemoryMatch extends MemoryFile {
  count: number;
  snippets: string[];
}

export interface MemoryDeps {
  glob?: (root: string) => Promise<string[]>;
  read?: (path: string) => Promise<string>;
}

const defaultRead = (path: string) => Bun.file(path).text();

async function defaultGlob(root: string): Promise<string[]> {
  const paths: string[] = [];
  const glob = new Bun.Glob("*/memory/*.md");
  for await (const relative of glob.scan({ cwd: root, onlyFiles: true })) paths.push(join(root, relative));
  return paths.sort();
}

export function defaultMemoryRoot(): string {
  return join(process.env.CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude"), "projects");
}

function projectFromPath(path: string): string {
  return path.split("/memory/")[0]!.split("/").at(-1)!;
}

export async function memoryFiles(root = defaultMemoryRoot(), deps: MemoryDeps = {}): Promise<MemoryFile[]> {
  const paths = await (deps.glob ?? defaultGlob)(root);
  return paths.sort().map((path) => ({ project: projectFromPath(path), name: basename(path), path }));
}

export async function listMemories(root = defaultMemoryRoot(), deps: MemoryDeps = {}): Promise<MemoryProject[]> {
  const grouped = new Map<string, MemoryProject>();
  for (const file of await memoryFiles(root, deps)) {
    const existing = grouped.get(file.project) ?? { project: file.project, path: file.path.split("/memory/")[0]! + "/memory", files: 0 };
    existing.files += 1;
    grouped.set(file.project, existing);
  }
  return [...grouped.values()].sort((a, b) => a.project.localeCompare(b.project));
}

export async function searchMemories(query: string, options: { root?: string; limit?: number; snippets?: number } = {}, deps: MemoryDeps = {}): Promise<MemoryMatch[]> {
  const needle = query.trim().toLowerCase();
  if (!needle) throw new Error("memory search needs one token or exact phrase");
  const read = deps.read ?? defaultRead;
  const matches: MemoryMatch[] = [];
  for (const file of await memoryFiles(options.root, deps)) {
    let text: string;
    try { text = await read(file.path); } catch { continue; }
    const lines = text.split(/\r?\n/);
    const matching = lines.filter((line) => line.toLowerCase().includes(needle));
    if (matching.length === 0) continue;
    const lower = text.toLowerCase();
    let count = 0;
    for (let index = 0; (index = lower.indexOf(needle, index)) >= 0; index += needle.length) count += 1;
    matches.push({ ...file, count, snippets: matching.slice(0, options.snippets ?? 3) });
  }
  return matches.sort((a, b) => b.count - a.count || a.path.localeCompare(b.path)).slice(0, options.limit ?? 20);
}

export async function showMemory(selector: string, root = defaultMemoryRoot(), deps: MemoryDeps = {}): Promise<{ file: MemoryFile; content: string }> {
  const files = await memoryFiles(root, deps);
  const exactPath = files.find((file) => file.path === selector);
  const candidates = exactPath ? [exactPath] : files.filter((file) =>
    file.project === selector || `${file.project}/${file.name}` === selector || file.project.includes(selector),
  );
  if (candidates.length === 0) throw new Error(`no Claude memory matches '${selector}'`);
  const index = candidates.find((file) => file.name === "MEMORY.md");
  const chosen = candidates.length === 1 ? candidates[0]! : index;
  if (!chosen) throw new Error(`memory selector '${selector}' is ambiguous; use a project/name from 'dejavu memory list --files'`);
  return { file: chosen, content: await (deps.read ?? defaultRead)(chosen.path) };
}
