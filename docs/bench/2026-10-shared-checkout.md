# Shared checkout: gate checks

Run on 2026-10-30 for `docs/superpowers/specs/2026-10-30-bale-shared-checkout-design.md`, before any bale
code. macOS 27.2, APFS, M3 Max. Sharing was done by a Python script that follows the spec's rules (verify
the blob hash, clone from the store with `cp -c` to a temp name, restore mode and nanosecond mtime, rename
over the file), not by bale. All trees were in a temp folder outside any registered repo. One run per case.

## Setup

A `git clone --shared` of the Android repo, with detached worktrees `w1`..`w4` at `c934c9138`.
`vendor/opencv` there is git tree `d80d225`: 536 files, 911 MiB by `du`. 103 of its files are 1 MiB or
larger and hold 890 MiB. Those 103 are what gets shared.

## Disk freed

| Run | Copies replaced | `df` free before -> after | Freed |
|---|---|---|---|
| `w1`, `w2`, empty store, right after a Gradle build | 206 (103 stored) | 82,507,272 -> 82,726,520 KiB | 214 MiB at once, 925 MiB a minute later |
| `w3`, `w4`, store already filled | 206 | 81,494,952 -> 83,316,048 KiB | 1,778 MiB |

The second run is the clean one. `df` was sampled every 10 s for 90 s on each side: it moved by less than
80 MiB before sharing, and stayed within 30 MiB of the new figure for the first 60 s after. 1,778 MiB is two
copies of 889 MiB. The first run's dip came during or just after the build that ended a minute earlier, so
it doesn't say whether APFS was late freeing the blocks or the build was still settling.

Sharing 206 files took 8.6 to 10.4 s, most of it `git hash-object` on each file.

## Git

After `git update-index --refresh`, `git status --porcelain` was empty in all four trees. It took 0.12 to
0.16 s the first time and 0.02 to 0.03 s the second.

## Gradle

`:apps:flipcash:app:assembleDebug` in `w2`. The scanner module's CMake build reads `vendor/opencv`.

| Build | Time | Tasks executed |
|---|---|---|
| Full, before sharing | 102 s | 1,106 (1,533 from cache) |
| No change, before sharing | 6 s | 30 |
| No change, after sharing | 17 s | 30, the same 30 |

The 30 tasks in both no-change builds are CMake configure and KMP dependency checks that run every time.
Of 467 files under the scanner's `.cxx` and `build` and the app's `build/outputs`, none had a different
mtime after the build that followed sharing. The extra 11 s wasn't investigated.

## Git LFS

No repo on this Mac uses LFS, so a temp repo held one 1 GiB random file tracked by LFS, with two worktrees.
1 GiB rather than 50 MiB so the change stands out from `df` noise. Before sharing there were three copies:
LFS's object and the two working files. The store copy was a clone of LFS's object, and both working files
were replaced by clones of it after their SHA-256 matched the pointer.

- `df` free rose by 2,062 MiB, two copies.
- `git status --porcelain` and `git lfs status --porcelain` were empty in both worktrees.
- Both files' SHA-256 still matched the pointer.

## Which repos gain

Tracked blobs of 1 MiB or more at `HEAD`, across trees on disk, counting every copy beyond the first:

| Repo | Trees | Shared blobs | Bytes freed |
|---|---|---|---|
| Android | 14 | 115 | 11.89 GiB |
| iOS | 13 | 7 | 1.23 GiB |
| bale, agent-team, code, loupe, loupe-private | 1 to 12 | 0 | 0 |

This assumes no copies share blocks today. The `w3`/`w4` run supports that for checkouts written by
`git worktree add`.
