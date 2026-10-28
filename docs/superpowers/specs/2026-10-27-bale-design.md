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
The iOS spike was measured on 2026-10-27. It cloned `code-ios-app-hotswap` (about 12 GB: 6.2 GB DerivedData
in `.build/dd` and 5.8 GB of packages in `.build/ds`) and ran its own command,
`xcodebuild build -scheme Flipcash -configuration Debug -destination generic/platform=iOS -derivedDataPath .build/dd`.

| Build | Time | Rewritten | Free space change |
|---|---|---|---|
| Clone of a warm tree at a new path, module cache kept | 70 s | 256 MB | build failed |
| Same, with `ModuleCache.noindex` deleted after cloning | 149 s | 3.6 GB (all 3,109 Swift files recompiled) | −3.6 GB |
| Same tree again, no changes, fixed path | 27 s | 135 MB, no compile steps | −23 MB |

- Clang's module cache records its absolute path, and a moved cache makes the build fail with
  `missing required module 'SwiftShims'`. Any clone to a new path must delete `ModuleCache.noindex` first.
- With the cache deleted, the new path recompiles everything. That gives no speed gain, but the packages and
  the rest of the build stay shared. A hot iOS workspace costs about 3.6 GB, compared with about 12 GB as a
  separate worktree.
- At a fixed path, iOS stays warm, as Android does.

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
2. Skip the tree if git has it locked with a reason other than bale's. Someone else locked it on purpose.
3. Check it is idle. Refuse if any process has its cwd or an open file inside the tree. On Windows a delete
   would also fail with a sharing violation, so check there first rather than relying on it. Never stop
   another session's Gradle daemon, Xcode or IDE.
4. Write the registry record as `baling`.
5. Rename each regenerable directory to a trash name inside the workspace, then delete the trash names.
   This makes a crash leave either a full directory or a trash name that the next run finishes deleting.
   The trash names (`.bale-trash-<name>-<digits>`) are chosen and written to the `baling` record before
   anything moves, and removal, on the first run or a resume, touches only those recorded paths. Each must
   still carry bale's trash name, sit under the root outside any nested repository, and hold no `.git`, or
   `up` errors and keeps it. A user file that happens to start with `.bale-trash-`, or another worktree's
   trash nested in the tree, is never removed.
   A directory with a regenerable name that holds files git tracks, or untracked files that are not
   ignored, is kept and reported: it is source, not build output. A regenerable-named entry that is itself
   untracked and not ignored, including a symlink, is kept the same way. So is a regenerable-named directory
   inside a submodule or a nested repository (any directory between the workspace root and it, or it, has a
   `.git` entry): the workspace's git can't see what it holds. A nested repository inside an ignored
   regenerable directory, such as `build/x/.git`, is treated as part of that directory's output: it is
   removed with it, and `cold` does not save it.
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

`up` does not lock the worktree in git. The git lock only stops `git worktree prune` and
`gc.worktreePruneExpire` from erasing the metadata of a worktree whose directory is gone, and a baled tree's
directory is still there. Leaving it unlocked means a tool that runs `git worktree remove` (Claude Code, T3)
can remove a baled tree; `bale gc` then collects its record. Trees baled by an earlier version still carry
bale's lock, and `open`, `cold` and `gc` handle them as before.

### `bale cold`

Everything in `bale up`, then:

1. `git worktree lock --reason "baled by bale"`, so `git worktree prune` and `gc.worktreePruneExpire` never
   remove its metadata once the directory is gone. Write the registry record as `cooling`, with the branch
   checked out now (not the one at registration), so `open` returns to it.
2. Save uncommitted changes, both tracked and untracked, as a commit object under `refs/bale/<id>/wip`, with
   `refs/bale/<id>/head` pointing to the commit it sits on. Do not put them on the user's branch. Several of
   these worktrees use a detached HEAD, and commits that nothing references are eventually garbage collected.
