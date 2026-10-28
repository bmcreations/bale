# bale: design

Status: draft for review, 2026-10-27.

## Problem

Agent sessions each get their own git worktree, and each worktree carries its own full build. On the
machine this design was measured on, 31 worktrees across two app repos hold 110 GB. 92 GB of that is
regenerable build output. Another 16 GB sits in the shared Xcode DerivedData folder. Running 4 or more
agent sessions, which means 7 to 12 worktrees at once, fills the disk within hours.

| Repo | Worktrees | On disk | Regenerable build output | With uncommitted changes |
|---|---|---|---|---|
| code-android-app (Gradle, Kotlin, native CMake) | 18 | 57.0 GB | 41.3 GB | 3 |
| code-ios-app (Xcode, SwiftPM) | 13 | 53.0 GB | 51.2 GB | 3 |

Worktrees share `.git`, so the source itself is cheap. A single Android worktree is about 1 GB of source and
1.5 to 7.9 GB of build output. A single built iOS worktree is about 140 MB of source and 4 to 12 GB of
`.build/` (a per-worktree DerivedData).

`disk-jockey` (an existing cleanup skill) reclaims this space after the disk is already full. It made the
problem less frequent but did not remove it.

## Goal

A tool, `bale`, that manages agent workspaces so disk use tracks the work that is active right now, not
the number of features that exist.

Success is one concrete test: with 4 or more agent sessions running (7 to 12 hot workspaces) for a working
day, the disk does not fill. Reclaiming space from idle worktrees only counts if this holds.

The acceptance run has two parts, both on this machine:

1. `bale migrate --apply` on the existing worktrees, recording `df` free space before and after.
2. A working day of 4 or more sessions creating workspaces through `bale`, sampling `df` free space every
   15 minutes. It passes if free space never drops below 20 GB. *(My call on the 20 GB floor.)*

`du` cannot be used for either because it counts copy-on-write clones twice.

### Non-goals

- Preserving simulator or emulator state across a bale.
- Sharing build output between machines. Everything here is per machine.
- Replacing git. Branches, commits and worktree metadata stay in git.

### Anti-goals

These would make the tool a failure even if disk use dropped:

- Losing uncommitted work or ignored files that matter, such as `.env`, `local.properties`,
  `google-services.json` or `*.local.xcconfig`.
- Deleting anything while a build, daemon or IDE is using it.
- Requiring every developer to change filesystems or run as admin to get the core benefit.

## Decisions

### Resume priority: warm build first, then uncommitted state

When an agent comes back to a baled feature, the build should be warm and its uncommitted changes should be
exactly where they were. Nothing else needs to survive.

### What the measurements changed

The first idea was a content-addressed store that deduplicates build output across worktrees, borrowed
from the Code VM's compressed accounts. Measurement ruled it out.

- Byte-identical build output between two independently built Android worktrees: 74 of 1,110 MB (7%). iOS:
  0%. Independent builds produce different bytes even from mostly identical code, so deduplicating after
  the fact recovers little.
- A copy-on-write clone (`cp -c -R`, APFS) of a built 4.6 GB Android worktree takes 16 s and about 40 MB.
- Building that clone at its new path, with no source changes, re-ran 1,077 Gradle tasks and restored
  1,505 from the build cache in 36 s. It rewrote 2.16 GB of outputs, so those blocks stop being shared.
  Free space dropped 1.8 GB. The re-run tasks are the per-module `merge*`, `bundle*` and `process*` steps,
  which have absolute-path inputs.
- At the same path, a no-change build took 14 s and a one-line Kotlin edit took 31 s and rewrote 24 MB.

Two things follow. A build tree is only cheap to keep while it stays at the path it was built at. And a
clone at a new path still costs about 2 GB instead of 4.6 GB, because source and native outputs stay shared.

The spike used `:apps:flipcash:app:assembleDebug --offline --no-daemon` on `code-android-app-polls`.
iOS was not measured. Absolute paths are present throughout its build output (`info.plist`, SwiftPM
`workspace-state.json`, module caches, compile file lists), and the shared DerivedData folder name is a hash
of the project path, so expect iOS to diverge at least as much.

### Two tiers

