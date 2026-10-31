# Chunk-level dedup on stash input: measurement

Run on 2026-10-30 with a throwaway C program (not in the repo) that only reads the input trees. Machine: macOS 27.2, load average high and varying (`sysctl -n vm.loadavg`: `{ 13.20 35.01 29.69 }` at the first run, `{ 31.18 29.34 28.07 }` before the iOS runs, `{ 21.98 28.82 28.33 }` at the end). Everything ran under `nice -n 10` with 4 worker threads. One run per case. Wall times are indicative only.

## What the store does today

From `docs/architecture.md` ("Stash and the store") and `crates/bale-core/src/store/`:

- Whole-file SHA-256 dedup, then zstd level 3, one blob per distinct file.
- A file whose path was stashed before is stored as a zstd `--patch-from` delta (long-distance matching on, window sized to the larger of file and base), up to `patch_max_depth` = 4 hops. Files or bases over 1 GiB are stored whole.
- Input is regular files with one link and at least `stash_min_bytes` (1 MiB), under DerivedData `Build/` and Gradle `build/`.
- The patch base comes only from the same tree's earlier stash. `ops/stash.rs` builds `bases` from the manifest of the tree's previous stash, and only when that stash has been restored (`prev.filter(|s| s.restored)`), keyed by the file's path in that tree. A different tree never supplies a base, so the second worktree of a project pays for a full blob unless an identical file exists (whole-file dedup).

## How it was measured

The program walks the roots in order (`scandir`, no symlinks, `lstat` for regular, `st_nlink == 1`, size >= 1 MiB; for Android only files under a directory named `build`, skipping `.git`, `node_modules`, `.gradle`). Phase 1 reads each file once with 4 threads, computing the whole-file SHA-256 and cutting FastCDC-style chunks (Gear hash, 64-bit, normalized chunking with a stricter mask below the average and a looser one above it; min = avg/4, max = avg*4; tested at avg 64 KiB and 16 KiB), keeping only a 16-byte truncated SHA-256 and a length per chunk. Phase 2 (single pass over files that are first occurrences by whole hash):

- zstd-3 over the whole file (streamed, size only).
- For chunks seen for the first time in group order: zstd-3 of each chunk alone, and zstd-3 of the same new chunks concatenated in order and cut into groups of about 1 MiB.
- Stand-in for cross-tree patch: a file with the same relative path in an earlier root and a different hash is compressed with zstd-3 using that file as a prefix (LDM on, window log as in `codec.rs`), keeping the smaller of patch and whole. Both files are read into memory, so pairs where either file is over 192 MiB were left as whole (3 files in the iOS group, 0 in the others). The "earlier root" is the nearest earlier root in the order below; bale would have to pick one of them too.

Chunk dedup is counted over the files that survive whole-file dedup, so "chunk unique" is never larger than "whole unique". The inputs were live trees that other agents were building in; the program opened them read-only and nothing was written to them. No file changed size or mtime between the start and end of its read (`files_changed_during_read=0` in every run), but Phase 2 re-reads files later, so a tree rebuilt between phases would skew chunk sizes a little. This is a rough snapshot.

Inputs, in the order they were given (earlier roots are patch-base candidates for later ones):

| Group | Roots | HEAD |
|---|---|---|
| iOS (3 trees) | `DerivedData/Code-fjtl...` `Build/` (main checkout `code-ios-app`) | `89a96b05b3` |
| | `DerivedData/Code-dflf...` `Build/` (`.claude/worktrees/group-ready-bubble`) | `0a4f4c38f5` |
| | `DerivedData/Code-bzjx...` `Build/` (`.claude/worktrees/composer-test-shadowing`) | `90d02259d4` |
| Android (3 trees) | `code-android-app` main checkout | `c934c91381` |
| | `.claude/worktrees/group-ready-bubble` | `72c8aead9b` |
| | `.claude/worktrees/desktop-three-pane` | `e5f835f52c` |
| Android (2 worktrees, no main) | the last two of the above | |

Substitutions: `composer-selection-ios18` no longer exists, so `composer-test-shadowing` (3.8 GiB `Build/`) stands in. The three `bugsnag-validation-*` trees have no build file of 1 MiB or more (inputs and reporting have about 600 MiB of small files, triage none), so they could not be used; the Android group uses the main checkout and the two worktrees that do have large build output. Only 11.3 of the roughly 18 GiB under the iOS `Build/` dirs is in files of 1 MiB or more. The input counts below are those files.

