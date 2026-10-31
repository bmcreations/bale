# Alternatives to stash and per-task worktrees: summary (2026-10-30)

Six ideas were measured and three were checked by reading the code, to find space or build time that the current levels (stash, baled, cold) and shared
checkouts leave on the table. Each measured result has its own file; this one ranks them and links the
evidence. Every timing is one run on a shared, loaded machine (load 4 to 128, 30 GiB of swap), so compare
timings only within a file, and only where that file says the load was similar.

| File | Covers |
|---|---|
| [ios-alternatives](2026-10-ios-alternatives.md) | APFS in-place compression, duplicate appex dylibs, recycling a tree, `cp -c -R` warm base, Xcode compilation cache |
| [android-alternatives](2026-10-android-alternatives.md) | APFS in-place compression, recycling a tree, `cp -c -R` warm base, Gradle build cache |
| [chunk-dedup](2026-10-chunk-dedup.md) | Content-defined chunking and cross-tree patch bases for the store |
| [linux-inplace](2026-10-linux-inplace.md) | btrfs `defragment -czstd` and `FIDEDUPERANGE`, in a Docker Desktop VM |

## Worth building

**In-place compression as a warm tier that needs no restore.** `afsctool -c -T LZFSE` on files of 1 MiB or more
under `.dd/Build` freed about 4.1 of 5.7 GiB in 37 s. Under Android `build/` it freed 893 MiB of 1,989 MiB by
`df` in 6.4 s (`st_blocks` says 1,283 MiB; the gap wasn't explained). Both kept every inode. Both truncated mtimes to
microseconds. With the mtimes truncated, Xcode relinked over four builds; with the nanosecond mtimes put back,
a no-op build linked nothing (22 s and 19 s, against 14 s uncompressed), and Gradle re-ran nothing. A leaf
edit built normally on compressed output on both platforms. On btrfs, `defragment -czstd` kept inode, mtime
and ctime, and brought an Android `build/` to 36.6% of its size.

Stash frees more per tree: 5.8 GiB, against 4.1 GiB, in the [warm-tier bench](2026-10-warm-tier.md). Its cost
is a 3 to 11 s restore, which needs a hook that fires before the build. Only Claude Code has such a hook (see
"Reach" below). A compressed tree needs no restore. Compressing would still be a sweep step on an idle tree, under the same
lock and busy checks as stash. So this tier would also work for T3 Code, Codex, Cursor
and Conductor trees, which today go straight from hot to baled after 4 h. Untested: the read cost during a
large incremental build, files held open by a running build, and Cargo.

**Clone the duplicate appex dylibs.** Replacing each `*.debug.dylib` pair under `Build/Products/` with a
`clonefile` copy of its twin freed 954 MiB on Flipcash. In both directions Xcode neither relinked nor
re-copied anything, over the two builds that were run. The warm-tier bench noted that the
`Debug-iphonesimulator` copy may be the linker's own output, so this needs more builds, including archive
and device builds, before it counts as settled. Unstash's twin rule could take
these pairs, and so could a pass over hot trees.

**Point every tree at one Gradle build cache.** A fresh clone at another path built in 44 s, against 179 s,
with 1,548 of 2,639 tasks from the cache, at a lower load (7.8 against 15.1). Kotlin compile, KSP, dexing and
resource merging all hit. The cache held 147 MiB. CMake still rebuilt the 942 MiB of `.so` files, and the
Kotlin incremental caches weren't restored (18 MiB against 359 MiB), so the next Kotlin edit in that tree may
not compile incrementally; that wasn't tested. bale doesn't configure build tools today, so turning this on (an init script, or
`org.gradle.caching`) would be a new kind of work for `bale-hooks`.

**Let the store take a patch base from another tree.** Today the base can only be the same tree's previous
stash. Across three iOS trees, zstd over the unique files stores 1.871 GiB. With a sibling tree's file at the
same path as the base, that drops to 1.499 GiB. Content-defined chunking reaches 1.431 GiB. The cross-tree
patch gets most of chunking's gain on iOS without changing the store format. On two Android worktrees,
chunking does much better than the patch (0.223 against 0.453 GiB, from 0.572), so chunking is worth it where
Android trees dominate. The chunking pass cost 7 to 14 s for 11 to 17 GiB, probably from page cache.

## Worth building, with limits

**Recycle a tree instead of creating one.** Switching a built Flipcash clone 25 commits back (213 files
changed) rebuilt in 110 s with 2,756 of 6,444 `SwiftCompile` tasks, against a 393 to 473 s full build. On
Android the same distance rebuilt 2,608 of 2,616 tasks. Every window from 20 to 50 commits back touches
`libs.versions.toml` or `settings.gradle.kts`, and that changes every task's classpath. Five commits back ran
430 of 2,638 tasks. Recycling pays off when the incoming branch is close to the tree's last build and doesn't
change Gradle's build files. It would also replace most new-worktree disk use, but this run didn't measure
that.

**Linux dedup of hot trees.** `FIDEDUPERANGE` (through `duperemove`) kept inodes and mtimes on btrfs and XFS,
and a write to one copy left the other alone. At 4 KiB blocks it freed 46 to 60% across copies of Android
`build/`, but only 12 to 18% at duperemove's default 128 KiB. Defragmenting with compression un-shares
extents, so dedup has to run after compression. Run in that order, three trees ended at 13% of their
uncompressed size (215 of 1,597 MB). All of this ran
in a VM, and no build was run on the result.

## Dead ends

- **`cp -c -R` of a warm tree.** Xcode failed at the new path until `ModuleCache.noindex` was deleted, then
  re-ran 5,672 of 6,444 `SwiftCompile` tasks. Gradle ran 2,624 of 2,638 tasks, and the copy grew by 2.04 GiB
  against 2.14 GiB for a fresh build. Both build tools key their state on absolute paths, so a cloned base
  saves neither time nor space after the first build.
- **Xcode compilation caching across paths.** With `COMPILATION_CACHE_ENABLE_CACHING=YES` and one cache
  directory, a clone at another path got 1,427 hits, all module precompiles, and 0 `SwiftCompile` or
  `CompileC` hits (344 s). The same path hit every lookup. For Flipcash on Xcode 27.2 the cache depends on the
  path.
- **APFS zlib.** 1.29x, and afsctool skipped every file over 100 MiB.

## Reach (code reading, not measured)

- **Restore triggers for other tools.** T3 Code's per-turn script runs after the turn
  (`crates/bale-hooks/src/t3.rs`). Codex, Cursor and Conductor offer only create and remove hooks. None of
  them can restore stashed files before a build reads them, which is why in-place compression matters for
  them.
- **Cargo and npm.** `regenerable_dirs` returns nothing when no build system is detected
  (`crates/bale-core/src/detect.rs`), so `regenerable = ["target"]` does nothing in a pure Rust repo. Adding
  `Cargo.toml` and `package.json` markers is small. Stashing Cargo output is risky: a stub's mtime becomes the
  stash time (`ops/stash.rs`), Cargo trusts its own fingerprints, and a build run before the restore would
  likely read empty `.rlib` files rather than rebuild. `node_modules` should be baled, never stashed.
- **Sparse checkout.** `bale new` falls back to `git worktree add` (`ops/new.rs`), and cold refuses a sparse
  tree (`ops/cold.rs`). `share` already works on one but still checks skip-worktree entries, which it then
  reports as missing (`clones/tracked.rs`). To fill only the sparse set, `materialize` would need to set
  skip-worktree bits and filter its `checkout-index` list. Supporting cold would need open to restore the
  sparse state.
