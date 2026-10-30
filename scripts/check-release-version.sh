#!/usr/bin/env bash
# Fails unless the workspace version, the Claude Code plugin's version and
# the version its launcher downloads all equal $1 (a tag without the v).
set -euo pipefail
want="$1"
root="$(cd "$(dirname "$0")/.." && pwd)"
cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
plugin=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$root/plugins/bale/.claude-plugin/plugin.json")
launcher=$(sed -n 's/^BALE_VERSION="\(.*\)"/\1/p' "$root/plugins/bale/scripts/bale")
status=0
for pair in "Cargo.toml:$cargo" "plugin.json:$plugin" "plugins/bale/scripts/bale:$launcher"; do
  if [ "${pair#*:}" != "$want" ]; then
    echo "error: ${pair%%:*} has version '${pair#*:}', tag is $want" >&2
    status=1
  fi
done
exit $status
