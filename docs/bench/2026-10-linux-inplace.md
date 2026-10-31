# Linux: in-place compression and FIDEDUPERANGE

Run on 2026-10-30 to see whether Linux has a cheaper path than stash (copy to a zstd store, truncate, copy back)
that still keeps each build file at its original inode and nanosecond mtime. Two candidates: `btrfs filesystem
defragment -czstd` (compress in place) and the `FIDEDUPERANGE` ioctl (share identical extents between files).
Nothing here ran bale; the operations were run by hand with `btrfs`, `duperemove` and `xfs_io`. One run per case.

**This ran in a Linux VM on macOS, not on bare Linux.** Timings include a loop device on a file in Docker's disk
image, so treat them as relative. Space numbers do not depend on that.

## Setup

| | |
|---|---|
| Host | macOS 27.2, M3 Max |
| Docker Desktop | 4.64.0 (engine 29.2.1), VM with 14 CPUs and 7.65 GiB RAM |
| VM kernel | 6.12.72-linuxkit; `btrfs` and `xfs` both in `/proc/filesystems` |
| Container | `debian:bookworm`, privileged; btrfs-progs 6.2, duperemove 0.11.2, btrfs-compsize 1.5, xfsprogs (`xfs_io`) |
| btrfs | 16 GiB sparse file on a loop device, `mkfs.btrfs` defaults, mounted `noatime` unless a section says otherwise (kernel shows `ssd,discard=async,space_cache=v2`) |
| XFS | 16 GiB sparse file on a loop device, `mkfs.xfs -m reflink=1` |

Inputs were `build/` directories of live worktrees under `code-android-app/.claude/worktrees/`, copied read-only
with `tar` through a bind mount and then onto the loop filesystem with `cp -a`.

| Tree | Source worktree | Files | Bytes |
|---|---|---|---|
| C | `group-ready-bubble` | 66,501 | 2,955,357,667 |
| A | `bugsnag-validation-reporting` | 60,648 | 485,793,059 |
| B | `bugsnag-validation-inputs` | 62,016 | 477,798,288 |

`bugsnag-validation-triage` has no `build/`, so B is its sibling `-inputs`. All `build/` directories were taken
whole, not only files of 1 MiB or more. C is the only tree with `.so`, `.apk`, `.dex` and large `.jar` files in
volume (by size: `.so` 954 MiB, `.bin` 416, `.apk` 359, `.jar` 279, `.dex` 274, `.class` 140). A and B are
mostly small `.class` and intermediate files. Whether A and B come from the same commit is unknown; 46,267 paths
are in both, and 44,855 of those (316 MB) are byte-identical. The iOS `DerivedData` input was not run.

Each file's inode, size, `mtime` and `ctime` (nanoseconds, GNU `find -printf '%i %s %T@ %C@'`) and its sha256
were recorded before and after each operation and diffed. "0 differing lines" below is that diff across every
file in the tree. "Physical" means `compsize -b` "Disk Usage" (extents counted once however many files reference
them) or the `used` column of `df -B1`; `df` also counts btrfs metadata.

## 1. btrfs in-place compression

`btrfs filesystem defragment -r -czstd -f` on tree C, copied onto an uncompressed mount.

| | Before | After |
|---|---|---|
| Physical, `compsize` | 2,955,357,667 | 1,081,008,811 (36.6%) |
| `df` used | 3,112,685,568 | 1,252,540,416 |
| Time | | 7.0 s (warm cache) |
| Files with a different inode, size, `mtime` or `ctime` | | 0 of 66,501 |
| Files with a different sha256 (caches dropped before the read-back) | | 0 of 66,501 |

The command does not update `ctime`; the diff above includes it. By extension, same run:

| Ext | Before | After | Ratio |
|---|---|---|---|
| `.so` | 1,000,673,280 | 386,128,824 | 0.385 |
| `.bin` | 437,430,481 | 156,662,900 | 0.358 |
| `.apk` | 377,118,720 | 141,750,272 | 0.376 |
| `.jar` | 294,665,794 | 82,421,595 | 0.279 |
| `.dex` | 289,255,992 | 109,463,813 | 0.376 |
| `.class` | 198,038,794 | 85,127,963 | 0.452 |

The `.jar` files in this tree are mostly stored rather than deflated, which is why they shrink. A jar that is
deflated compresses much less (below).