**Tier 1, all platforms, no special filesystem.** Bale an idle workspace by removing its regenerable build
output in place. The checkout stays at its path, so restoring is just building again, and Gradle's local
build cache (already enabled on this machine, 3.9 GB) refills most of it. This covers the 92 GB above and
works the same on macOS, Linux and Windows.

**Tier 2, copy-on-write, opt-in per platform.** Bale a repo once into a warm base: a fully built checkout of
the default branch. Each new session's workspace is a copy-on-write clone of that base, so it starts warm
and shares source and native build blocks with it. Based on the spike, this is about 2 GB per hot Android
workspace instead of 4.6 GB, and it is the only lever on the cost of hot workspaces. macOS ships first.

The first release needs both. Tier 1 alone cannot pass the acceptance run: every hot Android workspace
still costs a full build, and 12 of them can exceed the free space on this machine. So the first release
is tier 1 on macOS, Linux and Windows plus tier 2 on macOS. Tier 2 on the other platforms follows as each
backend is tested.

Build order:

1. Run the iOS clone spike, the same experiment as the Android one, because its result decides whether iOS
   gets tier 2.
2. Build tier 1, then `bale migrate`, and run acceptance part 1.
3. Build tier 2 on macOS, then run acceptance part 2.

### Two bale levels

| Level | What is removed | What is kept | Resume |
|---|---|---|---|
| `baled` | Regenerable build output | Checkout, uncommitted changes, ignored files, all at the same path | Run a build. Nothing needs restoring, because the tree is still a valid checkout. |
| `cold` | Build output and the checkout itself | Branch and commits in git; uncommitted changes under `refs/bale/<id>/wip`; ignored-but-important files in `~/.bale/` | `bale open <name>` checks out again at the same path and restores the saved files. |

`baled` is the default after an idle threshold. `cold` happens after a longer threshold or on request. It
saves the remaining source, about 1 GB per Android worktree.

`baled` needs no `bale open`. An agent or harness that enters a baled worktree sees a normal checkout and
the next build fills it back in. Only `cold` needs an explicit open.

### Command is the centre; hooks are thin callers

`bale` is a CLI. Every integration calls it rather than reimplementing any part of it.

```
bale install            # set up the periodic sweep and the optional hooks
bale migrate [--apply]  # adopt existing worktrees; dry run by default
bale new <name>         # create a workspace (tier 2: clone from the repo's base)
bale base [--refresh]   # tier 2: build or rebuild the repo's warm base
bale up <name>          # bale now
bale cold <name>        # bale to cold
bale open <name>        # restore a cold workspace
bale ls                 # registry with state, size, last activity
bale sweep              # bale everything idle past the thresholds
bale gc                 # remove orphans and expired saved state
```

Integrations, in order of how fully they hand control to `bale`:

1. **Agents call `bale` directly**, taught through a skill or an AGENTS.md snippet.
2. **Claude Code `WorktreeCreate` and `WorktreeRemove` hooks** (documented at
   code.claude.com/docs/en/hooks). `WorktreeCreate` replaces Claude Code's built-in git worktree creation.
   Its contract:
   - It prints only the absolute path on stdout, so nothing else may write to stdout.
   - It has no matcher and applies to every repo.
   - It skips `.worktreeinclude`.

   Its gaps:
   - It does not fire for `git worktree add` run through Bash.
   - `WorktreeRemove` does not fire when a subagent's worktree is cleaned up.

   Remove maps to `bale cold`, not to deletion. Install the hook in settings rather than a plugin, because
   plugin hooks do not run with `--worktree --tmux`. Because a broken hook would block worktree creation
   in every repo, `bale install` adds it only when asked. If `bale new` fails inside the hook, the hook falls
   back to a plain `git worktree add` and prints that path.
3. **git `post-checkout`** fires on `git worktree add` with the arguments `<null-ref> <new-HEAD> 1`. It only
   registers the worktree with `bale` so the sweep can manage it. It does not run with `--no-checkout`, and
   a global `core.hooksPath` would override per-repo husky or lefthook hooks, so it must chain to those
   rather than replace them.
4. **`bale sweep` on a schedule** catches everything else, including worktrees created by T3 or by plain git.

No `git` wrapper on PATH. It would affect every git call on the machine, and tools that run git by absolute
path would bypass it anyway.

### The registry and the data

The roles follow the split Tapedrive uses between a small control plane and a bulk data plane.

