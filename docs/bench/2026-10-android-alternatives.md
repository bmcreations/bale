# Android build output: alternatives to stash (2026-10-30)

Five checks on a Flipcash Android debug build, the Gradle twin of `2026-10-ios-alternatives.md`. Nothing here changes bale's code. Raw logs, snapshots and scripts were on the scratch image and are gone with it.

## Setup

- macOS 27.2, Apple M3 Max, 36 GiB. bale at `f27f601`; Flipcash Android (`code-android-app`) at `c934c9138`.
- Gradle 9.8.2, AGP 9.4.1, Kotlin 2.4.21 (KSP, not kapt), JDK JBR 21.0.4. The project has 146 `build/` directories (118 modules with an Android debug variant, 14 KMP modules, `buildSrc`, vendored modules).
- A 40 GB sparse APFS image (`hdiutil create -size 40g -type SPARSE -fs APFS -volname balespike-and`). Every checkout, log and the build cache were on it.
- Checkouts: `git clone --shared <code-android-app> <image>/and-a` (and `and-b`, `cache-a`, `cache-b`). Each has its own `.git`, so none enters bale's registry. The installed bale was not run.
- Build: `./gradlew --no-daemon --console=plain -I <image>/init-cache.gradle -Pkotlin.compiler.execution.strategy=in-process :apps:flipcash:app:assembleDebug`. `--no-daemon` and the in-process Kotlin compiler keep this run from starting or reusing daemons that other sessions share; it also means every build paid JVM start-up, and the Kotlin compiler shared the 6 GiB Gradle heap.
- The init script sets only the local build cache directory (to the image). The repo's committed `gradle.properties` already has `org.gradle.caching=true` and `org.gradle.configuration-cache=true`. Experiments 1-4 pass `--no-build-cache` so nothing is written to the real `~/.gradle/caches`; the configuration cache stayed on, as the repo has it.
- Dependencies came from the real `~/.gradle` (read and written there, so no re-download); nothing under it was modified by hand and its `gradle.properties` was not read or printed.
- The build failed at `processDebugGoogleServices` because the gitignored `apps/flipcash/app/google-services.json` is missing (the two failed attempts ran 33-37 s and 283-286 tasks). The spike clones got a hand-written stub for package `com.flipcash.app.android` with made-up IDs, plus a `local.properties` holding only `sdk.dir`. The real file was not copied. Everything else built without secrets.
- "Large files" means regular files of 1 MiB or more under a `build/` directory. "Tasks" are Gradle's own `N actionable tasks: X executed, Y up-to-date, Z from cache` line; per-type counts come from `> Task` lines in the `--console=plain` log.
- One run per case. The machine was shared with other agent sessions. Load is the 1-minute average at the start of the build (and at the end where it moved a lot); it ranged from 4 to 128, and swap sat at 30-31 GiB used of 31-32 GiB throughout. Timings across experiments are not comparable when the load differs.
- Physical space is `df -k` on the image after `sync`. The host's free space can't be used: a sparse image does not give blocks back.

## 1. Baseline

Fresh `and-a`, first full debug build (after the failed attempts had warmed the dependency cache).

| Case | Wall | Load at start (end) | Tasks |
|---|---|---|---|
| Full build | 200.0 s | 15.1 (11.1) | 2,639 executed, 0 up-to-date |
| No-op rebuild | 6.9 s | 9.5 (8.9) | 30 executed, 2,601 up-to-date |

| Build output in the tree | Value |
|---|---|
| `build/`, `.gradle/`, `.kotlin/` directories | 146 `build/`, one `.gradle/` (73 MiB), `.kotlin/` empty |
| Files / logical size | 57,209 files, 2,505 MiB (2,622 MiB allocated) |
| Files of 1 MiB or more | 205, 2,050 MiB |
| The 200 large files under `build/` by extension | `.so` 46 files 942 MiB, `.bin` 85 files 359 MiB, `.apk` 1 file 350 MiB, `.jar` 43 files 165 MiB, `.dex` 17 files 155 MiB |

The 30 tasks a no-op still executes are always-run tasks: `kmpPartiallyResolvedDependenciesChecker` (14) and CMake configure/build for four ABIs (16). Later no-ops took 6.7-6.8 s at load 6-17, and 8.2-12.7 s when the load was 44 and 4.5-17 respectively.

For bale: a fresh worktree of this project costs about 3.3 minutes and 2.5 GiB of tree-local build output, a fifth of the iOS figure, and the `.so` files (a third of the large-file bytes) come from CMake, not from Kotlin or AGP.

