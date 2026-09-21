#!/usr/bin/env bash
# Install the recall adapter to ~/.local/bin/dejavu-recall.
#
# The adapter is maintained in this repository; the installed copy is a build
# artifact. Re-run this after editing adapters/recall-hook.py.
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_file="$repo_dir/adapters/recall-hook.py"
target="${DEJAVU_RECALL_TARGET:-$HOME/.local/bin/dejavu-recall}"

[ -f "$source_file" ] || { echo "missing $source_file" >&2; exit 1; }

mkdir -p "$(dirname "$target")"

if [ -e "$target" ] && ! cmp -s "$source_file" "$target"; then
  backup="$target.bak-$(date +%Y%m%d-%H%M%S)"
  cp -p "$target" "$backup"
  echo "backed up existing adapter to $backup"
fi

install -m 0755 "$source_file" "$target"
echo "installed $target"

cat <<'NOTE'

Registered harness hooks that call this adapter (verified 2026-09-21):
  Claude Code  ~/.claude/settings.json        UserPromptSubmit
  Codex        ~/.codex/hooks.json            UserPromptSubmit
  OpenCode     ~/.config/opencode/plugins/agent-overlay.ts   chat.message
  Pi           ~/.pi/agent/extensions/agent-overlay.ts       before_agent_start
Changing the adapter's contents needs no hook re-registration; Codex hashes the
hook command string, not the script.
NOTE
