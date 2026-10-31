# iOS build output: alternatives to stash (2026-10-30)

Five checks on a Flipcash iOS build, to see whether anything beats stashing build files into the store. Nothing here changes bale's code. Raw logs, snapshots and scripts were on the scratch image and are gone with it.

## Setup

- Xcode 27.2 (27B52), macOS 27.2, Mac15,10 (M3 Max, 14 cores, 36 GB). bale at `f27f601`; Flipcash (`code-ios-app`) at `89a96b05b`.
- A 45 GB sparse APFS image (`hdiutil create -size 45g -type SPARSE -fs APFS -volname balespike`). Every checkout, `.dd`, log and cache was on it.
- Checkouts: `git clone --shared <code-ios-app> <image>/ios-a` (and `ios-b`, `ios-c`). Each has its own `.git`, so none enters bale's registry. The installed bale was not run.
- Build: `xcodebuild -project Code.xcodeproj -scheme Flipcash -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO -derivedDataPath <tree>/.dd build`. It succeeded without the gitignored secrets files, which were not copied. SwiftPM fetched into the real `~/Library` caches.
- "Large files" means regular files of 1 MiB or more under `.dd/Build`: 419 files, 5,658 MiB after the first build; 422-425 files, 5.7-5.8 GiB in later states.
- Counts come from lines in the xcodebuild log that start with the task name (`SwiftCompile`, `Ld`, `Copy`, `CompileC`). They count tasks that ran or were checked, not CPU time. `CompileSwift` and `CodeSign` were 0 in every log (`CODE_SIGNING_ALLOWED=NO`; Xcode 27 logs `SwiftCompile`).
- One run per case. The machine was shared with other agent sessions; load is the 1-minute average at the start of the build (and at the end where it moved a lot). Swap was 30-37 GB used of 31-39 GB throughout. Timings across experiments are not comparable when the load differs, and it did, from 4 to 95.
- Physical space is `df -k` on the image, taken after `sync`. The host's free space can't be used: a sparse image does not give blocks back to the host.
- `sudo purge` asks for a password, so cold-cache reads could not be forced that way.

## 1. Baseline

| Case | Wall | Load at start | Tasks |
|---|---|---|---|
| Full build, `ios-a` | 473 s | 52 (22 at end) | SwiftCompile 6,444, CompileC 2,076, Ld 212, Copy 674 |
| Full build, fresh clone `ios-c` | 393.5 s | 13 (36 at end) | identical counts |
| No-op rebuild, first | 29.1 s | 17.9 | Copy 67, nothing else |
| No-op rebuild, second | 19.8 s | 14.2 | all 0 |
| No-op rebuilds later in the day | 13.9-16.2 s | 4-9 | all 0 |

`.dd` is 11,422,500 KiB by `du -sk` (10.9 GiB) after the first build and 11,397,620 KiB after the second.

The two full builds ran the same tasks but differed by 80 s, so wall time for a full build on this machine depends on the load more than on anything in the tree. The warm-tier figure of 15-17 s for a no-op holds when load is under about 10; the first no-op after a full build copied 67 files and took 29 s.

For bale: a fresh worktree of this project costs 6.5-8 minutes and 11 GiB, and the comparison figures below are against these two builds.

## 2. In-place APFS compression instead of stash

Tool: `/opt/homebrew/bin/afsctool` 1.7.2. It has no minimum-size filter (`-m` is a maximum), so the files were passed by name, from a snapshot of every regular file of 1 MiB or more. `-c -T LZFSE -j8` compressed them. It printed `Unable to compress file` for all 422 files in the first run, yet every file carried the compressed flag (`UF_COMPRESSED`) afterwards and allocated blocks dropped, so the message is not reliable; the cause was not looked into.

| Run | Files | Logical | Allocated before -> after | Ratio | Time | Image used before -> after |
|---|---|---|---|---|---|---|
| LZFSE, first | 422 | 5,733 MiB | 5,733 -> 1,612 MiB | 3.56 | 37.8 s | 11,657,016 -> 7,502,560 KiB (3.96 GiB freed) |
| LZFSE, second, from decompressed | 425 | 5,809 MiB | 5,980 -> 1,617 MiB | 3.70 | 36.8 s | 11,977,712 -> 7,537,612 KiB (4.23 GiB freed) |
| ZLIB | 425 | 5,809 MiB | 5,811 -> 4,511 MiB | 1.29 | 10.7 s | 11,810,920 -> 10,508,096 KiB (1.24 GiB freed) |