**Writing afterwards.** On a `.so` (30.4 MB, 39% after compression) and a `.jar` (6.5 MB, 18%) from the run
above: overwrite 4 KiB at offset 8192 and append two bytes. Both kept their inode, `mtime` and `ctime` moved to
the write time (expected), the 8 KiB before the write and the bytes after it matched an uncompressed copy given
the same edit, and the file stayed `zstd` except the rewritten range, which `compsize` showed as 8 KiB of `none`
on this plain mount. The 4 KiB of random data written was not compared byte for byte.

### Mount option and level

Each row is tree C copied onto a fresh filesystem with that mount option, so the data was compressed at write
time; the last three are the defrag path on the plain copy for comparison.

| Case | Physical | Of original | `.jar` | `.dex` | `.so` | `.apk` |
|---|---|---|---|---|---|---|
| `compress=zstd` (heuristic) | 1,374,286,507 | 46.7% | 0.279 | 0.618 | 0.538 | 0.471 |
| `compress-force=zstd` | 1,064,387,243 | 36.2% | 0.276 | 0.342 | 0.381 | 0.376 |
| `compress-force=zstd:15` | 941,027,116 | 32.0% | 0.244 | 0.291 | 0.326 | 0.324 |
| plain copy, then `defragment -czstd` | 1,081,008,811 | 36.7% | 0.279 | 0.376 | 0.385 | 0.376 |
| plain, remount `compress=zstd:15`, then defragment | 944,680,748 | 32.1% | | | | |
| plain, remount `compress=zstd:1`, then defragment | 1,115,519,761 | 37.9% | | | | |

- `defragment -czstd` behaves like `compress-force`: it does not skip data the write-time heuristic would give
  up on. The heuristic left `.dex` at 0.618 and `.so` at 0.538.
- `btrfs-progs` 6.2 has no level flag for defragment. The level comes from the mount's `compress=zstd:N` at the
  time of the call (15 gave 32.1%, 1 gave 37.9%, the default 36.7%). Whether kernels other than 6.12 do the
  same is untested.
- Already-compressed input, a `.jar` made with `zip -9` (19.4 MB) and a gzip -9 of a `.so` (24.5 MB), both copied
  to each mount: `compress=zstd` 1.000 for both; `compress-force=zstd` 0.829 and 0.999; defragment on the plain
  copy gave the same 0.829 and 0.999. So force helps a deflated jar by 17% and does nothing for gzip output.
- The `compress=zstd:15` and `compress-force=zstd:15` writes were not timed.

## 2. FIDEDUPERANGE

`duperemove -dr` (block size 128 KiB by default, `-b 4K` where noted) and `xfs_io -c "dedupe src soff doff len"
dst`. Every row: 0 files with a different inode, size, `mtime` or `ctime`, and 0 files with a different sha256,
in every tree touched.

| Case | Filesystem | Physical before | Physical after | Time |
|---|---|---|---|---|
| A and a `cp -a` copy of A, `-b 128K` | btrfs | 1,064,094,490 | 870,890,266 (-18%) | 1.5 s |
| A and the copy, `-b 4K` | btrfs | 1,064,094,490 | 469,060,378 (-56%) | 7.2 s |
| A and B (two builds), `-b 128K` | btrfs | 1,065,185,145 | 938,155,897 (-12%) | 1.4 s |
| A and B, `-b 128K` plus `--dedupe-options=partial` run after | btrfs | 938,155,897 | 938,155,897 | 2.7 s |
| A and B, `-b 4K` | btrfs | 1,065,185,145 | 572,555,129 (-46%) | not recorded |
| A, A copy, B; A+copy then B vs A, `-b 4K` | btrfs | 1,597,232,390 | 638,321,926 (-60%) | 7.2 s + 5.8 s |
| same three trees, `-b 4K` | XFS | `df` used 2,186,821,632 | 1,592,168,448, then 1,228,513,280 | 5.7 s + 5.5 s |
| A and the copy, `-b 128K` | XFS | `df` used 2,186,821,632 (with B) | 1,993,617,408 | 1.2 s |

- The 4 KiB block size frees three to four times what the default does on these trees. The second pair's savings are
  larger than the data A and B share between them (316 MB of identical files), so most of the gain at 4 KiB is
  repeated blocks inside each tree, not across the two. Neither run measured memory use.
- The `partial` option added nothing on A and B at 128 KiB.
- btrfs and XFS freed about the same on the same input (`df` used fell 1,999,552,512 to 1,046,507,520 on btrfs
  and 2,186,821,632 to 1,228,513,280 on XFS, the btrfs figure includes metadata).

### What the ioctl does

`xfs_io` on a 30.4 MB `.so` and a copy of it on btrfs:

