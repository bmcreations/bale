# bale as a worktree standard: the Code VM model

Status: draft for review. Supersedes the "tier 2" sketch in `2026-10-27-bale-design.md`. Tier 1 (`up`, `cold`, `open`, the sweep) stays as the fallback for trees bale did not create.

## Intent

Today bale is a janitor: tools create full-size git worktrees and bale shrinks them hours later. The goal is a worktree standard in which a tree is small from the moment it exists, and a tree whose session is waiting on the user costs almost nothing, without the agent or the user doing anything.

The model is Flipcash's Code VM. Its README makes four claims:
- Apps "behave as if [they're] using standard Solana accounts".
- It "operates on compact representations of accounts".
- It can "compress dormant accounts off-chain, effectively reducing rent to zero".
- "Compressed accounts can be decompressed when needed", automatically through the app.

Mapped onto worktrees:

| Code VM | bale |
|---|---|
| Standard accounts to the app | A normal directory at the path the tool expects. Git, IDEs and watchers see nothing unusual. |
| Compact hot accounts | A hot tree is a shared, warm **base** plus the session's **delta**, using copy-on-write where the OS has it. |
| Dormant accounts compressed off-chain | A tree whose session is waiting is compressed. Its delta goes to an archive, leaving a stub and a commitment. |
| Decompress when needed, automatically | The next prompt rehydrates the tree before the agent sees it. |
| A transaction | One agent turn: from the user's prompt to the agent's stop. |

## Evidence so far (2026-10-28, this machine)

**Turn timing.** These figures come from 14 days of Claude Code transcripts: 37,452 turns across 380 trees. Only timestamps were read.
- Wait from the agent stopping to the next prompt: p50 152 s, p75 590 s, p90 2,682 s.
- Turn length: p50 131 s, p90 1,025 s.
- Waits longer than 1, 5, 10 and 30 minutes are 70%, 35%, 25% and 13% of all waits.
- **Waits longer than 1 minute cover 76% of tree time**, and waits over 10 minutes alone cover 73%.

So compressing after a 10-minute wait would capture most of the idle time, and only about 1 prompt in 4 would pay the cost of rehydrating.

**Compression cost.** This is a 6.22 GiB Xcode DerivedData, measured with `tar | zstd -T0 -3` at load average 11–18.
- It compresses to 2.07 GiB, a ratio of 3.0x.
- Compressing took 77 s and restoring took 50 s, about 130 MB/s.
- System time dominates (46 s and 44 s): restore is bound by per-file filesystem calls, not by zstd. Rehydrating whole build trees per prompt is too slow, so the delta must stay small or be stored in a form that restores without creating every file again.

**Storage benchmark.** These runs used two real Xcode DerivedData folders from two branches of the Flipcash iOS app: A (6.06 GiB) and B (6.27 GiB, 110,822 files). Load average was 6–100 during the runs; without sudo the file cache could not be purged, so every timing is warm-cache.

| Step | Result |
|---|---|
| One base image attached twice with two shadow files | works, each mount keeps its own writes, no sudo needed |
| `hdiutil attach -shadow` | 0.41–0.61 s over 10 runs (1.74 s on the first, cold run) |
| `hdiutil detach` | 0.15–0.30 s typically; 0.9, 2.1 and 10.3 s when load spiked |
| Content delta B over A (`rsync --checksum`) | 24,062 files and 4.17 GB changed; shadow file 4.43 GiB |
| zstd -3 of that shadow | 1.09 GiB (4.1x), compress 2.7 s, restore 7.2 s |
| (a) tar\|zstd of B: restore into an empty dir | 53–76 s for 2.08 GiB compressed |
| (c) `ditto --hfsCompression` | left files uncompressed (no `compressed` flag, same size on disk), so (c) is still untested |

What this means for the design:
- **Restoring a single file beats restoring many files.** Decompressing the whole shadow (4.43 GiB) took 7.2 s, against 53–76 s to untar a similar amount of build output as separate files. The tar route (a) can't meet the budget for build output at any realistic size.
- **An uncompressed shadow costs only the attach.** Re-attaching a shadow left on disk takes about 0.4–0.6 s, inside the 2 s p50 budget. The disk saving then comes from sharing the base, not from compressing it.
- **Compressing the shadow is for long waits.** At the measured speed, about 0.65 GB/s, a compressed shadow restores within the 3–4 s p95 target only up to about 2–2.5 GB. A larger shadow should stay uncompressed until the tree has been idle much longer than T.
- **Branches drift far from a shared base.** Two branches' builds differed in 4.17 of 6.5 GB, so a long-lived branch tree ends up with most of a build in its delta. The base has to be refreshed, or a branch's own last build used as its base.

