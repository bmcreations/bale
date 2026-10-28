#!/bin/sh
# Fails when bale-core names a tool outside its tests. A test module, a
# column-0 `#[cfg(test)]` followed by a `mod` line, is skipped up to its
# closing brace (a column-0 `}`, as rustfmt writes it), and the lines after
# it are checked again. Other `#[cfg(test)]` items (test-only statics,
# branches inside functions) are checked like any other code.
set -eu
cd "$(dirname "$0")/.."
hits=$(find crates/bale-core/src -name '*.rs' | sort | while IFS= read -r f; do
    awk -v f="$f" '
        intest { if (/^}/) intest = 0; next }
        pending && /^(pub(\([a-z]+\))? )?mod / {
            pending = 0
            # `mod tests;` has no body to skip.
            if ($0 !~ /;[[:space:]]*$/) intest = 1
            next
        }
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
