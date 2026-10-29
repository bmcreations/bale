# Shared Swift packages: acceptance run

Run on 2026-10-29 with `target/debug/bale` built from `feat/shared-swift-packages` (446e206). Fixture: Flipcash iOS (`Code.xcodeproj`, `Flipcash` scheme, `generic/platform=iOS Simulator`, `CODE_SIGNING_ALLOWED=NO`), built into the real global DerivedData. bale's state lived in a temp `BALE_HOME`; `HOME` was untouched. Xcode 27.1, macOS 27.2. Machine load was high and varied during the run (load average 22 to 160), so wall times are only indicative. One run per case.

## What was run

1. The main checkout (`t0/ios`) got one full build with no bale involvement, so its `SourcePackages` was finished.
2. Three worktrees `w1`..`w3` were made with `git worktree add --detach` from the main checkout and registered with `bale adopt`, which calls `register`, the same function the WorktreeCreate hook falls back to.
3. The three full builds were started at the same moment, then the three no-op builds at the same moment.
4. In `w1`, the `swift-atomics` pin in `Package.resolved` was edited from 1.3.0 to 1.2.0 (a tag already present in the shared `repositories/`). `bale hook claude pre-tool` ran with a synthetic Bash PreToolUse payload whose `cwd` was `w1`. `w1` was built, then `w1`, `w2` and `w3` were built again at the same moment.

## Links and resolve

| Check | Result |
|---|---|
| Shared folders after registering w1..w3 | 1 (`spm/057bc4d367e8f181`) |
| Links created | 3 (one `SourcePackages` symlink per worktree DerivedData folder, all to that key) |
| Resolve failures in the 4 concurrent-or-later full builds (grep for `could not resolve` / `failed to resolve`) | 0 |
| `error:` lines in any build log | 0 |

## Builds

| Build | Exit code | Wall time | `SwiftCompile` lines | `Ld` lines |
|---|---|---|---|---|
| main, full, alone, no sharing | 0 | 404 s | 6216 | 212 |
| w1, full (3 at once) | 0 | 574 s | 6216 | 212 |
| w2, full (3 at once) | 0 | 573 s | 6216 | 212 |
| w3, full (3 at once) | 0 | 573 s | 6216 | 212 |
| w1, no-op (3 at once) | 0 | 39 s | 0 | 0 |
| w2, no-op (3 at once) | 0 | 33 s | 0 | 0 |
| w3, no-op (3 at once) | 0 | 38 s | 0 | 0 |

The three full builds are not faster than the lone main build because they ran together on a loaded machine. The wall times say nothing about the feature.

## Disk

`du -sk` of each DerivedData folder. `du` does not follow the `SourcePackages` symlink, so the linked folders exclude the shared copy.

| Folder | Size |
|---|---|
| main DerivedData, unlinked (own `SourcePackages` 2.27 GiB) | 9.43 GiB |
| w1 DerivedData (linked) | 7.29 GiB |
| w2 DerivedData (linked) | 7.29 GiB |
| w3 DerivedData (linked) | 7.29 GiB |
| shared `spm/<key>` | 2.27 GiB |

Three trees unlinked would take 3 x 9.43 = 28.28 GiB, taking the main checkout's folder as the per-tree size. Three linked trees plus one shared folder take 24.14 GiB, so 4.14 GiB less. Each linked folder is 2.14 GiB smaller than the unlinked one (the shared copy is 2.27 GiB; the 0.13 GiB gap is other contents differing between the main folder and the worktree folders, not measured). With N trees the saving is N x 2.14 - 2.27 GiB, so each extra tree beyond the first saves about 2.14 GiB, about 2.27 GiB by `SourcePackages` size.

The 11.1 GiB figure from Task 0/1 was measured on an in-tree `-derivedDataPath .dd`, not on global DerivedData, and is not comparable with these numbers. Not measured: physical blocks used by the APFS-cloned second key (see below); `du` reports its apparent size.

## Pin bump

| Step | Result |
|---|---|
| Edit | `swift-atomics` 1.3.0 (`b601256e`) to 1.2.0 (`cd142fd2`) in `w1/.../Package.resolved` |
| `bale hook claude pre-tool` (cwd `w1`) | exit 0; `w1`'s link moved from key `057bc4d367e8f181` to `ca9d3d14b0351b9a`; `w2` and `w3` stayed on `057bc4d367e8f181` |
| New key's folder | 2.27 GiB apparent (`du`), seeded by cloning the old key; physical use not measured |
| `w1` build after the bump | exit 0, 160 s, 6212 `SwiftCompile` lines, 200 `Ld`; the log shows `Checking out 1.2.0 of package 'swift-atomics'` |
| `w1` build again | exit 0, 21 s, 0 compiles |
| `w2` build | exit 0, 28 s, 0 compiles |
| `w3` build | exit 0, 28 s, 0 compiles |

Moving a base package such as `swift-atomics` recompiled nearly the whole graph in `w1`. That is Xcode's behaviour for a changed dependency, not a failure of sharing. `w2` and `w3` were not affected.

Every build log, including the no-ops, has about 50 `Fetching`/`Cloning`/`Updating from` lines for the packages; the builds went through SPM's update check against the network each time. This was not isolated from the timings.

## Cleanup

The four DerivedData folders this run created (three worktrees plus the main checkout's), the three worktrees and the temp bale home with `spm/` were deleted. Free space before the run was 115176268 KiB and after cleanup 110742316 KiB; the 4.2 GiB difference is not attributable to a single cause (Xcode's `ModuleCache`, `CompilationCache` and SDK caches grow during builds, and other processes were writing to the disk).