**Compression options benchmark.** Same two DerivedData folders (A 6.63 GB and B 6.73 GB on disk, B has 92,561 files), warm cache, load 4-47.

| Option | Size | Time | Reading every file back |
|---|---|---|---|
| Plain folder | 6.63 GB | - | 66 s |
| Compressed image ULFO (lzfse) | 2.40 GB (2.8x) | create 100 s, attach 0.36-0.61 s | 81 s |
| Compressed image UDZO (zlib) | 2.62 GB (2.5x) | create 104 s, attach 0.38-0.64 s | 76 s |
| APFS compression, `afsctool` LZFSE | 2.76 GB (2.4x) | 42 s for the whole tree | 84 s |
| APFS compression, `afsctool` ZLIB | 3.90 GB (1.7x) | 37 s | 59 s |

Writing B into an attached ULFO image of A took 110 s and grew the shadow to 7.65 GB, more than B itself. Folding image plus shadow into a new ULFO image took 9.4 s and gave 2.54 GB.

Storing B as a delta over A:
- 2.33 GB of B's 6.50 GB is byte-identical to A at the same path, and 1.09 GB is duplicated within B itself.
- 18,193 files changed (3.27 GB). zstd -3 per file gives 774 MB; `zstd --patch-from` against A's version gives 293 MB.
- 4,104 new files (893 MB) give 424 MB with zstd -3.
- So B costs about 717 MB over A, against 2.54 GB for the folded ULFO image and 4.43 GB for an uncompressed shadow.
- A trained dictionary shrinks files of 128 KB or less by another 17% (sample of 10,000: 30.3 MB to 25.1 MB). Those files are 88,777 of the 92,561 but only 1.05 GB.
- Xcode's index (`Index.noindex`) is 203-209 MB per folder, about 3%.

What this means for the design:
- **APFS compression costs nothing to restore.** Reads decompress on the fly at about the speed of the plain folder, so a tree compressed this way is never "restored". Files a build rewrites are stored uncompressed until bale compresses them again, so this suits trees between turns. Linux (btrfs `compress=zstd`) and Windows (`compact /EXE:LZX`) have equivalents. `afsctool` is GPL-3, so bale has to write its own compressor for this.
- **A compressed image suits cold trees.** It attaches in under a second with nothing unpacked, but a build writing into it grows the shadow past the size of the build output, so it shouldn't be used for a tree that's building.
- **Delta over a base is 3.5x smaller than any packaged option** (717 MB against 2.54 GB for B). Restoring it means writing about 22,000 files, which is too slow without a filesystem that writes them only when they're read. That is the case for bale's own format plus a user-space filesystem.

**Per-turn delta (calibration builds).** Shared clones of Flipcash iOS (`main` 7a27829f5, scheme Flipcash, simulator build, in-tree `-derivedDataPath`) and Flipcash Android (`code/cash` 429f5914a, `:apps:flipcash:app:assembleDebug`). Each edit was built incrementally against an APFS snapshot of the previous build output; the delta is measured against that snapshot.

| Repo, edit | Build | Files changed / added | Changed bytes | zstd -3 alone | `--patch-from` |
|---|---|---|---|---|---|
| iOS clean build | 388 s | 114,901 files, 11.58 GB | | | |
| iOS leaf (string in a view) | 45 s | 77 / 803 | 1.05 GB | 219 MB | 19 MB (+5 MB added) |
| iOS core body (`FlipcashCore`, ~875 importers) | 35 s | 37 / 7 | 4.17 GB | 884 MB | 201 MB |
| iOS core API (new public func) | 95 s | 79 / 19 | 4.24 GB | 910 MB | 454 MB |
| Android clean build | 126 s | 54,048 files, 2.58 GB | | | |
| Android leaf | 9 s | 58 / 10 | 474 MB | 131 MB | 6 MB |
| Android core body (~591 importers) | 9 s | 49 / 10 | 480 MB | 131 MB | 3 MB |
| Android core API | 28 s | 237 / 140 | 808 MB | 236 MB | 8 MB (+8 MB added) |