ZLIB compressed 391 of 425 files and left every file over 100 MiB alone, including all the `*.debug.dylib` copies (the 468 MiB `Flipcash.debug.dylib` among them). Why was not looked into. LZFSE compressed the same dylibs to about a quarter of their size (the 468 MiB one to 120 MiB allocated).

### Does the file survive?

| Property | After `afsctool -c` | Notes |
|---|---|---|
| Inode | unchanged on all files | in all three runs |
| Size, mode | unchanged on all files | |
| Nanosecond mtime | changed on 420 of 422 (LZFSE first), 279 of 425 (LZFSE second), 277 of 425 (ZLIB) | The two files that kept theirs have a nanosecond part that is a multiple of 1000. The changes seen were -128 to -690 ns. The tool restores the mtime with microsecond precision, so the sub-microsecond part is lost. |
| `afsctool -d` | inode and mtime unchanged on a mixed state of 425 files | It restores what is there; it does not repair a truncated mtime. |

### What Xcode does with the result

| State before the no-op build | Wall | Load | Ld | Copy | Files with a new inode afterwards |
|---|---|---|---|---|---|
| Compressed, mtime truncated (first run), build 1 | 73.2 s | 8.3 | 111 | 49 | 277 of 422 |
| same, build 2 | 32.5 s | 5.5 | 51 | 3 | - |
| same, build 3 | 25.6 s | 6.4 | 30 | 2 | - |
| same, build 4 | 14.9 s | 5.6 | 0 | 0 | - |
| Compressed, ns mtime put back with `os.utime(ns=)` (second run), build 1 | 22.2 s | 4.7 | 0 | 0 | 0 (1 mtime changed) |
| same, build 2 | 18.8 s | 8.9 | 0 | 0 | - |
| Uncompressed, same day | 13.9-14.9 s | 4-6 | 0 | 0 | 0 |

With the truncated mtimes Xcode relinked and took three extra builds to settle, the same pattern Task 0 saw for new inodes. With the mtimes restored it did no work, but the build was 4-8 s slower than uncompressed, which fits it reading 1.6 GB through decompression. Ordinary build output is also rewritten by the build itself: after the edit build below, 411 of 425 files were still compressed, the rest had been rewritten.

Reading all 425 files with `cat > /dev/null` after the image was detached and re-attached took 6.4 s, then 2.5 s on the second pass (LZFSE state, load 8.5). The same read of the uncompressed state was not measured cold: the detach failed with "Resource busy" (a stray `ibtoold` held the volume), and the uncompressed read that was done was 3.4 s and 0.45 s warm. So the cold-read cost of compression is only known for the compressed side.

### Edit in a leaf file

One line appended to `Flipcash/Core/Screens/Main/Home/WalletCardGeometry.swift`.

| State | Wall | Load | SwiftCompile | Ld | Copy |
|---|---|---|---|---|---|
| Compressed (424 of 425 files flagged) | 39.5 s | 8.5 | 18 | 4 | 6 |
| Uncompressed, edit | 34.0 s | 5.6 | 18 | 4 | 6 |
| Uncompressed, revert | 35.7 s | - | 18 | 4 | 6 |

Writing into compressed outputs works: the build succeeded, 13 of 425 files got a new inode, 10 changed size, and allocated space went from 1,668 to 2,390 MiB. The cost was about 4-5 s over uncompressed, with the load differing slightly.

An earlier attempt to compare went wrong and is worth knowing: after `afsctool -d` on a state that had been through a build on top of truncated mtimes, a revert of the source edit set off 110 links, then 51, 30, 0, the same sequence as above. That one is the truncated-mtime case, not a cost of decompression.

### Reading

