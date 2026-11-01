# What an agent's Flipcash DerivedData holds (2026-11-01)

The October benches priced a Flipcash iOS worktree at 11 GiB. That figure comes from the bench's own build,
which made two architectures. The agent builds found on this machine made one, and come to about 3.3 GiB of
build output plus 2.3 GiB of Swift packages that bale already shares between trees.

Setup: read-only `du -sk` of DerivedData folders already on disk, M3 Max, Xcode 27.2 installed at the time of
reading. The folders were built by agents on 29 and 31 October; the Xcode version they were built with was
not recorded. The architectures are the directory names under `Objects-normal/` in
`Build/Intermediates.noindex`. Sizes are GiB of 1,073,741,824 bytes.

## Two agent builds

`/tmp/dd-baseline` and `/tmp/dd-kb` are `-derivedDataPath` folders from agent sessions: the first built the
main checkout, the second the `group-ready-bubble` worktree. Each holds one configuration,
`Debug-iphonesimulator`, and every `Objects-normal/` directory sampled (the first 40) contains only `arm64`.

| Part | dd-baseline | dd-kb |
|---|---|---|
| `Build/Products` | 1.58 | 1.63 |
| `Build/Intermediates.noindex` | 1.19 | 1.22 |
| `ModuleCache.noindex` | 0.25 | 0.25 |
| `Index.noindex` | 0.17 | 0.17 |
| `SourcePackages` | 2.27 | 0 (bale's `spm` link) |
| Total by `du -sk`, with logs and the rest | 5.79 | 3.59 |

The checkouts themselves are 0.14 GiB (`group-ready-bubble` and `code-ios-app-flaky-tests`).

So one tree with one agent build costs about 5.8 GiB without bale and about 3.6 GiB with its Swift packages
shared.

## Why the bench measured 11 GiB

The October benches built with `-destination 'generic/platform=iOS Simulator'`, which builds arm64 and
x86_64. The clone build in [ios-alternatives](2026-10-ios-alternatives.md) failed "on both architectures",
and the appex debug dylibs show the difference:

| Dylib | Bench (both archs) | dd-kb (arm64) |
|---|---|---|
| `NotificationContent.debug.dylib` | 346 MB | 173 MB |
| `NotificationService.debug.dylib` | 304 MB | 152 MB |
| `ShareExtension.debug.dylib` | 350 MB | 175 MB |

The bench's `.dd` was a fresh `-derivedDataPath` with no shared packages, so it likely held its own
`SourcePackages` too; the bench files don't give its size. This reading didn't rebuild to split the gap
exactly.

An agent can still reach the bench's size: a "does it compile" build against the generic destination makes
both architectures.

## A long-lived tree grows by variant

DerivedData keeps every configuration a tree has built. The main checkout's folder in
`~/Library/Developer/Xcode/DerivedData` is 13.99 GiB across three:

| Part | GiB |
|---|---|
| `Products/Variant-TSan` | 2.34 |
| `Intermediates.noindex/Variant-TSan` | 1.81 |
| `Products/Debug-iphonesimulator` | 1.93 |
| `Products/Debug-iphoneos` | 1.64 |
| `SourcePackages` | 2.28 |

A worktree that also ran device builds or TSan tests would grow the same way, so 11 to 14 GiB is the size of
a tree that has built several variants, not of a fresh one.

## What this changes

- Per-tree savings in the October files are against a two-architecture build. Ratios still hold. The
  bench's 10.9 GiB `.dd` is 1.9 times an agent tree with its own packages (5.8 GiB) and 3.3 times the build
  output alone (3.3 GiB).
- The second copies of the appex dylibs are 477 MiB of dd-kb's 1.63 GiB of Products, so cloning them would free about 15% of
  an agent tree.
- Agents already build one architecture and the index store is 0.17 GiB, so forcing `ONLY_ACTIVE_ARCH` or
  turning off `COMPILER_INDEX_STORE_ENABLE` would save little.
