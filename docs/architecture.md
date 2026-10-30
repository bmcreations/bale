# How bale works

This is the map for working on bale itself. The README covers what bale does for a user; this covers
where that lives in the code and why it is built that way. Paths are relative to the repo root.

## Crates

| Crate | Role |
|---|---|
| `crates/bale-core` | Everything that decides and moves bytes: the registry, the store, the operations and the sweep. It never names a tool. |
| `crates/bale-hooks` | One adapter per tool (`claude`, `codex`, `cursor`, `conductor`, `t3`, `git`) that turns a tool's hook call into core calls, plus installing those hooks into each tool's config. |
| `crates/bale-cli` | The `bale` binary (clap, `src/main.rs`) and the sweep schedule (`src/install.rs`: launchd, systemd user timer, Task Scheduler). |

`scripts/check-core-agnostic.sh` fails CI when a non-test line in `bale-core/src` mentions a tool by name.
Tool knowledge belongs in `bale-hooks`; core takes paths and flags.

The main modules in `bale-core/src`:

- `registry`: one JSON record per worktree, written to a temp file, fsynced and renamed.
- `lock`: a per-tree file lock. Operations try it without waiting and skip a tree that is locked.
- `busy`: which processes hold a tree (lsof on macOS, `/proc` on Linux; Windows reports none).
- `idle`: the newest source mtime in a tree, skipping `.git` and build output.
- `detect`: build-system markers, and which directories count as build output.
- `derived_data`, `spm`, `spm_link`, `spm_lock`: Xcode DerivedData ownership and the shared Swift package folder.
- `store`: the content-addressed store (`blob`, `codec`, `manifest`, `index`, `gc`).
- `trace`, `history`: the usage trace and the `bale history` summary built from it.
- `ops/*`: one module per operation: `up`, `stash`, `unstash`, `cold`, `open`, `sweep`, `gc`, `new`, `migrate`, `reap` and `spm_gc` (macOS), with helpers in `meta` and `disk`.

## State on disk

Everything lives under `BALE_HOME`, which defaults to `~/.bale`:

| Path | Holds |
|---|---|
| `registry/<id>.json` | One record per worktree: path, repo, level, pin, origin, last activity, stash state. `<id>` is 12 hex characters. |
| `locks/<id>.lock` | The tree's lock. |
| `store/objects/`, `store/manifests/`, `store/index/` | Stashed file contents, one manifest per stash, and a stat index per tree. |
| `<id>/` | A cold tree's saved uncommitted work and ignored files. |
| `dd-roots` | DerivedData roots learned from `-derivedDataPath` and Claude scratchpads. |
| `spm/<key>` | Shared Swift package checkouts. |
| `trace/<date>.jsonl` | The usage trace, one file per UTC day, kept 60 days. See `docs/usage-trace.md`. |
| `restore-failed/<id>.txt` | A restore that failed, reported on the next prompt. |
| `backups/` | Copies of tool config files bale edited. |
| `config.toml` | User settings. A repo's `.bale.toml` overrides them. |

The schedule logs to `~/Library/Logs/bale/sweep.log` on macOS.

## Levels

A record's `level` is one of `hot`, `baling`, `baled`, `cooling`, `cold` or `opening`. `baling`, `cooling` and
`opening` mark an operation in progress. The next sweep finishes `baling` and `cooling` whatever the tree's
idle time, so a crash part way is repaired rather than left behind.

| State | On disk | Gets there by | Comes back by |
|---|---|---|---|
| hot | The full tree. | Creation, `bale open`, or any build. | — |
| stashed | Build files of 1 MiB or more are in the store; each is an empty stub at its original inode. | Sweep, after 10 min idle, on trees a Claude Code hook has touched. | The next prompt starts `bale unstash` in the background; the next tool call waits for it. |
| baled | Build output deleted. | Sweep, after 4 h idle, or `bale up`. | The next build regenerates it. |
| cold | Uncommitted work and ignored files saved under `BALE_HOME/<id>/`, git's worktree entry locked, checkout deleted. | Sweep, after 7 days idle with no uncommitted changes, or `bale cold`. | `bale open`. |

"Stashed" is not a level: it is `record.stash` with `restored` false, on a hot or baled tree.

Idle time runs from the later of the newest source change and the record's `last_activity`, which hooks
bump. `sweep::decide` (`crates/bale-core/src/ops/sweep.rs`) checks, in order:

1. No uncommitted changes and idle for `idle_cold`: cold.
2. Hot (or baled with output that grew back) and idle for `idle_baled`: up.
3. Hot or baled, touched by a restore hook, not stashed, and idle for `idle_stash`: stash.
4. Otherwise keep.

Pinned trees are never upped or colded. Defaults live at the top of `crates/bale-core/src/config.rs`:
`idle_stash` 10 min, `idle_baled` 4 h, `idle_cold` 7 days, `reap_grace` 60 min, `stash_min_bytes` 1 MiB,
`patch_max_depth` 4.

## What counts as build output