## 2. In-place APFS compression instead of stash

`/opt/homebrew/bin/afsctool` 1.7.2, `-c -T LZFSE -j8`, over a snapshot of the 200 large files under `build/` (passed by name, 100 per invocation). The snapshot was taken after a no-op build.

| Run | Files | Logical | Allocated (`st_blocks`) before -> after | Time | Image used before -> after |
|---|---|---|---|---|---|
| LZFSE | 200 | 1,989 MiB | 1,977 -> 694 MiB | 6.4 s | 5,417,424 -> 4,503,428 KiB (893 MiB freed) |

The two figures for freed space disagree: `st_blocks` says 1,283 MiB, `df` says 893 MiB. The cause was not found (the iOS run's two figures agreed within about 5%), so treat 893 MiB as the real saving and the ratio as between 1.8 (by `df`) and 2.9 (by `st_blocks`). `afsctool` printed `Unable to compress file` 200 times, as in the iOS run, yet 198 files carried `UF_COMPRESSED` afterwards. The two that did not are the 2.2 MiB and 1.7 MiB `discrete_*_table.bin.jar` files, which are already zip data.

By extension (allocated, before -> after): `.so` 46 files 929 -> 347 MiB, `.bin` 85 files 360 -> 123 MiB, `.apk` 1 file 350 -> 125 MiB, `.jar` 43 files 165 -> 43 MiB, `.dex` 17 files 155 -> 50 MiB.

### Does the file survive?

| Property | After `afsctool -c` |
|---|---|
| Inode | unchanged on all 200 |
| Size, mode | unchanged on all 200 |
| Nanosecond mtime | changed on 199 of 200 |

Same as the iOS run: the tool keeps the inode and loses the sub-microsecond part of the mtime. The mtimes were put back with `os.utime(ns=)` and then matched on all 200. All builds below ran on the restored state.

### What Gradle does with the result

| State | Wall | Load | Tasks |
|---|---|---|---|
| Uncompressed, no-op (experiment 1) | 6.9 s | 9.5 | 30 executed, 2,601 up-to-date |
| Compressed, no-op | 6.8 s | 5.9 | 30 executed, 2,601 up-to-date; the same 30 tasks |
| Compressed, leaf edit | 19.5 s | 5.7 | 40 executed, 2,591 up-to-date |
| Compressed, revert | 18.1 s | 11.7 | 40 executed |
| Decompressed (`afsctool -d`, mtimes restored), no-op | 6.7 s | 16.7 | 30 executed |
| Decompressed, same leaf edit | 17.1 s | 15.0 | 40 executed, 2,591 up-to-date |
| Decompressed, revert | 16.5 s | 13.7 | 40 executed |

Gradle re-ran nothing because of the compression, as expected from its content-hash fingerprints. The edit appended `internal val baleSpike = 1` to `apps/flipcash/features/appupdates/.../UpdateRequiredView.kt`, a module only the app depends on. Tasks that ran besides the 30: `kspDebugKotlin`, `compileDebugKotlin`, `transformDebugClassesWithAsm` and two jar bundles in `appupdates`; the same four plus `hiltSyncDebug`, `mergeLibDexDebug` and `packageDebug` in the app. No task failed reading a compressed `.jar`, `.dex`, `.so` or `.apk`: `mergeLibDexDebug` read compressed dex and `packageDebug` wrote a new APK over the compressed one.

After the compressed edit build, 191 of the 198 compressed files were still compressed; 3 had a new inode and 7 a changed mtime. The edit build was 2.4 s slower compressed (19.5 s against 17.1 s), with the load the other way round (5.7 against 15.0), so the difference is within what one run each can show. `df` rose by 285 MiB over that edit build (4,503,432 -> 4,794,624 KiB), mostly the rewritten 350 MiB APK. `afsctool -d` of the 191 files took 3.8 s and kept every inode. The cold-read cost was not measured.

For bale: the iOS conclusion carries over. Inodes survive, Gradle doesn't care, and the saving is about 0.9 GiB of 2.5 GiB here, smaller in absolute terms than iOS's 4 GiB because the whole tree is smaller. Compression is in place, so a crash mid-run leaves a mix and nothing is shared across trees. Untested: ZLIB or LZVN, a source change that touches the native libraries, and an R8 (release) build, since the debug build doesn't run R8.

## 3. Recycle a tree

`and-a`, built at `c934c9138`, switched with `git switch --detach`.

| What | 25 first-parent commits back (`eba8deb50`) | 5 commits back (`32d89638e`) |
|---|---|---|
| `git diff --stat \| tail -1` | 364 files changed, 2,259 insertions(+), 23,968 deletions(-) | 51 files changed, 179 insertions(+), 1,376 deletions(-) |
| Tracked files before / after | 4,519 / 4,390 | 4,519 / 4,506 |
| Git rewrote (new inode) | 229 | 38 |
| Git deleted / added | 132 / 3 | 13 / 0 |
| Kept their inode | 4,158 | 4,468 |
| Build after switching | 170.1 s, load 10.1 (8.1); 2,608 of 2,616 tasks executed | 155.2 s, load 11.0 (110.4); 430 of 2,638 executed |
| Build after switching back | 156.7 s, load 6.8 (15.2), run with `--info`; 2,608 of 2,638 executed | 113.6 s, load 128.4 (44.1); 430 of 2,638 executed |
| Full build, for comparison | 200.0 s, load 15.1; 2,639 executed | same |
| No-op after switching back | not run | 8.2 s, load 44; 30 executed |

Switching back from the 25-commit window rewrote 361 files (the 132 deleted plus the 229 changed), the same 4,158 kept.

The 25-commit window rebuilt essentially everything. The `--info` log of the way back gives "Class path of task ... has changed" as the reason, including for `:apps:flipcash:features:appupdates` tasks. The window changes `gradle/libs.versions.toml` (`flipcash2-client-protocol` 0.23.0 -> 0.21.0, `play-services-base` removed) and `settings.gradle.kts` (drops `:apps:flipcash:features:notification-status`). Of the other windows, every one from 20 to 50 commits back changes `libs.versions.toml` or `settings.gradle.kts` too, so none of them would have been better. Which of the two causes the classpath change was not tested, and whether `appupdates` is source-identical across the window was not checked. The 5-commit window touches no build configuration and executed 430 tasks (about 16%): 68 `compileDebugKotlin` and 66 `kspDebugKotlin`, the module-level work behind 51 changed files.

The two timings of the 5-commit window ran at loads of 11-128, so 155 s and 114 s say little about the cost; the task count is the usable figure. The 25-commit build was 15% faster than the full build with the same task count, within what the load difference allows.

For bale: git leaves 92-99% of tracked files alone, so recycling is cheap on the disk side. Whether it saves build time depends on whether the window changes the version catalog or `settings.gradle.kts`; when it does, Gradle rebuilds all 2,600 tasks and a recycled tree costs what a fresh one does. A tree a few commits behind costs about a sixth of the tasks.

## 4. Clone of a warm base

`and-a` (built at `c934c9138`, no-op clean) copied to `and-b` on the same image with `cp -c -R`, including `build/`, `.gradle/` and the shared clone's `.git/objects/info/alternates`. Build in `and-b`, with `--info`.

| Step | Wall | Load | Tasks | Image used |
|---|---|---|---|---|
| `cp -c -R and-a and-b` | 11.6 s (sys 11.1 s) | 35.6 | - | 5,520,272 -> 5,553,528 KiB (+32 MiB; inodes 114,763 -> 203,210) |
| Build 1 | 236.7 s | 28.4 (21.2) | 2,624 of 2,638 executed | 7,694,796 KiB (+2.04 GiB) |
| No-op | 9.9 s | 17.4 | 30 executed, 2,601 up-to-date | - |

The copy preserved mtimes (checked on a source file and the APK) and, as expected, gave every file a new inode. The build succeeded; the configuration cache entry was stored fresh, not reused, and nothing errored, so nothing had to be deleted.

It was not incremental. Of 2,624 "not up-to-date" reasons in the log, 2,473 begin with an `Output property ... file ... has been removed`, and the lines name the source tree's paths, for example `.../and-a/buildSrc/build/kotlin/compileKotlin/classpath-snapshot/shrunk-classpath-snapshot.bin`. That reads as the copied `.gradle/` execution history pointing at the old absolute output paths; the history files were not inspected to confirm it. Another 117 tasks reported `Value of input property 'generatedPngsOutputDir' has changed`, an absolute-path input in one of the project's convention plugins. Even `:buildSrc` rebuilt.

The build took 236.7 s against 200.0 s for a fresh one, at a load of 28 against 15, so it saved no time. It saved no space either: the clone cost 2.04 GiB after the build, against 2.14 GiB (3,174,820 -> 5,417,120 KiB) for the first full build in `and-a`, because the rebuild rewrote the cloned outputs.

For bale: a `cp -c -R` base buys nothing for a Gradle tree at a new path. The iOS clone failed outright on the module cache; Gradle kept working but redid everything. Untested: copying without `.gradle/`, and relocating with the build cache (experiment 5).

## 5. Gradle build cache across paths

`cache-a` and `cache-b` are fresh clones at `c934c9138` (both have the stub `google-services.json`, same content), at different directory names under the same parent. `--build-cache`, with the cache directory (`<image>/bcache`) empty at the start.

| Build | Wall | Load | Tasks |
|---|---|---|---|
| A, `cache-a`, empty cache | 178.7 s | 15.8 (8.2) | 1,993 executed, 646 from cache |
| B, `cache-b`, A's cache | 44.4 s | 7.8 (5.9) | 1,091 executed, 1,548 from cache |
| Baseline (experiment 1, no cache) | 200.0 s | 15.1 (11.1) | 2,639 executed |
| B, no-op | 12.7 s | 4.5 | 30 executed |

The cache held 1,026 entries, 147 MiB (150,652 KiB) after A and 1,028 entries after B.

A's 646 hits came from the same build: tasks with identical inputs in different modules (`extractDeepLinksDebug` 116, `javaPreCompileDebug` 110, `generateDebugResources` 102, `processDebugNavigationResources` 102, `parseDebugLocalResources` 101, `compileDebugLibraryResources` 100 hit, while 17 of that type ran). That is also why A was a little faster than the baseline.

B, by task type:

| Type | B from cache | B executed |
|---|---|---|
| `compileDebugKotlin` | 117 of 117 | 0 |
| `kspDebugKotlin` | 79 of 79 | 0 |
| `compileDebugJavaWithJavac` | 83 of 85 | 2 |
| `transformDebugClassesWithAsm` | 75 of 75 | 0 |
| `compileDebugLibraryResources` | 117 of 117 | 0 |
| `mergeDebugResources`, `processDebugResources` | 1 each | 0 |
| `dexBuilderDebug`, `mergeProjectDexDebug`, `mergeLibDexDebug`, `mergeExtDexDebug` | 1 each | 0 |
| `hiltAggregateDepsDebug`, `hiltJavaCompileDebug` | 1 each | 0 |
| `packageDebug`, `mergeDebugNativeLibs` (3), `mergeDebugJavaResource` | 0 | 1, 3, 1 |
| CMake configure and build, 4 ABIs | 0 | 16 |
| `mergeDebugAssets`, `mapDebugSourceSetPaths`, `mergeDebugJniLibFolders` | 0 | 118 each |
| `bundleLibCompileToJarDebug`, `bundleLibRuntimeToJarDebug`, `processDebugJavaRes`, `writeDebugAarMetadata`, `copyDebugJniLibsProjectOnly` | 0 | 117 each |
| the same kinds of task in the 14 KMP modules | 0 | 14 each |

The Gradle build cache is not keyed by checkout path for this project, so unlike Xcode's compilation cache it works across trees. Kotlin compile, KSP, AGP dexing and resource merging all hit. What still executes is either not cacheable by design (CMake, `packageDebug`, native-lib merging) or cheap per module, but 936 of the executed tasks are small copy and merge tasks, one per module.

B's tree is not the same as a built one. Its large files total 1,704 MiB against 2,050 MiB in `cache-a`, and 126 of them against 205: the `.bin` files, which are Kotlin incremental caches, are 18 MiB against 359 MiB. `.so`, `.apk`, `.jar` and `.dex` bytes are identical. So B's next Kotlin edit probably can't compile incrementally; that was not tested. The 942 MiB of `.so` files are rebuilt by CMake in every tree and are in no cache, which keeps a fresh worktree's largest single piece of output.

B ran at load 7.8 against 15.1 for the baseline, so 44.4 s against 200.0 s overstates the ratio somewhat, but the 1,548 cache hits are the evidence, not the clock.

For bale: for Gradle, sharing work between worktrees at different paths is something the build tool already supports, by a cache of 147 MiB for a 2.5 GiB tree. That does not remove the disk cost of build output in each tree, only the time to produce it. Untested: the cache on a second, differently named parent directory; cache growth as sources change; eviction (Gradle removes entries unused for seven days by default); and the shared cache under two concurrent builds.