- **Registry, the control plane.** Small and authoritative: one record per workspace with id, repo, path,
  branch, level, last activity, and saved-state location. It must survive a crash at any point.
- **Data.** Build output stays in the build tools' own places. Gradle's cache stays in `~/.gradle`. Saved
  ignored files live under `~/.bale/<id>/`. Uncommitted changes live as git objects under
  `refs/bale/<id>/*` in the repo, so normal `git gc` keeps them reachable.

The compress and decompress steps follow the Code VM's `compress` and `decompress` instructions. The state
is captured and verified first, and only then is the hot copy deleted. On the way back, the state is checked
against what was captured before it is used.

## How a bale works

### `bale up` (to `baled`)

1. Take the workspace's lock. Refuse if another `bale` operation holds it.
2. Check it is idle. Refuse if any process has its cwd or an open file inside the tree. On Windows a delete
   would also fail with a sharing violation, so check there first rather than relying on it. Never stop
   another session's Gradle daemon, Xcode or IDE.
3. `git worktree lock --reason "baled by bale"`, so `git worktree prune` and `gc.worktreePruneExpire`
   never remove its metadata.
4. Write the registry record as `baling`.
5. Rename each regenerable directory to a trash name inside the workspace, then delete the trash names.
   This makes a crash leave either a full directory or a trash name that the next run finishes deleting.
6. Remove the workspace's external build output too. That includes Xcode DerivedData folders under
   `~/Library/Developer/Xcode/DerivedData` whose `info.plist` `WorkspacePath` points into this workspace.
   A folder with a missing or unreadable `WorkspacePath` is left alone. So is global DerivedData that matches
   no registered workspace; cleaning those stays with `disk-jockey`.
7. Write the registry record as `baled`.

The regenerable set is a list per build system, not a guess.

- Gradle: `build/`, `.gradle/`, `.kotlin/`, `.cxx/`, `.externalNativeBuild/`.
- Xcode: the workspace's DerivedData, wherever `-derivedDataPath` points it (`.build/` on this machine), and
  matching global DerivedData folders.
- SwiftPM: `.build/`.

The list is configurable per repo. Anything not on it is never touched in this level.

### `bale cold`

Everything in `bale up`, then:

1. Save uncommitted changes, both tracked and untracked, as a commit object under `refs/bale/<id>/wip`, with
   `refs/bale/<id>/head` pointing to the commit it sits on. Do not put them on the user's branch. Several of
   these worktrees use a detached HEAD, and commits that nothing references are eventually garbage collected.
2. Save ignored files that are not regenerable to `~/.bale/<id>/files/`. Selection uses a denylist: every
   ignored file except the regenerable directories, `node_modules`, and similar entries from a configurable
   list. An allowlist would miss the next secret someone adds. Files are stored with mode 0600, never in any
   shared store.
3. Verify the saved state by hashing it against the tree.
4. Remove the checkout directory. The worktree stays locked in git.
5. Write the registry record as `cold`.

### `bale open`

1. Check out the branch at the same path. If the directory was moved, use `git worktree repair`.
2. Apply `refs/bale/<id>/wip` and restore the saved files, checking hashes.
3. Rewrite machine-specific absolute paths that are known to break, such as `sdk.dir` in
   `local.properties`. This only matters if the path or machine changed.
4. Unlock the worktree and mark it hot.

### Idle

A workspace is idle when nothing has its cwd or a file open inside it, and no source file has changed for the
threshold. Defaults are 4 hours to `baled` and 7 days to `cold`, both configurable. Gradle expires local build
cache entries that go unused (7 days by default), so a workspace baled longer than that resumes colder.

A busy workspace is skipped and reported, never forced. Long-lived daemons do not usually block this. An
`lsof` snapshot of the 29 worktrees on this machine, taken while Gradle daemons, a JetBrains IDE and several
agent sessions were running, found open files in only one worktree (a `java` process with 9 handles) and a
process cwd in two. The other 26 had nothing open. On macOS, Gradle's file-system watching uses FSEvents,
which holds no file handles in the tree. *(Builder's call: if this turns out different on Linux or Windows,
add an opt-in flag to stop a Gradle daemon whose only registered project is the workspace being baled.)*

### Unattended sweeps never cold a dirty tree

