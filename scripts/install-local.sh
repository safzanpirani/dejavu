#!/usr/bin/env bash
# Builds the release binary and installs it as $DEST/dejavu (default ~/.local/bin) with a deja
# symlink. The binary goes in through a temporary file and a rename: copying over a running
# binary in place gets it killed on macOS.
#
#   scripts/install-local.sh
#   DEST=/tmp/dejavu-bin scripts/install-local.sh
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
dest=${DEST:-$HOME/.local/bin}
cd "$root"

if command -v mbx >/dev/null 2>&1; then
  mbx build --release -p dejavu
else
  cargo build --release -p dejavu
fi

binary="${CARGO_TARGET_DIR:-$root/target}/release/dejavu"
[ -x "$binary" ] || { echo "install-local: no binary at $binary" >&2; exit 1; }

mkdir -p "$dest"
temporary="$dest/.dejavu.$$.tmp"
trap 'rm -f "$temporary"' EXIT
cp "$binary" "$temporary"
chmod 755 "$temporary"
mv -f "$temporary" "$dest/dejavu"
ln -sfn dejavu "$dest/deja"
echo "installed dejavu $("$dest/dejavu" --version) at $dest/dejavu (deja -> dejavu)"