`detect` finds build systems by marker at the root or one level down (`settings.gradle(.kts)`,
`Package.swift`, `*.xcodeproj`, `*.xcworkspace`). For those, `build`, `.gradle`, `.kotlin`, `.cxx`,
`.externalNativeBuild` and `.build` are regenerable, as is any directory that is itself a DerivedData root.
`regenerable` in config adds names. Rust `target/` and `node_modules` are not on the list.

`up` keeps a matching directory that holds tracked or unignored files, or that sits inside a nested repo or
submodule, because deleting it would lose something git cannot bring back.

Stash takes less: DerivedData `Build/` (not `SourcePackages` or module caches) and Gradle `build/`. Only
single-link files of `stash_min_bytes` or more go to the store.

Xcode keeps DerivedData outside the tree, in `~/Library/Developer/Xcode/DerivedData` or wherever
`-derivedDataPath` points. Each DerivedData folder records its project's path in `info.plist`. The folder
belongs to the deepest registered tree that contains that project, so a tree nested inside another owns its
own build output.

## Hooks and a Claude Code turn

Only the Claude Code hooks see each turn, so only trees they touch get stashed (`restore_hook` on the
record). The other tools register trees and mark activity; their trees go straight from hot to baled.

| Claude Code event | `bale hook claude …` | Does |
|---|---|---|
| `WorktreeCreate` | `create` | `bale new` on branch `worktree-<name>` and prints the path. Falls back to plain `git worktree add`. |
| `UserPromptSubmit` | `prompt` | Reports an earlier failed restore. If the tree is stashed, bumps its lock so a running stash stops, starts `bale unstash --background` and returns. |
| `PreToolUse` | `pre-tool` | Waits up to 60 s for the restore of the working tree and of any tree the command names (`cd`, `git -C`, `-project`, `-workspace`, `-derivedDataPath`, a file path). Learns DerivedData roots and relinks Swift packages. |
| `Stop` | `stop` | Marks activity. |
| `WorktreeRemove` | `remove` | `cold`, ignoring the hook's own process tree. |

The prompt hook does not wait because a restore can take seconds, and the user would feel that on every
message. The first tool call is where the files are needed. Prompt and pre-tool have a 90 s timeout, above
the 60 s lock wait, so the agent sees bale's own error rather than a hook timeout.

The other adapters, one file each in `crates/bale-hooks/src/`:

- Codex, Cursor and T3 Code run `setup` when they create a tree, which registers it.
- T3 Code's `settle` marks activity; it never ups or colds.
- Conductor's `setup` registers and its `archive` colds the tree.
- The git `post-checkout` hook registers a new linked worktree.

## Stash and the store

Stash moves files into `store/objects/<hh>/<sha256>.zst`. Files are deduplicated whole by SHA-256 and
compressed with zstd level 3. A file that changed since the last stash of the same path is stored as a zstd
`--patch-from` delta against the earlier blob, up to `patch_max_depth` hops. Files or bases over 1 GiB are
stored whole.

Each stash writes a manifest at `store/manifests/<commitment>.json`. The commitment is a SHA-256 over the
roots and the sorted entries (path, root, hash, size, mode, mtime in ns). The record stores it, and reading a
manifest whose hash does not match is an error.

Stubs keep their inode on purpose. Xcode relinks any file whose inode changed, which costs about 45 s on a
large app, so stash truncates in place and never deletes, renames or clones over a stashed path. Nothing is
truncated until the blob and manifest are durable and the record points at the manifest. Unstash refills each
stub in place with its original mode and nanosecond mtime, so the next build compiles nothing. On macOS a
duplicate under `Build/Products/` is cloned from its restored twin instead of decoded twice.

`bale gc`, not the sweep, forgets records whose path is gone. It also deletes blobs, manifests and indexes
no record reaches, once they are over an hour old.

## The sweep

`bale sweep` runs from the schedule: every 5 min on macOS, every 30 min on Linux and Windows. In order:

1. Read the records, skipping unreadable ones.
2. Register worktrees of known repos that no hook saw (20 s limit).
3. macOS: seed the shared Swift package folder and drop stubs whose trees are gone.
4. Index shared DerivedData and measure each tree, in parallel (8 threads, 120 s limit).
5. For each record: decide, then `up`, `cold` or `stash`. Each writes a trace line.
6. macOS: learn DerivedData roots from Claude scratchpads, reap DerivedData whose project is gone, and
   delete unused shared Swift packages.
7. Write the sweep's trace line and prune old trace files.

There is no global sweep lock. Each operation takes the tree's lock without waiting and skips a locked tree,
so a hook always wins over the sweep, and two sweeps never work on the same tree at once.

A tree is busy when any process other than bale (and the hook that called it) has its working directory or an
open file inside it. `up` and `cold` refuse a busy tree; stash skips it and tries again next sweep.

Reap deletes a DerivedData folder only when its project path is missing, no live record holds it, and
nothing in it changed within `reap_grace`.

## Not built

This file started as the "Code VM" design (`git log --follow docs/architecture.md`), which planned more than
shipped. Not built: a shared per-repo base that new trees clone from, reflinked or mounted trees,
`bale wait`/`bale resume`, and a policy for which of a turn's changes to keep. Cold deletes the checkout
rather than mounting it from the store, and the prompt hook restores in the background rather than
blocking.
