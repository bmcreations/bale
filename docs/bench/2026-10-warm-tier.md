# Warm tier checks

## Task 0: does Xcode accept a rewritten build output? (2026-10-29)

Restoring a stashed file with the same bytes, mode and nanosecond mtime is not enough. If the file gets a new inode, Xcode relinks every binary built from it. Writing the bytes back into the same inode costs nothing.

Setup: Flipcash iOS (`Flipcash` scheme, simulator, `CODE_SIGNING_ALLOWED=NO`), a `git clone --shared` checkout with in-tree `-derivedDataPath .dd`. Xcode 27.1 (27A9275), macOS 27.2, M3 Max, 36 GB. Load average 4–18 during the timed builds. One run per case, warm cache. The full build took 354 s; a build with no edit takes 15–17 s.

Each case rewrote files with no source edit, restored mode and mtime with `os.utime(ns=...)`, then ran one incremental build.

| Case | Files rewritten | Build | Link steps | Files the build rewrote |
|---|---|---|---|---|
| No change | 0 | 15.3–16.7 s | 0 | – |
| Final product `Flipcash.debug.dylib` (451 MB), new inode | 1 | 15.6 s | 0 | 0 |
| Same file, same inode | 1 | 15.6 s | 0 | 0 |
| Control: same file, mtime moved 5 s | 1 | 19.5 s | 0 | 1 (copied again) |
| T1: every file ≥ 1 MiB under `.dd`, new inode | 711 (7.73 GB) | 45.8 s | 16 | 39 |
| T2: same files, same inode | 711 | 17.6 s | 0 | 0 |
| T3: same files, new inode via `cp` (xattrs kept) | 711 | 43.0 s | 16 | 39 |

The 16 link steps are the per-architecture intermediate binaries of Flipcash, NotificationContent, NotificationService and ShareExtension. The final product passes with a new inode because Xcode copies it from those intermediates rather than linking it. T1 to T3 leave out `XCBuildData/` and `SDKStatCaches.noindex/`, which change on every build.

T3 rules out lost extended attributes as the cause. The only attribute on build output is `com.apple.provenance` (132,481 entries; all 712 files ≥ 1 MiB carry it), plus `com.apple.xcode.CreatedByBuildSystem` on 9 directories. macOS adds `com.apple.provenance` to new files by itself, so T1 kept it too.

So a stashed file must keep its inode: stash truncates it to 0 bytes in place, and unstash writes the bytes back into it. Writing 7.7 GB back in place took 7.3 s from page cache, single-threaded Python. Some files under `SourcePackages/checkouts` are read-only, so the owner write bit has to be set before writing and the mode restored after.

Two more results:

- Rust's `std::fs::copy` clones on APFS: three copies of a 1 GiB file changed free space by 0–1 MiB. It can't be used to restore duplicates, though, because a clone is a new inode.
- Gradle wasn't tested; it fingerprints content, not inodes.

## Task 10: stash and restore on the same build (2026-10-29)

The restore missed its budget because the disk itself was slow to take writes at the time. On this machine, a plain 8-thread write of 5.4 GiB took 5.1–7.0 s, and bale's restores took 3.0–10.8 s for the same bytes. The rest held: each stash freed 5.7 GiB of disk, a restore kept every inode, and the build after a restore compiled and linked nothing.

Setup: the Task 0 checkout, made a linked worktree so `bale adopt` takes it, with `.dd/` in its git exclude file so the stash doesn't keep it as an untracked file. Release build of `feat/warm-tier` at f18a635, temp `HOME` and `BALE_HOME`. Load average (1 min) 7–13 throughout, against 4–18 for Task 0. Three local snapshots existed (macOS update snapshots); `df` still showed the full saving, so none of them held the stashed blocks.

| Run | Stash | Stored | Freed (`df`) | Restore |
|---|---|---|---|---|
| 1 (empty store) | 9.5 s | 979 MiB | 5,128 MiB | 10.8 s |
| 2–5 (same content) | 1.4–1.5 s | 0 B | 5,826–5,828 MiB | 4.0, 6.6, 3.5, 5.3 s |
| After a one-line Swift edit | 5.0 s | 10.6 MiB | 5,815 MiB | 5.6 s |
| 3 more, timed with `time -l` | – | – | – | 3.0, 7.1, 4.7 s |