`bale sweep` moves a workspace to `cold` only if it has no uncommitted changes. Dirty trees stay `baled`,
which keeps their changes on disk and still removes the build output, the bulk of the space. `bale cold` run
by hand works on dirty trees. *(My call: auto-cold of dirty trees is the one path that could lose work, and
the source it saves is about 1 GB per Android worktree.)*

### Cold edge cases

*(My calls, chosen to fail loudly rather than guess.)*

- The wip commit holds tracked and untracked files. Ignored files go to `~/.bale/<id>/files/` instead.
  Symlinks are saved as symlinks, because the existing `seed-worktree-env.sh` session hook links `.env`
  into worktrees from the main checkout.
- `open` checks out the commit in `refs/bale/<id>/head`, so the wip always applies cleanly. If the
  workspace was on a branch and that branch still points to the same commit, it checks out the branch
  instead. If the branch has moved, it stays detached at the saved commit and says so.
- If the branch is checked out in another worktree, `open` stays detached at the saved commit and reports it.
- `cold` refuses trees with submodules that have their own uncommitted changes, and trees using sparse
  checkout. LFS pointer files need no special handling, because the checkout restores them.

### Configuration

*(My call.)* Per-repo settings live in an optional `.bale.toml` committed at the repo root. Machine settings
live in `~/.bale/config.toml`. Repo settings override machine settings. Build systems are detected from
marker files: `settings.gradle(.kts)`, `*.xcodeproj` or `*.xcworkspace`, and `Package.swift`. The tool is
generic. The two app repos are the first targets and the acceptance test, not the only supported repos.

### Git

*(My call.)* `bale` runs the `git` binary rather than using a library, so behavior matches what agents see.
Commands that hit `index.lock` or a ref lock held by an agent's git operation are retried with backoff,
then reported. `bale` never deletes a lock file.

## Tier 2: the repo's warm base

`bale base` creates, per repo, a full checkout of the default branch at a fixed path under `~/.bale/bases/`
and builds it. `bale new <name>` clones it into the new workspace's path with the platform's copy-on-write
call, creates the git worktree metadata for it, checks out the branch, and builds.

What is known from the spike: the clone is near-instant, and the first build at the new path takes about 36 s
and diverges by about 2 GB on Android. What is not known: the iOS numbers, and how much the base drifts as
the default branch moves. Checking out a feature branch in the clone rewrites only the files that differ from
the default branch, so the extra divergence depends on the size of the feature. That was not measured.

Tier 2 is enabled per build system, and only where a measured spike shows at least a 40% saving per hot
workspace compared to a separate worktree. Android met that bar at about 55%. If iOS does not meet it, iOS
workspaces use tier 1 only. *(My call on the threshold.)*

Refreshing the base has a cost. A clone keeps the old base's blocks allocated until the clone itself is
deleted or baled, so for a while two generations of the base are on disk. The design rebuilds the base on a
schedule, at most daily, and reports how much space older generations still hold.

### Platforms

Tier 1 is required on macOS, Linux and Windows in the first release, and is tested on all three in CI. Tier 2
is required on macOS in the first release and ships on the other platforms as each backend is tested.

