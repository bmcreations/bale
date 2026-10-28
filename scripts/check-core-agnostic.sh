#!/bin/sh
# Fails when bale-core names a tool outside its tests. Each file is checked
# up to its test module: a column-0 `#[cfg(test)]` followed by a `mod` line.
# Other `#[cfg(test)]` items (test-only statics, branches inside functions)
# do not end the check, since production code follows them.
set -eu
cd "$(dirname "$0")/.."
hits=$(find crates/bale-core/src -name '*.rs' | sort | while IFS= read -r f; do
    awk -v f="$f" '
        pending && /^(pub(\([a-z]+\))? )?mod / { exit }
        { pending = 0 }
        /^#\[cfg\(test\)\][[:space:]]*$/ { pending = 1; next }
        { print f ":" FNR ": " $0 }
    ' "$f"
done | grep -iE 'claude|conductor|cursor|codex' || true)
if [ -n "$hits" ]; then
    echo "bale-core names a tool outside its tests:" >&2
    echo "$hits" >&2
    exit 1
fi
echo "bale-core names no tool outside its tests"
