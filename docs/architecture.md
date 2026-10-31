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
- `clones`: shared checkouts: file clones (`reflink`), the clone store (`store`), which tracked files can be
  shared (`tracked`), `share` and `materialize` (`share`), and the store's `gc`.
- `trace`, `history`: the usage trace and the `bale history` summary built from it.
- `ops/*`: one module per operation: `up`, `stash`, `unstash`, `cold`, `open`, `sweep`, `gc`, `new`, `migrate`, `reap` and `spm_gc` (macOS), with helpers in `meta` and `disk`.

## State on disk

Everything lives under `BALE_HOME`, which defaults to `~/.bale`:

| Path | Holds |
|---|---|
| `registry/<id>.json` | One record per worktree: path, repo, level, pin, origin, last activity, stash state, the head it was last shared at. `<id>` is 12 hex characters. |
| `locks/<id>.lock` | The tree's lock. |
| `store/objects/`, `store/manifests/`, `store/index/` | Stashed file contents, one manifest per stash, and a stat index per tree. |
| `clones/git/<hh>/<oid>`, `clones/lfs/<hh>/<sha256>` | One read-only copy of each large tracked file, shared by every repo. See "Shared checkouts". |
| `<id>/` | A cold tree's saved uncommitted work and ignored files. |
| `keep/<id>/` | The keep-list: copies of small ignored files (mirrored paths) that survive the tree being deleted and recreated. Separate from `<id>/`, which only a cold tree has. |
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
`patch_max_depth` 4, `share_checkouts` on, `clone_min_bytes` 1 MiB.

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
| `UserPromptSubmit` | `prompt` | Reports an earlier failed restore. Restores kept files the tree lacks. If the tree is stashed, bumps its lock so a running stash stops, starts `bale unstash --background` and returns. |
| `PreToolUse` | `pre-tool` | Waits up to 60 s for the restore of the working tree and of any tree the command names (`cd`, `git -C`, `-project`, `-workspace`, `-derivedDataPath`, a file path). Learns DerivedData roots and relinks Swift packages. |
| `Stop` | `stop` | Marks activity and saves the keep-list. |
| `WorktreeRemove` | `remove` | `cold`, ignoring the hook's own process tree. |

The prompt hook does not wait because a restore can take seconds, and the user would feel that on every
message. The first tool call is where the files are needed. Prompt and pre-tool have a 90 s timeout, above
the 60 s lock wait, so the agent sees bale's own error rather than a hook timeout.

The other adapters, one file each in `crates/bale-hooks/src/`:

- Codex, Cursor and T3 Code run `setup` when they create a tree, which registers it.
- T3 Code's `settle` marks activity; it never ups or colds.
- Conductor's `setup` registers and its `archive` colds the tree.
- The git `post-checkout` hook registers a new linked worktree.

## The keep-list

Hosts such as T3 Code sometimes delete a worktree directory and make it again with `git worktree add`, so
small ignored files an agent wrote there (handoff notes, plans) vanish. The path hashes to the same id, so
the record re-attaches and the keep-list can put them back.

`bale-core/src/keep.rs` is generic: given a `KeepSpec` (relative paths, a per-file cap of 1 MiB and a
per-tree cap of 10 MiB) it copies files into `keep/<id>/` (`save`) and copies back the ones the tree lacks
(`restore`). It never follows a symlink, never replaces an existing file (`persist_noclobber`), takes the
tree lock without waiting, and writes temp file, fsync, rename. Removed files stay in the store; `gc` drops
the store of an id with no record after 30 days. Config keys `keep_paths`, `keep_max_file_bytes` and
`keep_max_total_bytes` come through `config.rs`; `keep_paths` is `None` unless set, because the tool paths
(`.claude/state/`, `.claude/plans/`) are `DEFAULT_KEEP_PATHS` in `bale-hooks`, which core must not name.

`bale-hooks` calls it from Claude Code's `prompt` (restore) and `stop` (restore, then save), and from T3
Code's `settle`. A restore that copies files writes a `restore-kept` op line with `files`. `bale cold` is
unaffected: it saves ignored files under `<id>/` as before, and `bale open` brings them back; the keep store
is a separate copy and also makes a missing file reappear on the next prompt.

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

## Shared checkouts

Tracked files of `clone_min_bytes` or more are kept once in `BALE_HOME/clones/`, and each tree's copy is a
clone of the stored file: `clonefile` on macOS, the `FICLONE` ioctl on Linux. A clone shares the stored
file's blocks until one of them is written, so editing a tree's copy changes only that tree. The code is in
`crates/bale-core/src/clones/`.

A store file is named by its git blob oid (`git/<hh>/<oid>`), or for a Git LFS file by the SHA-256 in its
pointer (`lfs/<hh>/<sha256>`). An oid names the same bytes in any repo, so one store serves them all. Adding
a file clones it to a temp name in the store and sets the copy's mtime to now. It then checks that the
source file's size, mtime, mode, inode and ctime still match what was hashed; if they don't, it removes the
copy and neither stores nor replaces the file. Otherwise it sets mode `0o444` and hard-links the copy to its
final name, so a store file is written once and never replaced; `AlreadyExists` means another tree stored it
first.