Each stash covers 408 files, 5.4 GiB. The store ended at 990 MiB. Run 1 freed 698 MiB less than later runs. Its blobs account for at most 979 MiB of that, so the gap is only partly explained. Run 1's restore also overlapped the flush of the blobs it had just written. In real use, minutes to hours pass between a stash and its restore, so runs 2–5 are the representative ones, with a worst case of 7.1 s.

Pass criteria from the plan: restore median ≤ 2 s and worst ≤ 5 s. Median 5.3 s and worst 10.8 s, so it fails. The no-op build after a restore must not be slower than before. It passes: no-op builds took 18.0 s before stashing, 27.0 s and 19.3 s after restores, and 37 s and 24 s in a recheck with no restore between. None of them compiled or linked anything.

Why the restore is slow:

- A restore used about 9 s of CPU on every run (6.5 s user, 2.5 s system), while wall time ranged from 3.0 to 7.1 s. Fixed work with a varying wall time means waiting, not computing. Spread over 8 threads, 9 s of CPU is about 1.2 s, which is the most a faster decoder could get the restore down to.
- A sample of one restore put most worker time in `write` (3,040 samples) and `close` (1,421). Decompression was about 450, and SHA-256 was 623.
- Rewriting the 430 MiB `Flipcash.debug.dylib` in place on one thread took 0.13–0.39 s, with no time in `close`. The stall only shows up with 8 writers at once.
- Starting the largest files first did not help (3.4–9.5 s), so idle threads at the end aren't the cause.

The 2 s median needs about 2.7 GB/s of writes sustained, which this machine didn't deliver under that day's load. The earlier restore benchmark wrote 3.94 GB in 0.92–2.53 s, which is 1.5–4.3 GB/s. That day managed 0.8–1.9 GB/s, so the machine's state changed, not bale.

Two ways to write fewer bytes:

- The `.app` and `.appex` copies under `Build/Products` duplicate intermediate binaries: three dylib pairs, 919 MiB. Task 0 showed Xcode accepts a final product with a new inode, so those copies could be restored as APFS clones of their twins instead of rewritten.
- The spec's lazy file system restores only the files a build opens.

## Duplicate products cloned (2026-10-29)

On macOS, unstash now restores an entry under `Build/Products/` as an APFS clone of a byte-identical entry outside `Build/Products/` that it has just restored, instead of decoding and writing the bytes again. The clone is a new inode; everything else keeps its inode.

On the Task 10 checkout the rule cloned 202.6 MiB of the 5.6 GiB restored (the stash is now 413 files; the checkout carried the leaf edit from Task 10). It did not reach the 919 MiB that Task 10 expected. The three `*.debug.dylib` pairs (312, 281 and 323 MiB) are both under `Build/Products/`: `Flipcash.app/PlugIns/X.appex/X.debug.dylib` and `Debug-iphonesimulator/X.appex/X.debug.dylib`. The rule needs a twin outside `Build/Products/`, so neither copy qualifies. The 202.6 MiB is mostly `Assets.car`, `opencv2` and the `.bundle` resources. Counting every duplicate of a group but its first copy, the manifest holds 1,259 MiB of duplicates.

Restore time showed no gain that the load lets us separate. Eight stash/unstash cycles each with the old and the new release binary, alternating the order between the two rounds, with 1-minute load average 154–189 (`uptime` at each run is in the table; Task 10 ran at 7–13):

| Binary | Restore wall time, s | Median |
|---|---|---|
| Before (Task 12 build) | 5.6, 15.3, 10.5, 11.0, 10.4, 3.8, 6.6, 5.3 | 8.5 |
| With clones | 11.1, 4.1, 4.0, 5.9, 19.7, 8.1, 13.2, 5.2 | 7.0 |

| Run | Load averages (1, 5, 15 min) |
|---|---|
| First round, old-1 to new-4 | 153/118/124 to 170/136/130 |
| Second round, new-1 to old-4 | 181/142/133 to 164/149/136 |

Same-binary runs vary from 3.8 to 15.3 s, so a 1.5 s difference in medians is noise; 202.6 MiB is 3.6% of the bytes. Task 10's numbers (median 5.3 s, worst 10.8 s) were taken at load 7–13, so these runs are not comparable with them either. Cloning the three dylib pairs would save about 916 MiB, 16% of the bytes. Whether Xcode accepts that is untested: Task 0 tested `Flipcash.debug.dylib` in `Flipcash.app/`, not the appex dylibs, and `Debug-iphonesimulator/X.appex/X.debug.dylib` may be the linker's own output.