## Results, iOS (3 trees, 1246 files)

Sizes in GiB.

| Measure | avg 64 KiB | avg 16 KiB |
|---|---|---|
| Input | 11.337 | 11.337 |
| Unique after whole-file SHA-256 (991 files) | 7.927 | 7.927 |
| Unique after chunking | 5.488 | 5.047 |
| zstd-3, unique files | 1.871 | not run |
| zstd-3, unique files, same-path cross-tree patch where a base exists | 1.499 | not run |
| zstd-3, unique chunks, each chunk alone | 1.575 | 1.616 |
| zstd-3, unique chunks, in ~1 MiB groups | 1.431 | 1.346 |
| Unique chunks / index at ~40 bytes each | 74,458 / 2.8 MiB | 275,532 / 10.5 MiB |

Against the 1.871 GiB the store would hold today with no base: patch -20.0%, chunks alone -15.8%, chunks in groups -23.5% (64 KiB) or -28.1% (16 KiB). 460 of the 991 unique files had a same-path file in an earlier tree with a different hash, and 457 of those came out smaller as a patch (3 skipped for size).

## Results, Android (main checkout + 2 worktrees, 2666 files)

| Measure | avg 64 KiB | avg 16 KiB |
|---|---|---|
| Input | 16.778 | 16.778 |
| Unique after whole-file SHA-256 (1705 files) | 8.968 | 8.968 |
| Unique after chunking | 3.301 | 2.700 |
| zstd-3, unique files | 3.488 | not run |
| zstd-3, same-path cross-tree patch | 3.488 (no file qualified) | not run |
| zstd-3, unique chunks, each chunk alone | 1.145 | 1.048 |
| zstd-3, unique chunks, in ~1 MiB groups | 0.987 | 0.873 |
| Unique chunks / index at ~40 bytes each | 44,988 / 1.7 MiB | 151,062 / 5.8 MiB |

All 418 files in the two worktrees are byte-identical to a file in the main checkout, so whole-file dedup already removes them and no patch pair exists; every unique file is in the main checkout. The chunk gain here is therefore inside one tree's build output: the unique files drop from 3.488 to 0.987 GiB (-71.7%) with 64 KiB groups. Not attributed further; the types with the most input are `.bin`, `.ap_`, `.apk` (zip containers whose members may also exist as separate files) and `.so`.

## Results, Android (2 worktrees only, 418 files)

Dropping the main checkout leaves two trees with different HEADs, which is closer to the iOS situation. 64 KiB average, GiB.

| Measure | Value |
|---|---|
| Input | 2.752 |
| Unique after whole-file SHA-256 (347 files) | 1.913 |
| Unique after chunking | 0.750 |
| zstd-3, unique files | 0.572 |
| zstd-3, same-path cross-tree patch | 0.453 (-20.8%) |
| zstd-3, unique chunks alone | 0.253 (-55.8%) |
| zstd-3, unique chunks in ~1 MiB groups | 0.223 (-61.0%) |
| Unique chunks / index | 10,342 / 0.4 MiB |

## By file type (64 KiB average, GiB, unique files only after the first column)

Types not in the list are under `other`; no-extension files (Mach-O executables, mostly) are in iOS `other` at 1.03 GiB input, then `.pcm` 0.64, `.car` 0.40. Android `other` is `.bin` 2.7, `.ap_` 1.95, `.apk` 1.06, `.ttf` 0.75, `.html` 0.57. There were no `.a` files of 1 MiB or more in either group.

iOS:

| Type | Files | Input | Whole-unique | Chunk-unique | zstd whole | zstd + patch | zstd chunks alone | zstd chunks grouped |
|---|---|---|---|---|---|---|---|---|
| `.dylib` | 35 | 5.81 | 3.48 | 2.29 | 0.77 | 0.65 | 0.67 | 0.60 |
| other | 494 | 3.38 | 2.55 | 1.89 | 0.71 | 0.63 | 0.59 | 0.54 |
| `.o` | 565 | 1.65 | 1.65 | 1.16 | 0.32 | 0.18 | 0.26 | 0.24 |
| `.swiftmodule` | 152 | 0.50 | 0.25 | 0.15 | 0.07 | 0.05 | 0.05 | 0.04 |

Android (3 trees):