3. Save ignored files that are not regenerable to `~/.bale/<id>/files/`. Selection uses a denylist: every
   ignored file except those inside the regenerable directories `up` actually removes, `node_modules`, and
   similar entries from a configurable list. An ignored file inside a regenerable-named directory that `up`
   keeps is saved. Inside a submodule, ignored files under a regenerable name count as regenerable and are not
   saved. An allowlist would miss the next secret someone adds. Files are stored with mode 0600, never in any
   shared store. The manifest also records the permission bits of every regular file and directory `open`
   writes, and of the tree's root (on unix), and every directory with nothing in it, since git keeps
   neither a full mode nor empty directories. Hard links are not preserved: each link comes back as a
   separate file with the same bytes, so no data is lost.
4. Verify the saved state by hashing it against the tree. This in-place check is an early refusal; the
   safety of the delete rests on step 5.
5. Rename the checkout directory to a trash name and check the moved tree against what `open` would give
   back. bale runs `open`'s own restore (the wip commit read into a private index and checked out, then the
   saved files, modes and empty directories) into a directory beside the trash, and compares the two with
   plain file reads, not through git: the path set both ways by exact name, file types, bytes, modes on
   unix (files, directories and the root), and symlink targets. The check dir is checked out with HEAD
   where it was in the tree, so config that depends on the branch (`includeIf "onbranch:..."`) applies as
   it will at `open`. The walk skips only the root's `.git`, the regenerable directories `up` removed, and
   what step 3 skips (`node_modules` and the denylist). A skipped directory still has every path the wip
   commit holds under it walked and compared, since `open` writes those. Any entry, or the tree's root,
   with an extended attribute other than the ones the OS or apps set on their own (`com.apple.provenance`,
   `com.apple.quarantine`, `com.apple.lastuseddate#PS`, `com.apple.macl`, `com.apple.TextEncoding`,
   `security.selinux`, and `com.apple.FinderInfo` on `.DS_Store` only) counts as a difference: nothing
   saves xattrs, and a Finder tag or colour label is the user's. On macOS the same goes for a file flag
   (`chflags`) other than `UF_COMPRESSED` and `UF_TRACKED`, which the OS keeps up itself (`hidden` is
   allowed on `.DS_Store` only, where Finder sets it; `uchg`, `nodump` and a dataless iCloud file refuse),
   and for an extended ACL with any entry. On Linux an ACL is a `system.posix_acl_*` xattr, so the xattr
   rule covers it. Linux `chattr` attributes are not checked.

   Each checked-out submodule, nested ones too, is restored the same way: its commit in the wip tree is
   checked out from the submodule's own git dir (so its own config applies, as it will for `git submodule
   update`) and compared byte for byte. Inside a submodule the walk also skips its `.git`, its regenerable
   directories and the root's that fall inside it, and the deny and `regenerable_extra` entries rebased
   onto it. A file whose mode the checkout would not give back, such as a 0600 file, refuses. A wip commit
   whose gitlink differs from HEAD's (a submodule moved to another commit, or one added) refuses, because
   `open` resets the index to HEAD and `git submodule update` would then check out HEAD's commit; a
   deleted submodule directory goes cold, since `open` gives that back as a deletion. Submodules
   are also checked through git: a tracked edit, an untracked file, a new commit, or an ignored file that
   is not regenerable inside a submodule counts as a change, since the wip commit holds only the
   submodule's commit.

   The compare hashes (sha256) every regular file it reads outside submodules, from the bytes it compared,
   records the target of every symlink outside submodules, and keeps the stat (device, inode, size, mode, mtime and ctime on unix; size, mtime and creation time
   elsewhere) of every entry it walked. Then check again for processes holding the moved tree, and stat
   every entry again: a process that opened the tree after the busy check, or wrote after the compare read
   a file, could have changed it. A rewrite that keeps the size and lands within the filesystem's mtime
   granularity can still slip past the stat. On any change, move the tree back and return the record to
   `baled`. A disk difference is reported as skipped, naming the first
   differing path; a submodule change or a new holder is reported busy (with the holders, when there are
   some). If a check itself fails, move the tree back the same way and report the error. Only when both checks pass,
   write the hashes and symlink targets into the manifest, record the trash name and the wip commit it
   was checked against, then remove the trash directory. The worktree stays locked in git. If the removal
   fails partway, the record stays `cooling` and the error names the first entry that could not be
   removed, with what usually blocks it (a directory without write permission, an immutable flag, an ACL
   that denies delete); the next `cold` finishes the removal.

   The check dir is named `.bale-check-<id>-<6 random characters>` and removed when the check ends. One a
   crash leaves (it holds copies of the tree's files, saved secrets included) is removed by the next
   recovery of that workspace or by `bale gc`, both under the workspace's lock. `bale gc` lists each one it
   removes, and a dry run lists the ones it would remove. Other workspaces' check dirs in the same parent
   are left alone.

   Because the compare reads the disk, a tree whose files git would not give back byte for byte stays:
   a clean filter that changes content, `core.autocrlf=input` on a CRLF file, a new `ident` file holding an
   unexpanded `$Id$` (checkout would expand it), a case variant of `.git` such as `d/.GIT` (which `add -A`
   drops), and a fifo, socket or device. A repo with `core.autocrlf=true` and `text=auto` still goes cold, because checkout writes the
   CRLF bytes back.
6. Write the registry record as `cold`.

### `bale open`

1. Check out the branch at the same path. If the directory was moved, use `git worktree repair`.
2. With HEAD where `cold` left it (on the recorded branch, or detached), check out `refs/bale/<id>/wip`
   through the same steps as cold's check, so branch-dependent config applies the same way. Restore the
   saved files, checking hashes, then the recorded modes and empty directories. Check the result, still
   with HEAD there, against the wip commit's own path list and the manifest, never the ignore rules in
   force at open time: an exclude line removed while the tree was cold turns a saved ignored file into an
   untracked one, and that must not fail the open. Then compare the raw bytes of every file against the
   hashes cold recorded, and check that every recorded symlink is still a symlink to the same target. If
   git config changed while the tree was cold (a filter, eol rules, `core.symlinks=false`), the checkout
   can write different bytes, or a regular file in place of a symlink, that git's own diff still calls
   equal; `open` then stops naming the file, with the record left `opening` and the saved state kept. A
   save made before hashes or symlink targets were recorded has none, and that check is skipped for it. Submodules are not checked out; `open` notes them for `git
   submodule update --init`. Only after the check does HEAD detach for the index reset.
3. Rewrite machine-specific absolute paths that are known to break, such as `sdk.dir` in
   `local.properties`. This only matters if the path or machine changed.
4. Release bale's git lock (a lock with any other reason stays) and mark it hot.

### Idle

A workspace is idle when nothing has its cwd or a file open inside it, and no source file has changed for the
threshold. Defaults are 4 hours to `baled` and 7 days to `cold`, both configurable. Gradle expires local build
cache entries that go unused (7 days by default), so a workspace baled longer than that resumes colder.

A sweep also bales a `baled` workspace again when it has been idle for the `baled` threshold and its
regenerable directories have come back, for example after a build with no `bale open`. This applies to
dirty trees too, since `up` touches no source.

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
- Before anything else, `cold` checks that the folder at the recorded path is the worktree's own
  checkout: git run there must use the worktree's admin dir, the same check `open` makes before it
  restores. A folder recreated at the path with no `.git` would let git climb into an enclosing repo and
  save that repo's state over the record. Such a folder, or one where git fails, is skipped, naming the
  path, with no ref or `$BALE_HOME` write and the record unchanged. `bale gc` makes the same check before
  it releases bale's lock on a `hot` workspace or deletes its leftover saved state, and leaves both alone
  when it fails. `up` makes it too before removing output from a `hot` or `baled` tree. Registering
  (`bale up|cold|open <path>`, the remove hook, `migrate --apply`) refuses a path with no `.git` entry,
  and one whose admin dir is not directly under `<common>/worktrees/`, so a stray folder can never
  repoint the record's admin dir at an enclosing or main repo.
- `cold` skips a tree, before saving anything, when any of these hold: a submodule (nested ones too) has
  uncommitted changes or untracked files, ignored files that are not regenerable, or its own `.git`
  directory; a submodule is checked out at a commit other than the one HEAD records (a commit made inside
  it shows as ` M lib`); a submodule directory that is not checked out holds files; the tree uses sparse
  checkout; or
  an index entry at the root or in a submodule is flagged assume-unchanged or skip-worktree, since `git
  status` skips edits to those. LFS pointer files need no special handling, because the checkout restores
  them. Anything step 5's disk compare would refuse is caught there, after the move, and the tree moves
  back.
- Every git command bale runs passes `-c status.showUntrackedFiles=all -c core.fsmonitor=false -c
  core.untrackedCache=false -c diff.ignoreSubmodules=none`, so the user's or repo's config cannot make a
  tree look unchanged when files exist. `-c` does not override `submodule.<name>.ignore` in `.gitmodules`,
  so each `git status` also passes `--ignore-submodules`.

### Recovering an interrupted `cold`

A record left `cooling` by a crash resumes on the next `cold` or sweep. Before anything else, stale
`.bale-check-<id>-*` dirs beside the path are removed.

- If something is at the path and a trash directory sits beside it, the checkout was already moved
  aside, so the folder at the path was made since (an editor recreating `.idea/workspace.xml`, say).
  Saving it would replace the save of the trash. `cold` skips, naming the path and the trash directory,
  and asks the user to move the folder away and run `cold` again; the record stays `cooling` and nothing
  is written.
- If the folder at the path is not the worktree's own checkout (the checkout check above),
  `cold` skips, naming the path, and the record stays `cooling`.
- Otherwise the checkout is still at its path, and `cold` saves and verifies it again from step 2.
- If the checkout is gone and `refs/bale/<id>/wip` does not resolve, or the saved manifest is missing,
  nothing is deleted: `cold` reports an error and leaves any trash directory beside the path.
- Otherwise each trash directory beside the path is handled in turn:
  - If step 5 recorded it (both the trash name and the wip commit match), it was already checked and is
    deleted.
  - If it was not recorded and has no `.git`, it is kept and reported as an error, since git can't check it
    against the save.
  - If it was not recorded, it is checked against the save with step 5's disk compare, then for processes
    holding it and for changes since the compare, as step 5 does. If all pass, it is deleted. If it
    changed, a process holds it, or the check fails, it is moved back to the recorded path and the record returns to `baled`;
    a change is reported busy, and a failed check as an error. If the move back fails, the trash stays where it
    is, the record stays `cooling`, and the error names both paths.
- When every trash directory is gone, the record is written `cold`.

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
and diverges by about 2 GB on Android. On iOS it diverges by about 3.6 GB, and the clone must have
`ModuleCache.noindex` deleted or the build fails. What is not known: how much the base drifts as the default
branch moves. Checking out a feature branch in the clone rewrites only the files that differ from
the default branch, so the extra divergence depends on the size of the feature. That was not measured.

Tier 2 is enabled per build system, and only where a measured spike shows at least a 40% saving per hot
workspace compared to a separate worktree. Android met that bar at about 55%. iOS met it at about 70%, but
only for disk: the first build at a new path is a full recompile. *(My call on the threshold.)*

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

- **T3's worktree creation** does not go through Claude Code hooks. Whether T3 exposes its own hook is
  unknown. Until it does, T3 worktrees are picked up by the sweep.
- **Same-path restore was inferred, not run.** The spike showed no-change builds at the original path are
  up to date. A full `bale up`, then build cycle on a real worktree still needs to be timed.

## Risks

- **Hot workspaces still cost disk.** With tier 1 alone, every hot Android workspace costs a full build,
  1.5 to 7.9 GB on this machine. Twelve hot workspaces could exceed the 78 GB currently free even with every
  idle one baled. Tier 2 brings an Android workspace to about 2 GB and an iOS workspace to about 3.6 GB.
  Twelve hot workspaces are then 24 to 43 GB, which fits the 20 GB floor on this machine but with little
  margin on an iOS-heavy day. Each new iOS workspace also pays a full compile, about 2.5 minutes here.
  Linux on ext4 and Windows on NTFS have no tier 2 at all.
- **Delete races.** A Gradle daemon or IDE can recreate `build/` while it is being deleted. The idle check
  reduces this. It does not remove it.
- **Build-cache expiry.** Resumes after more than about 7 days are colder than "warm".
- **Base generations.** Under tier 2, refreshing the base keeps the previous generation's blocks allocated
  until every clone of it is gone.