- A turn touches tens to hundreds of files, not tens of thousands, so restoring one turn's delta doesn't need a lazy filesystem. Most of the bytes are a few large files rewritten in place: the iOS debug dylibs (350–470 MB each, some stored twice) and the Android APK (381 MB, 2 MB as a patch).
- Writing those files back is the restore cost. An iOS core edit means writing about 4 GB, which may not fit the p95 budget; that restore time is not measured yet.
- **A tree cloned to a new path does not start warm.** The iOS build failed outright (`SwiftShims` precompiled with the old module cache path). With `ModuleCache.noindex` deleted it built in 200 s and rewrote 28,887 files. The Android build succeeded but rewrote 985 files and deleted 2,873, because outputs embed absolute paths. Build output is only valid at the path it was built at.

**Serving the store without root (macOS 27.2).** A Rust NFSv3 server (`nfsserve` 0.11, its `mirrorfs` passthrough example) mounted with `mount_nfs` on localhost as the user, with no sudo, kext or System Settings approval. Over a clone of one DerivedData folder (91,399 files), single runs:

| Test | Plain APFS | Through NFS | Ratio |
|---|---|---|---|
| mount / unmount | | 0.05 s / 0.03 s | |
| read every file (`tar`) | 61.6 s | 144.9 s | 2.35x |
| `find -type f` | 1.15 s | 11.6 s | 10x |
| open and read 1,000 small files | 0.15 s | 0.79 s | 5.3x |

FSKit can serve a path-backed filesystem since macOS 26 (`FSPathURLResource`), but only from a signed app extension that the user enables in System Settings, and it stays in the I/O path just like NFS. On Linux the equivalent is FUSE (`fuser`); on Windows, ProjFS, which hydrates a placeholder into a real file on first read and then steps aside.

## Design

### Tree layout

- **Base.** One per repo. It is a checkout at a commit plus a warm build, read-only, and owned by bale.
- **Tree.** Every bale tree is created from the base with copy-on-write: `clonefile` on APFS, reflink on btrfs and XFS, block clone on ReFS and Dev Drive. A new tree's source costs close to zero bytes. Its build does not carry over: the calibration builds showed build output is only valid at the path it was built at, so a new tree builds once at its own path.
- **No copy-on-write available** (ext4, NTFS): the tree is a plain worktree, and tier 1 governs it.
- **Build output must live in the tree.** Xcode's shared DerivedData is keyed by workspace path, so a tree cloned to a new path gets a new, empty DerivedData folder and builds from scratch. A bale tree builds with `-derivedDataPath` inside the tree, so the clone carries the base's build. Each tree's build output is stored and restored at that tree's own path only.

### Lifecycle per turn (Claude Code)

1. `Stop`: bale records that the turn ended and starts a timer for this tree. Nothing else runs; the hook must return within milliseconds.
2. After a wait of **T** (default 10 min, from the data above), if the tree is not busy, the sweep or a per-tree timer compresses it:
   - It writes the delta to an archive and records a hash commitment of the delta.
   - It drops the delta's bytes from the tree, so the tree falls back to base plus stub.
3. `UserPromptSubmit`: if the tree is compressed, bale rehydrates it and checks the commitment before the prompt continues.
   - A hash mismatch refuses: the archive is kept and the hook fails loudly. It never continues silently.
   - On a hot tree this is a marker-file stat and nothing else.
4. No work on `PreToolUse`. It fires on every tool call.

`Notification`, which covers permission waits in the middle of a turn, is out of scope until data shows those waits are long.

### What the delta holds (a policy, not a constant)

- **Always kept:** edits, untracked files that can't be regenerated, and git state. These are small.
- **Divergent build output:** one of two policies.
  - `keep`: archive it. Resume is warm, but the archive and rehydrate time are bigger.
  - `drop`: delete it. Resume falls back to the base's warm build and an incremental rebuild.

  The benchmark picks the default, and it can be set per repo.

### Rehydrate budget

Rehydrate blocks the prompt: p50 at most 2 s, and p95 at most 3–4 s, with 5 s the most that's acceptable. Measured throughput then sets the largest delta allowed to stay in `keep` mode. A delta over that size switches to `drop`, or to the image-file storage below if that proves faster.

### Storage choice under test

Two candidates:
- **(a)** A zstd tar of the delta's files. This works everywhere, but restore is bound by per-file filesystem calls.
- **(b)** One image file per tree layered on the base: a sparse image with `-shadow` on macOS, overlayfs on Linux, a VHDX differencing disk on Windows. Compressing and restoring then move one file, at raw zstd speed.