| Call | Result |
|---|---|
| whole file, identical copy | 30,390,024 of 30,390,024 bytes deduped; physical 60,784,640 to 30,392,320; destination inode, `mtime`, `ctime` unchanged |
| whole file, copy differing by 4 bytes at offset 1,000,000 | `Extents did not match.` Nothing changed |
| first 512 KiB, then 1 MiB to the end, of the same differing copy | both succeed |

The kernel compares the bytes itself under the file locks, so a stale hash can't merge different data. The same
two partial calls on XFS freed 29,339,648 bytes (`df` used). On btrfs, partial dedupe of the one 30 MB extent
freed nothing (`df` delta 0, `compsize` 60,784,640 before and after), because the original extent stays
allocated while any of its bytes are still referenced.

### Copy on write

After `-b 128K` dedupe of A and a copy, 4 KiB written into a 1 MB `R.jar` in the first tree:

| | btrfs | XFS |
|---|---|---|
| `df` used delta | +4,096 | +131,072 |
| Written file | same inode, `mtime` and `ctime` set to now | same |
| Other copy | sha256 unchanged; inode, `mtime`, `ctime` unchanged | same |

## 3. Defragment after dedupe

btrfs, A and a copy, then B. `compsize` over all trees touched.

| Step | Physical | Notes |
|---|---|---|
| A, copy, B on disk | 1,597,232,390 | uncompressed, no sharing |
| after `duperemove -b 4K` | 638,321,926 | `df` used 1,046,507,520 |
| `defragment -czstd` on the first tree only | 750,368,611 | `df` used 1,174,814,720, 128 MB more than before the defrag |
| `defragment -czstd` on all three | 558,489,525 | referenced == uncompressed: no sharing left |
| `duperemove -b 4K` again | 214,609,845 | `df` used 654,954,496 |

At the default block size on A and a copy: dedupe 870,890,266; defragment the first copy only 683,187,575 (178 MB
compressed plus 505 MB uncompressed still held by the second); defragment both 356,446,164 (exactly two
compressed copies, nothing shared); dedupe again 294,567,892. Compressing both first on a fresh filesystem and
then deduping reached the same 294,567,892, and left inodes, sizes, `mtime` and `ctime` unchanged.

Defragment un-shares extents: after it, each tree has its own copy. The order doesn't matter for the end state
as long as the dedupe runs last, and `duperemove` does share compressed extents.

## What it means for bale on Linux

**In-place compression instead of stash.** On btrfs it meets the inode rule: 66,501 files kept inode, `mtime`,
`ctime` and content, the files stayed readable and writable, and there is no restore step and no stub. Tree C
went to 36.6% of its size, 32% at zstd:15. What it does not cover:

- It only works where the tree lives on btrfs. Most Linux home directories are ext4, which has no such
  mechanism. Stash stays the portable path.
- The level is the mount's, so bale would pick it by remounting or take whatever the user mounted with.
  `compress-force` is not needed; the defragment call forces compression itself.
- A later write into a compressed file is not recompressed unless the mount or an inode property says so
  (untested whether `chattr +c` or `btrfs property set compress` changes that).
- Not tested: defragmenting a file that a build is writing or has mapped, the CPU cost of reading compressed
  files during a build, crash behavior mid-defragment, and Gradle, Cargo or any other build tool running over
  compressed trees. bale's busy check and tree lock would still apply before any such call.
- It also has no "stashed" state, so nothing marks which trees were compressed, and the space comes back only
  by writing, not by an explicit restore.

**FIDEDUPERANGE for hot trees.** It also meets the rule on btrfs and XFS: inode, size, `mtime`, `ctime` and
bytes unchanged in every file of every run, and a later write into one copy leaves the other alone. Because the
kernel verifies the bytes, running it on a tree an agent is still using can only fail on a range, not corrupt
one (running it against a live build was not tested). Savings depend on block size and input: 12% to 18% at
128 KiB, 46% to 60% at 4 KiB, with `duperemove` taking 5 to 7 s over about 1.2 GB of cached files. Partial
dedupe frees nothing on btrfs unless a whole extent becomes unreferenced.

**Both together.** Defragment first or last, but dedupe last: defragment breaks sharing, and the three-tree case
ended at 13% of the uncompressed size (214,609,845 of 1,597,232,390) only after the second dedupe. `df` used was
higher after a defragment of one tree than before it.

Not measured: ext4 or other filesystems, XFS or btrfs on bare metal, trees larger than 3 GB, iOS build output,
and the effect of either method on Gradle, Cargo or Xcode incremental builds.