`afsctool` keeps the inode, which is the property bale's stash is built around, and frees 4.0-4.2 GiB of 5.7 GiB here without any store. But it truncates the nanosecond mtime, so a bale that used it would have to put the mtime back itself (one `utimensat` per file, about as cheap as what unstash already does). With that in place the no-op was free of relinks. Open points: the 4-8 s no-op penalty and the 5 s edit penalty are single runs at different loads; the cold read of uncompressed data was not measured; compression happens in place in the working tree, so a crash mid-run leaves a mix and nothing is deduplicated across trees (stash stored 990 MiB for several 5.4 GiB stashes in Task 10). ZLIB is not usable because it skipped the large dylibs.

## 3. Duplicate appex dylibs by clone

The three `*.debug.dylib` pairs (NotificationContent 346 MB, NotificationService 304 MB, ShareExtension 350 MB) are byte-identical (`cmp`) between `Flipcash.app/PlugIns/X.appex/` and `Debug-iphonesimulator/X.appex/`. A script replaced the victim with `cp -c twin tmp`, copied the mode, set the nanosecond mtime from the original with `os.utime(ns=)`, and renamed over it. Inode, size and mode checked afterwards; mtime in ns equalled the original on all six.

| Step | Image used / change | Wall | Load | Ld | Copy | Notes |
|---|---|---|---|---|---|---|
| Replace the 3 `PlugIns/` copies by clones of the `Debug-iphonesimulator/` ones | 11,859,324 -> 10,882,228 KiB (954 MiB freed) | - | - | - | - | inodes 180790, 181471, 181499 became 182141-182143 |
| No-op build | - | 17.1 s | 6.6 | 0 | 0 | |
| No-op build, again | - | 16.5 s | - | 0 | 0 | the three inodes were still 182141-182143 |
| Replace the 3 `Debug-iphonesimulator/` copies by clones of the `PlugIns/` ones | 10,882,228 -> 10,864,328 KiB | - | - | - | - | inodes became 182164-182166; nothing more to free, since the twins already shared blocks |
| No-op build | - | 15.5 s | 5.8 | 0 | 0 | |
| No-op build, again | - | 14.2 s | - | 0 | 0 | |
| Leaf edit, as in experiment 2 | - | 39.0 s | - | 4 | 6 | build succeeded; none of the six dylib inodes changed |
| Revert | - | 37.9 s | - | 4 | 6 | |

Xcode neither relinked nor re-copied in either direction. The second direction was applied on top of the first, so it is "replace both copies in turn", not each direction in isolation; the first direction alone was already enough to save the space. The leaf edit does not touch the appex dylibs, so whether a source change that does (and so makes Xcode rewrite a cloned file) is handled was not tested.

For bale: the 919 MiB (954 MiB here) that Task 10 hoped to save by cloning can be had by cloning one dylib of each pair from the other, which the current unstash rule skips because both sit under `Build/Products/`. Xcode accepted it here for two builds.

## 4. Recycle a tree

`ios-a`, built at `89a96b05b`, switched with `git switch --detach b41020e7f`, 25 first-parent commits back (`git diff --stat`: 213 files changed, 496 insertions, 12,936 deletions).

| What | Value |
|---|---|
| Tracked files before / after | 3,346 / 3,272 |
| Git rewrote (new inode) | 139 |
| Git deleted | 74 |
| Kept their inode | 3,133 |
| Build after switching | 110.5 s, load 8.1 (26.2 at end); SwiftCompile 2,756, CompileC 0, Ld 30, Copy 71 |
| Build after switching back | 80.2 s, load 22.4; SwiftCompile 2,807, Ld 30, Copy 71 |
| Full build, for comparison | 393.5 s (load 13-36) and 473 s (load 52-22); SwiftCompile 6,444, CompileC 2,076, Ld 212 |
| No-op after the switch back | 14.5 s |

The switch-and-build ran about a quarter to a third of the wall time of a full build and 43% of the Swift compile tasks, with no C compiles. The older commit has fewer files, and the load and the direction differ between the runs, so the 80 and 110 s are not a clean pair. A diff of 213 files cost 2,756 `SwiftCompile` tasks, so the work is module-level recompilation, not one task per changed file.