| Platform | Tier 1 | Tier 2 copy-on-write |
|---|---|---|
| macOS (APFS) | Yes | `clonefile`, which clones whole directory trees in one call. Ships first. |
| Linux (btrfs, XFS) | Yes | `FICLONE` per file. `bale` walks the tree itself. Same filesystem only. |
| Linux (ZFS) | Yes | Only on OpenZFS 2.2.2 or later, and only with explicit opt-in. 2.2.0 had silent data corruption that block cloning made more likely. |
| Linux (ext4) | Yes | None. Tier 1 only. |
| Windows (Dev Drive, ReFS) | Yes | `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, which needs Windows 11 24H2. Clones must be cluster-aligned and under 4 GB per call. Dev Drive needs admin to create, 50 GB or more, and cannot be the C: drive. Gradle file-system watching is unsupported on ReFS, which slows builds. |
| Windows (NTFS) | Yes | None. Tier 1 only. |

Rejected fallbacks:

- Hard links: build tools write into files in place, which would corrupt every workspace sharing them.
- Rootless overlayfs: Ubuntu 24.04 restricts unprivileged user namespaces by default.
- ProjFS: it fills files in lazily but stores full copies, so it saves no space.

The `reflink-copy` crate covers macOS and Linux file clones, but its own documentation calls the Windows
backend untested. Treat it as a starting point and test each backend separately.

## Migration

`bale migrate` adopts every existing worktree of the repos it is pointed at.

- Default is a dry run. It shows each worktree, its size, its regenerable size, whether it is dirty, and what
  would happen. `--apply` acts.
- It never removes a worktree, never discards uncommitted changes, and never touches a busy tree. It takes
  disk-jockey's safety contract as-is.
- It registers every worktree, bales the idle ones, and leaves busy ones hot.
- It records `df` free space before and after, which is the acceptance measurement.
- Worktrees under temp directories such as `/private/tmp/...` are reported separately. macOS may purge them,
  and they belong to whatever tool created them.

On this machine the regenerable total is 92.5 GB across 31 worktrees, plus 16 GB of global DerivedData.

## Later: devices for mobile projects

This is a separate module, planned after tier 1 ships. It is off for repos with no iOS or Android target.

Simulators and emulators use as much disk as the worktrees do. Measured on 2026-10-27:

- `~/Library/Developer/CoreSimulator/Devices` takes 72 GB across 15 devices, the two largest at 22 GB and
  12 GB.
- `~/.android/avd` takes 26 GB. Two of its AVDs (`flipcash_blockstore` and `invites_android`) are already
  named after features.

How the module works:

- Each workspace can own one device. It is created from a base device: `xcrun simctl clone` on iOS (a
  copy-on-write clone on APFS), or a new AVD on the shared system image on Android.
- Device state is not preserved. Baling a workspace wipes its device's data, and colding it deletes the
  device. On open, a fresh device is created from the base.
- `bale` never boots, pauses or stops a device. It only creates, wipes and deletes, through `xcrun simctl` and
  `avdmanager` and by deleting AVD files.
- loupe is optional. When it is installed, `bale` checks loupe's claims and skips any device that another
  session has claimed or that the user has marked hands-off. Without loupe, `bale` skips any device that is
  booted.
- `migrate` reports devices by size and never links or deletes them on its own. The user picks which device
  belongs to which workspace.

## Packaging

- Written in Rust, as a single static binary per platform.
- Crates: `bale-cli` builds the `bale` binary, `bale-core` holds the registry, lifecycle and migration, and
  `bale-clone` holds the tier 2 copy-on-write backends later. The bare `bale` crate name on crates.io is
  already taken by an unrelated pack-format crate.
- Distributed through Homebrew and `cargo install bale-cli` first. A Windows installer comes with tier 2 on
  Windows.

## Left to the builder

- Registry format and location, as long as updates are atomic and crash-safe.
- How the sweep is scheduled on each OS (launchd, a systemd user timer, Task Scheduler).
- How `bale` detects open files per OS (`lsof` or `proc_pidinfo`, `/proc`, the Restart Manager API).
- Output formats, with machine-readable output available for agents.
- Exact regenerable-directory lists beyond the ones above.

## Open questions

- **iOS hot cost under tier 2.** Not measured. Run the same spike on `code-ios-app` before building tier 2.
- **T3's worktree creation** does not go through Claude Code hooks. Whether T3 exposes its own hook is
  unknown. Until it does, T3 worktrees are picked up by the sweep.
- **Same-path restore was inferred, not run.** The spike showed no-change builds at the original path are
  up to date. A full `bale up`, then build cycle on a real worktree still needs to be timed.

## Risks

- **Hot workspaces still cost disk.** With tier 1 alone, every hot Android workspace costs a full build,
  1.5 to 7.9 GB on this machine. Twelve hot workspaces could exceed the 78 GB currently free even with every
  idle one baled. Tier 2 brings an Android workspace to about 2 GB, so 12 hot ones are about 24 GB. If iOS
  does not qualify for tier 2, a mixed day of hot iOS workspaces at 4 to 12 GB each could still fail the
  acceptance run. Linux on ext4 and Windows on NTFS have no tier 2 at all.
- **Delete races.** A Gradle daemon or IDE can recreate `build/` while it is being deleted. The idle check
  reduces this. It does not remove it.
- **Build-cache expiry.** Resumes after more than about 7 days are colder than "warm".
- **Base generations.** Under tier 2, refreshing the base keeps the previous generation's blocks allocated
  until every clone of it is gone.