| Type | Files | Input | Whole-unique | Chunk-unique | zstd whole | zstd chunks alone | zstd chunks grouped |
|---|---|---|---|---|---|---|---|
| other | 1650 | 8.04 | 5.29 | 1.10 | 2.56 | 0.38 | 0.36 |
| `.so` | 306 | 6.02 | 1.68 | 0.91 | 0.58 | 0.35 | 0.33 |
| `.jar` | 420 | 1.77 | 1.38 | 0.71 | 0.15 | 0.21 | 0.11 |
| `.dex` | 290 | 0.94 | 0.61 | 0.58 | 0.19 | 0.20 | 0.19 |

Android (2 worktrees), same columns plus patch: other 1.30 / 1.26 / 0.22 / zstd 0.40 / patch 0.29 / chunks 0.07 / grouped 0.07; `.so` 0.92 / 0.19 / 0.18 / 0.06 / 0.06 / 0.07 / 0.06; `.jar` 0.30 / 0.26 / 0.16 / 0.05 / 0.04 / 0.05 / 0.03; `.dex` 0.23 / 0.20 / 0.19 / 0.06 / 0.06 / 0.06 / 0.06.

## Cost of the chunking pass

| Group | Avg | Wall | CPU | Peak RSS |
|---|---|---|---|---|
| iOS 11.3 GiB | 64 KiB | 9.7 s | 19.9 s | 0.02 GiB |
| iOS | 16 KiB | 7.3 s | 19.9 s | 0.03 GiB |
| Android 16.8 GiB | 64 KiB | 7.0 s | 24.3 s | about 21 MiB |
| Android | 16 KiB | 14.1 s | 25.3 s | 0.03 GiB |

This pass includes the whole-file SHA-256 that stash already computes, and the files were probably in the page cache after earlier reads, so it is not the cold-disk cost and the added cost over today's hashing was not isolated. Peak memory is the per-file chunk lists plus a 4 MiB read buffer per thread. The 1.64 GiB RSS seen in the iOS compress pass comes from holding patch pairs in memory (capped at 192 MiB per file, four threads) and is a property of the measurement, not of chunking.

## Reading

- On iOS, cross-tree patching captures most of what chunking does. Patch gives 1.499 GiB against 1.575 GiB for chunks stored one by one, and 1.431 GiB for chunks stored in ~1 MiB groups. The difference between patch and grouped chunks is 0.07 GiB of 1.871, and the patch figure covers only the 460 files with a same-path base. The per-type rows point the same way: `.o` goes 0.32 to 0.18 with a patch and 0.24 with grouped chunks, so for objects a patch is better than chunks.
- Chunking wins where there is no same-path partner. `.dylib` is the largest iOS type (5.81 of 11.34 GiB): patch 0.65, grouped chunks 0.60. The 3 files skipped for the 192 MiB cap in this harness are probably among them; bale's codec goes up to 1 GiB, so the patch column may be slightly low for `.dylib`.
- On Android the main-checkout case is exact duplicates, which the store handles. What chunking adds is dedup inside a tree (zip-like containers repeating their members), which neither whole-file dedup nor a patch against the same path can see: 3.488 to 0.987 GiB in the 3-tree group, 0.572 to 0.223 GiB in the 2-worktree group. The cause was not isolated to specific files.
- Compressing each chunk alone loses ratio: iOS 1.575 against 1.431 grouped (9%), Android 1.145 against 0.987 (14%). Any chunk design would need grouped compression, which complicates reading one chunk back.
- 16 KiB chunks cut stored bytes further only when grouped (iOS 1.431 to 1.346, Android 0.987 to 0.873) and quadruple the index (10.5 and 5.8 MiB at ~40 bytes per chunk) and the per-file chunk references in manifests. With each chunk compressed alone, 16 KiB is worse on iOS (1.616 against 1.575).
- Allowing a patch base from another tree is a smaller change than chunking: `bases` in `ops/stash.rs` would be looked up by the relative path in any registered tree's last manifest instead of only the tree's own previous stash. The depth bound and lock rules would need a look, since a base blob would then be read on behalf of a different tree. This run did not implement or time it.
- Restore cost of chunks, not measured: an unstash would read the manifest's chunk list, fetch and decompress chunks (grouped chunks mean decompressing a group to get one), and write them into the existing stub in order. Writing into the stub is the same in-place fill that blob restore does today, so inode, mode and mtime handling would not change. GC would need to track chunk reachability rather than blob reachability. Index size is 1.7 to 10.5 MiB for the groups above, small against the stored bytes.
