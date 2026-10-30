# Working on bale

bale is a Rust CLI that keeps agent worktrees from filling the disk. `docs/architecture.md` explains how the
code is laid out and why; read it before changing an operation, the store or a hook. The README is the user
guide.

## Build and check

```
cargo build -p bale-cli
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked --no-fail-fast
scripts/check-core-agnostic.sh
```

Run the narrowest test first (`cargo test -p bale-core --test <file>` or `cargo test -p bale-cli <name>`), then
the workspace. CI runs the tests on macOS, Linux and Windows; the last two scripts run on Linux only.

## Don't touch the real machine

bale deletes build output and checkouts, so a test or a manual run against your real `HOME` can delete real
work.

- Tests isolate themselves: `crates/bale-core/tests/support/mod.rs` (`Fixture`, `isolate_home`) and
  `Env::isolate` in `crates/bale-cli/tests/cli.rs`, which sets `HOME`, `USERPROFILE`, `BALE_HOME`,
  `BALE_CLAUDE_TMP` and the git config to temp paths. New tests use these helpers.
- To try a change by hand, run `cargo run -p bale-cli --` with `HOME`, `BALE_HOME` and `BALE_CLAUDE_TMP`
  pointed at a temp directory. Don't run an installed `bale` (for example `~/.local/bin/bale`) or
  `bale install` while developing: the installed copy is the one managing your worktrees, and `install`
  changes the real launchd, systemd or Task Scheduler entry.
- `bale install --print-only` and `bale sweep --dry-run` show what would happen without doing it.

## Rules the code depends on

- `bale-core` never names a tool. Tool-specific parsing and config go in `bale-hooks`;
  `scripts/check-core-agnostic.sh` enforces it.
- Anything that deletes checks the tree's lock and the busy rule first, and keeps directories that hold
  tracked or unignored files. Don't add a delete path that skips those.
- Stash keeps each stub at its original inode and truncates only after the blob, the manifest and the record
  are durable. Unstash restores mode and nanosecond mtime. Breaking either makes Xcode relink or loses data
  on a crash.
- Records are written to a temp file, fsynced and renamed. Go through `registry` instead of writing JSON
  directly.
- Operations take the per-tree lock without waiting and skip a locked tree, so a hook always beats the sweep.
- Hooks run on every prompt and tool call. Keep `pre-tool` and `prompt` fast, and write only to stderr unless
  the event uses stdout (`WorktreeCreate` prints the path).
- Tests that write files with Windows in mind: the runner sets `GIT_CONFIG_NOSYSTEM=1`, so `core.autocrlf`
  is off unless a test sets it in the repo config.

## Docs

- `README.md`: the user guide. Update it when a command, flag or default changes.
- `docs/architecture.md`: how bale works. Update it when a level, the sweep order, the store format or a hook
  changes.
- `docs/usage-trace.md`: the trace format.
- `docs/bench/`: measured results; each file says how it was measured.
- `docs/superpowers/specs/` and `docs/superpowers/plans/`: the design and plans as written at the time. They
  are history, not a description of the current code.

## Commits and pull requests

Conventional commit titles with a scope where one fits (`feat(cli): …`, `fix(sweep): …`, `docs: …`). PRs are
squash-merged, so the PR title becomes the commit on `main`.

## Releasing

The workspace version (`Cargo.toml`), `plugins/bale/.claude-plugin/plugin.json` and `BALE_VERSION` in
`plugins/bale/scripts/bale` must match; CI checks them with `scripts/check-release-version.sh`. To release:

1. Bump all three in one PR and merge it.
2. Tag `main`: `git tag vX.Y.Z && git push origin vX.Y.Z`.

`.github/workflows/release.yml` then builds macOS, Linux and Windows archives, publishes the GitHub release
with a `.sha256` per archive, and writes `bale.rb` to `bmcreations/homebrew-tap` using the
`HOMEBREW_TAP_TOKEN` secret. The plugin downloads the release named by `BALE_VERSION`, so a plugin update
and its binary move together.
