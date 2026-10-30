# bale: share tracked files across checkouts

First step toward the "Code VM" items in `docs/architecture.md` ("Not built"). This spec picks the one item
the trace says is worth building first, gates it on one measurement, and puts the other items in order with
the evidence for each.

## Problem

Every checkout of a repo holds its own copy of every tracked file. For most repos that costs little. For the
Android app it doesn't: `vendor/opencv` is a tracked OpenCV SDK, 536 files and 910 MiB, and every checkout
writes it out again.

On 2026-10-30, 14 Android checkouts on this Mac hold it, all at the same git tree (`d80d225`), for 12.5 GiB
by `du`. One copy would do. 103 of its files are 1 MiB or larger and hold 890 of the 910 MiB.

bale can't share build output between checkouts: the calibration builds in the Code VM design showed it is
only valid at the path it was built at. Tracked source has no such problem. A file at blob `X` holds the
same bytes in every checkout, so on APFS, btrfs and XFS each copy can be a clone of one stored file and use
no extra blocks.

## Design

### The clone store

`BALE_HOME/clones/<repo-key>/<oid>` holds one file per git blob, written once and never changed. Only blobs
of `clone_min_bytes` or more go in (default 1 MiB, the same cut as stash).

The store must sit on the same volume as the checkout, because a clone can't cross volumes. bale compares
`st_dev` and does nothing for a checkout on another volume.

Files with a `filter` attribute (Git LFS, smudge filters) are skipped, since their working copy is not the
blob's bytes.

### New checkouts

`bale new`, and so the Claude Code `WorktreeCreate` hook, runs `git worktree add --no-checkout`, reads
`HEAD` into the index, clones each large blob from the store, then runs `git checkout-index -a` without
`-f` for everything else and `git update-index --refresh`. A blob missing from the store is checked out
normally and then added to the store as a clone of that new file, so the next checkout shares it.

### Existing checkouts

Most checkouts here weren't made by `bale new`, which has run once since 2026-10-28. Of the 76 records on
2026-10-30, 55 have no origin tool (found by the sweep or `bale adopt`), 13 came from T3 Code and 8 from
Claude Code hooks. The sweep shares their files too, under these rules:

- Only baled trees, never hot ones. A baled tree rebuilds from scratch anyway, so
  a source file's new inode can't cost a rebuild. The warm-tier bench showed Xcode relinks on a new inode
  for build output; whether it reacts the same way to source files is untested, and this rule makes that
  question moot for v1.
- Take the tree's lock without waiting and skip busy trees, as `up` does.
- Replace a file only when its bytes hash to the index's blob. Modified files are left alone.
- Clone to a temp name next to the file, keep mode and nanosecond mtime, then rename over it.
- Run `git update-index --refresh` afterwards, since the index caches inode numbers.

### Reclaiming the store

`bale gc` deletes a stored blob that no live record's index references, once it is over an hour old, like
the stash store.

### Platforms

- macOS: `clonefile`, already used by `spm.rs` and `unstash.rs`.
- Linux: `ioctl(FICLONE)` on btrfs and XFS. On ext4 the call fails and bale checks out normally.
- Windows: not in v1. ReFS and Dev Drive block cloning come later.

## Gate before building

One measurement, on clones of two Android checkouts in a temp `BALE_HOME`:

1. `df` before and after sharing `vendor/opencv` between them. `du` can't answer this, because it counts
   cloned blocks in full. Expected: about 0.9 GiB freed.
2. `git status` is clean afterwards, and a second `git status` is fast.
3. A Gradle build in the shared tree after a full build beforehand: nothing in `vendor/opencv` is rebuilt.
4. Across every registered tree, the bytes of tracked blobs of 1 MiB or more held by two or more trees, to
   see whether any repo besides Android gains.

If step 1 frees well under 0.9 GiB, some checkouts already share blocks and the saving above is overstated;
stop and report.

Expected result on this Mac: about 11.6 GiB back now (13 of 14 copies), and about 0.9 GiB for each new
Android checkout. The iOS checkouts hold 0.14 GiB of source each, so they gain little.

## The other items, in order

| Item | Evidence on 2026-10-30 | Next step |
|---|---|---|
| Trace how long a tool call waits for a restore | `unstash` ran 9 times, 0.09 to 8.1 s; 3 runs exceeded the 5 s ceiling the Code VM design set for a blocking restore. The pre-tool hook doesn't record how long it waited, so the cost the agent feels is unknown. | Add `wait_ms` to the pre-tool trace line, collect a day, then decide whether the prompt hook should block. Small. |
| `bale wait` / `bale resume` for tools without turn hooks | Stash only reaches trees a Claude Code hook touched: 12 trees have been stashed. The 13 T3 Code trees and 55 trees no hook created go from hot to baled at 4 h. | `wait` can stash on demand. `resume` needs a signal before the next turn, which T3 Code doesn't give (`runOnSettle` fires after a turn). Open question: which tools could call `resume`, and when. |
| Keep or drop a turn's build output | Already a timing: stash at 10 min keeps it, `up` at 4 h drops it. | A per-repo override of `idle_stash` and `idle_baled`, if a repo needs one. |
| Serve cold trees from the store instead of deleting them | 292 `cold` operations against 1 `open` (0.3 s) since 2026-10-28. The NFS loopback bench read files 2.35 to 10 times slower than APFS. | Not planned. Opening a cold tree is rare and already fast. |

The Code VM design's per-repo warm base, cloned with its build, stays dropped: build output doesn't survive a
new path, so a base can only share source, which is what this spec does without a base checkout.