For bale: a recycled tree needs no new 11 GiB, and git only touched 213 of 3,346 files. Whether that beats a clone of a warm base depends on how far the new commit is from the tree's last build, which this run varied only once.

## 5. Clone of a warm base, and the compilation cache

### 5a. `cp -c -R` of the built clone

`ios-a` (built, no-op clean) copied to `ios-b` on the same image; build with `-derivedDataPath ios-b/.dd`.

| Step | Wall | Load | Tasks | Image used |
|---|---|---|---|---|
| `cp -c -R ios-a ios-b` | 18.5 s (sys 16.4 s) | 17.1 | - | 11,985,744 -> 12,018,348 KiB (+32 MiB, inode count doubled) |
| Build 1 | 89.0 s, **failed** | 13.9 | SwiftCompile 774, Ld 40, Copy 189 | 12,829,724 KiB (+811 MB) |
| Build 2 after deleting `ios-b/.dd/ModuleCache.noindex` | 124.3 s | 24.7 | SwiftCompile 5,672 of 6,444, CompileC 492 of 2,076, Ld 166 of 212, Copy 485 of 674 | 20,723,044 KiB (+8.3 GiB in total vs. before the build) |
| No-op | 14.4 s | - | all 0 | - |

The first build failed on both architectures: `precompiled file '.../ios-b/.dd/ModuleCache.noindex/.../SwiftShims-....pcm' was compiled with module cache path '.../ios-a/.dd/ModuleCache.noindex/...'`, then `missing required module 'SwiftShims'`. The module cache stores absolute paths. Deleting that directory was the only fix tried.

The build that worked was close to a full rebuild by task count, though 124 s against 393-473 s. The task lines don't say how much of that was real compile work, and the load differs. The clone ended up taking 8.3 GiB of its own, 73% of a fresh `.dd`, because the build rewrote almost everything it had cloned.

### 5b. `COMPILATION_CACHE_ENABLE_CACHING=YES`

`xcodebuild -showBuildSettings` shows `COMPILATION_CACHE_CAS_PATH` (default `<dd>/CompilationCache.noindex`). Both builds used `COMPILATION_CACHE_ENABLE_CACHING=YES COMPILATION_CACHE_CAS_PATH=<image>/cas`. Flipcash builds with caching on; there were no errors. Hit/miss comes from the `Cache hit` / `Cache miss` lines in the log.

| Build | Path | Wall | Load | Hits | Misses | CAS size |
|---|---|---|---|---|---|---|
| A, `ios-a`, empty CAS | `.../ios-a` | 285 s | 28 (95 at end) | 0 | 4,699 | 3,325,976 KiB |
| B, `ios-c` with no `.dd` (never built before), A's CAS | `.../ios-c` | 344 s | 83 (23 at end) | 1,427 | 3,134 | 5,279,132 KiB |
| C, `ios-c` again with `.dd` deleted, same CAS | `.../ios-c` | 218 s | 32 (8 at end) | 4,503 | 0 | 5,320,324 KiB |

B's hits by task: `SwiftExplicitDependencyGeneratePcm` 800 and `PrecompileModule` 627, which are module precompiles. Its misses: `CompileC` 1,584, `SwiftCompile` 1,265, `SwiftEmitModule` 113, `SwiftExplicitDependencyGeneratePcm` 118, `PrecompileModule` 54. No `SwiftCompile` or `CompileC` task hit at a different path. When C rebuilt `ios-c` at the same path B had used, every `CompileC`, `SwiftCompile` and `SwiftEmitModule` task hit. So the keys include the checkout path; the cache is not path-independent for this project. Path remapping options were not tried.

B took 344 s against 393-473 s for builds without the cache, which the load swing (83 down to 23) makes meaningless as a comparison. C, with every lookup a hit, still took 218 s and 681 `PrecompileModule`, 212 `Ld` and 674 `Copy` tasks ran: caching removes compile work, not linking or copying. The CAS grew to 3.3 GiB after A and 5.3 GiB after B.

For bale: a warm base cloned to a new path doesn't give a cheap build (5a: the module cache breaks, and the rebuild rewrote 8.3 GiB); and the compilation cache doesn't share work between worktrees at different paths, though it would for a tree rebuilt in place (C).