A third, macOS-only candidate needs no rehydrate at all:
- **(c)** Compress the delta's files in place with APFS transparent compression (`ditto --hfsCompression` or `afsctool`). Reads decompress on the fly, and a write simply stores that file uncompressed again. It only works if the ratio on build output comes close to zstd.

The benchmark decides between them. (b) also changes the mount model and needs admin rights on Windows.

### Busy rule

Any process other than the agent's own that holds the tree open keeps it hot whatever the turn state is: an IDE, a build daemon or a shell. Only trees the agent alone uses are compressed per turn. This is the IDE answer: an open IDE pins the tree.

### Base lifecycle

- A base is built with `bale base build <repo> [--commit]` and refreshed when main moves by N commits or after D days.
- A tree records the base it was cloned from.
- An old base is kept while any tree references it, then garbage-collected.
- Rebasing a delta onto a new base is out of scope for v1. Trees stay on their original base until they are removed.

### Commitment

The commitment is a SHA-256 over the delta's archive manifest: paths, modes and content hashes. It is stored in the tree's state record. It replaces tier 1's filesystem-walk restore check for trees bale created.

### Races

- Each tree has one lock.
- Hook and sweep contend for it. The hook wins, and the sweep skips the tree and retries on its next pass.
- Two sessions on one tree: the later `UserPromptSubmit` waits on the lock.
- Claude runs matching hooks in parallel, so bale's hooks must not depend on the order of other hooks.

### Tool scope

Turn-level compression needs two signals: one when a turn ends, to start the timer, and one before the next prompt runs, to rehydrate. Without the second, a compressed tree would break the agent's next turn.

- **Claude Code** has both (`Stop`, `UserPromptSubmit`). That includes Claude threads inside T3 Code: T3 runs Claude through the Agent SDK with the user's `~/.claude/settings.json` loaded, so those hooks fire there.
- **T3 Code's own scripts** give only the first signal. `runOnSettle` fires when a thread settles, and the only other trigger is `runOnWorktreeCreate`. T3 threads on other agents (Codex) stay on tier 1 timers.
- **Cursor, Codex and Conductor** stay idle-based (tier 1 timers) until their hooks show both signals.
- The standard is the tree format and the CLI (`bale new`, `bale open`, `bale wait`, `bale resume`), not any one tool.

## Benchmark

This extends the trace-driven benchmark already planned.

**Strategies compared:**
- plain worktrees
- `git clean`
- shared DerivedData and SPM cache
- tier 1
- base plus delta, idle-only
- **Code VM(T, keep|drop, storage a|b|c)**

**Inputs:**
- Turn timings from transcripts (done).
- Tree sizes and regenerable bytes from the `~/.bale/trace` recorder (running).
- Off-hours calibration (approved): from a warm base, apply a representative edit, run an incremental build, and measure the delta with `df` (not `du`, which counts clones at full size). Two builds, because they answer different questions:
  - **Same path:** what a turn costs after rehydrating a tree in place. This is what the per-turn budget depends on.
  - **New path:** what a new tree cloned from the base costs. If this is close to a full build, new trees don't start warm, but the per-turn model still holds.
- Rehydrate time for each storage candidate.

**Scores:**
- Idle disk: GiB·hours, and the peak.
- Resume cost: rehydrate seconds plus rebuild seconds per prompt, p50 and p95.

The acceptance bar stays the same: a full day of 4 or more sessions keeps free disk at or above 20 GB.

## Non-goals (v1)

- A virtual filesystem (EdenFS-style lazy materialisation via FUSE, ProjFS or NFS loopback). This is kept as the follow-up if storage (b) still leaves resume too slow.
- Freezing process state (CRIU). Build daemons restart on resume.
- Rebasing deltas onto newer bases.

## Decisions

- Off-hours calibration builds on the Flipcash iOS repo and one Android repo are approved.
- Rehydrate budget: p50 at most 2 s; p95 3–4 s, with 5 s acceptable.
- Storage is bale's own format in v1, not packaged images or filesystem compression: content-addressed files, deduplicated across trees, with changed files stored as `zstd --patch-from` against the tree's previous version. A tree's B-over-A delta was 717 MB this way, against 2.54 GB for the smallest packaged option.
- No off-machine storage tier.
- Cold trees are served through a user-space filesystem (NFSv3 loopback on macOS, FUSE on Linux, ProjFS on Windows) while a background job writes the files for real; then bale unmounts and swaps in the real directory. A turn-sized delta is written back directly.
