/**
 * Help for `dejavu memory project`. Kept in its own module so the top-level
 * HELP block in `cli.ts` stays focused on the established commands.
 */

import { DEFAULT_RECALL_BUDGET, DEFAULT_RECALL_RECORDS, MAX_RECALL_BUDGET } from "./project-memory-types.ts";

export function projectMemoryHelp(): string {
  return `dejavu memory project — cross-harness, per-project memory

  dejavu memory project init [--cwd PATH] [--name NAME] [--folder]
      Register a project and return its id. Git repositories bind by their
      common directory, so every worktree shares one identity. --folder forces
      an explicit directory boundary for non-Git trees.

  dejavu memory project resolve [--cwd PATH] [--project-id ID]
      Explain which project and boundary this directory resolves to.

  dejavu memory project projects
      List project identities and their bindings (never memory bodies).

  dejavu memory project bind --project-id ID --cwd PATH
      Attach another checkout or folder to an existing project.

  dejavu memory project unbind --project-id ID --binding PATH
      Detach one binding. Memories and the project are kept.

  dejavu memory project add --file INPUT.json [--cwd PATH] [--project-id ID]
                            [--harness NAME] [--session-id ID] [--request-id ID]
  dejavu memory project update ID --if-revision N --file INPUT.json [...]
  dejavu memory project supersede ID --if-revision N --file INPUT.json [...]
  dejavu memory project archive ID --if-revision N --reason TEXT [...]
  dejavu memory project purge ID --if-revision N --reason TEXT [...]
      Write operations. --file - reads the payload from stdin. Mutations are
      scoped, validated, revisioned, and idempotent per --request-id.

  dejavu memory project get ID [--cwd PATH]
  dejavu memory project list [--include-inactive] [--limit N]
  dejavu memory project search PHRASE [--include-inactive] [--all-projects]
                                      [--limit N] [--snippets N]
  dejavu memory project history ID
      Read operations. list and search are metadata-first; --all-projects is
      manual-only and never used by automatic recall.

  dejavu memory project recall [--query TEXT] [--path RELPATH]
                               [--budget-chars N] [--limit N]
                               [--include-unverified]
      Bounded, deterministic context for the selected project. The whole
      rendered context string is charged against the budget
      (default ${DEFAULT_RECALL_BUDGET}, ceiling ${MAX_RECALL_BUDGET}, ${DEFAULT_RECALL_RECORDS} records).
      No model call and no transcript index refresh.

  dejavu memory project export --format json|markdown --output PATH
                               [--include-local-metadata]
  dejavu memory project import --file PATH [--project-id ID | --create-project NAME] [--dry-run]
  dejavu memory project import-claude --from DIR --project-id ID
                                      [--kind KIND] [--dry-run]
      Portability and explicit migration. Claude imports are unverified and
      stay out of automatic recall until confirmed; source files are untouched.

  dejavu memory project doctor
  dejavu memory project backup --output PATH

memory kinds        decision, convention, procedure, pitfall, handoff
verification        unverified, user_confirmed, code_verified
harness             claude, codex, pi, opencode, manual, unknown

shared flags        --cwd PATH, --project-id ID, --json
exit codes          0 success · 1 operational · 2 invalid arguments
                    3 revision conflict · 4 missing project or record`;
}