`tracked::candidates` reads the index. A candidate is a stage-0 entry with mode `100644` or `100755` and a
blob of at least `clone_min_bytes`. A path whose `filter` attribute is `lfs` and whose blob parses as an LFS
pointer is keyed by the pointer's SHA-256, and the pointer's size is what counts against the cut. Git could
convert the bytes of anything else on checkout, so these are skipped: a path with any other `filter`, a path
with a `text`, `eol`, `ident` or `working-tree-encoding` attribute set, and the whole tree when
`core.autocrlf` is `true` or `input`.

A working file is replaced only when its bytes match its key: `git hash-object --no-filters` gives the
index oid, or for LFS the file's SHA-256 is the pointer's. A modified file is left alone. Replacing clones the
store file to `<file>.bale-share`, sets the original mode and nanosecond mtime, and renames it over the
file; a `<file>.bale-share` left by a crashed run is removed first. `git update-index --refresh` then
updates the index's stat data, so `git status` doesn't hash the new inodes again.

The store and the tree must be on the same volume (`st_dev`), because a clone can't cross volumes. Before
hashing anything, sharing clones a tiny probe file inside `clones/` and removes it. On another volume, on a
filesystem without clones and on Windows, sharing does nothing and says why rather than failing.

Files get shared in three places:

- `bale share TARGET [--ignore-pid PID]` takes the tree's lock without waiting, skips a busy tree, and
  prints `shared N files (X MiB), stored M` or `skipped: <reason>` (`locked`, `busy`, `disabled`,
  `other volume`, `clones unsupported`), followed by `, N errors` when files were left alone for an io
  error. When every file was shared or left alone as modified or missing, it sets the record's
  `shared_head` to the `HEAD` it read under the lock.
- `bale new`, and so the `WorktreeCreate` hook, runs `git worktree add --no-checkout` on macOS and Linux
  and then fills the tree itself: `read-tree HEAD`, a clone for each candidate already in the store, `git checkout-index` for
  every other index path, `update-index --refresh`, and last it verifies the large files git wrote and adds
  them to the store for the next tree. `--no-checkout` skips git's `post-checkout` hook, so bale runs it
  with `git hook run`; a failing hook is reported and the tree is kept. If filling the tree fails, bale
  removes the new tree, and removes the branch only if `bale new` created it. With `share_checkouts` off,
  with `core.sparseCheckout` on in the repo, or on another OS, `bale new` runs a plain `git worktree add`
  instead.
- The sweep shares trees that are baled, unpinned and whose `HEAD`, read with `git rev-parse HEAD`, is not
  their `shared_head`. A hot tree is left alone: a baled tree rebuilds from scratch anyway, and whether a
  build tool rebuilds when a source file gets a new inode is untested. It emits a `share` row only when it
  shared or skipped something. A tree with nothing to share, or skipped as `disabled`, `other volume` or
  `clones unsupported`, gets no row.

`bale gc` builds the live set from the candidates of every record whose path exists; a cold tree adds
nothing. It removes a store file outside that set, or a temp file from an interrupted add, only once its
mtime is over an hour old; adding a file sets that mtime, so a file added in the last hour is kept. If a
record can't be read or a tree's candidates can't be listed, it removes nothing. The
`clones: N bytes freed` it prints is the files' logical size. A store file can still share blocks with a
tree's copy, so the space actually freed can be lower.

`share_checkouts` and `clone_min_bytes` are read from `config.toml` and a repo's `.bale.toml`. `bale new`
reads the main checkout's `.bale.toml`, since the new tree has no files yet; `bale share`, the sweep and
`bale gc` read the tree's own.

On the Android repo, where 117 files of 1 MiB or more hold 943 MiB, `bale share` freed about 940 MiB per
tree once the store held them, and a `bale new` tree took 62 MiB where `git worktree add` took 1,002 MiB
([bench](bench/2026-10-shared-checkout.md#after-implementation)).

## The sweep

`bale sweep` runs from the schedule: every 5 min on macOS, every 30 min on Linux and Windows. In order:

1. Read the records, skipping unreadable ones.
2. Register worktrees of known repos that no hook saw (20 s limit).
3. macOS: seed the shared Swift package folder and drop stubs whose trees are gone.
4. Index shared DerivedData and measure each tree, in parallel (8 threads, 120 s limit).
5. For each record: decide, then `up`, `cold` or `stash`. Each writes a trace line.
6. Share the large tracked files of baled, unpinned trees whose `HEAD` moved since their last share.
7. macOS: learn DerivedData roots from Claude scratchpads, reap DerivedData whose project is gone, and
   delete unused shared Swift packages.
8. Write the sweep's trace line and prune old trace files.

There is no global sweep lock. Each operation takes the tree's lock without waiting and skips a locked tree,
so a hook always wins over the sweep, and two sweeps never work on the same tree at once.

A tree is busy when any process other than bale (and the hook that called it) has its working directory or an
open file inside it. `up` and `cold` refuse a busy tree; stash skips it and tries again next sweep.

Reap deletes a DerivedData folder only when its project path is missing, no live record holds it, and
nothing in it changed within `reap_grace`.

## Not built

This file replaces the "Code VM" design (`git log -- docs/code-vm-design.md`), which planned more than
shipped. Not built: a shared per-repo base that new trees clone from (only large tracked files are
cloned), mounted trees, `bale wait`/`bale resume`, and a policy for which of a turn's changes to keep.
Cold deletes the checkout rather than mounting it from the store, and the prompt hook restores in the background rather than
blocking.
