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

**Still unmeasured:**
- How large the delta over a warm base is after a typical turn's edit and incremental build. This needs real builds.
- How fast the delta restores when stored as one image file instead of a tar of files.

## Design

### Tree layout

- **Base.** One per repo. It is a checkout at a commit plus a warm build, read-only, and owned by bale.
- **Tree.** Every bale tree is created from the base with copy-on-write: `clonefile` on APFS, reflink on btrfs and XFS, block clone on ReFS and Dev Drive. A new tree costs close to zero bytes and starts with a warm build.
- **No copy-on-write available** (ext4, NTFS): the tree is a plain worktree, and tier 1 governs it.
- **Build output must live in the tree.** Xcode's shared DerivedData is keyed by workspace path, so a tree cloned to a new path gets a new, empty DerivedData folder and builds from scratch. A bale tree builds with `-derivedDataPath` inside the tree, so the clone carries the base's build. Whether that build is still valid at the new path is measured, not assumed (see Benchmark).

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
