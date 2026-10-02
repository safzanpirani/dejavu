#!/usr/bin/env bash
# Runs one dejavu command through the Bun source and the Rust binary and
# diffs stdout and the exit code. Each side keeps its own index so neither
# touches ~/.cache/dejavu.
#
#   scripts/parity.sh find deployment timeout --json
#   PARITY_STDERR=1 scripts/parity.sh show <locator>   # also diff stderr
set -uo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=${PARITY_DIR:-${TMPDIR:-/tmp}/dejavu-parity}
mkdir -p "$work"
rust=${DEJAVU_RUST:-$root/target/release/dejavu}
export DEJAVU_NO_UPDATE_CHECK=1 NO_COLOR=1
DEJAVU_INDEX_PATH="$work/ts.sqlite" bun run "$root/src/cli.ts" "$@" >"$work/ts.out" 2>"$work/ts.err"; ts=$?
DEJAVU_INDEX_PATH="$work/rs.sqlite" "$rust" "$@" >"$work/rs.out" 2>"$work/rs.err"; rs=$?
status=0
if [ "$ts" != "$rs" ]; then echo "exit: ts=$ts rs=$rs"; status=1; fi
if ! diff -u "$work/ts.out" "$work/rs.out" >"$work/stdout.diff"; then echo "stdout differs: $work/stdout.diff"; head -40 "$work/stdout.diff"; status=1; fi
if [ "${PARITY_STDERR:-0}" = 1 ] && ! diff -u "$work/ts.err" "$work/rs.err" >"$work/stderr.diff"; then echo "stderr differs: $work/stderr.diff"; head -20 "$work/stderr.diff"; status=1; fi
[ $status = 0 ] && echo "same ($(wc -c <"$work/rs.out" | tr -d ' ') bytes, exit $rs)"
exit $status
