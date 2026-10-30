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
80 MiB before sharing, and after sharing it stayed within 5 MiB for 40 s and within 45 MiB for 60 s. 1,778 MiB is two
copies of 889 MiB. The first run's dip came during or just after the build that ended a minute earlier, so
it doesn't say whether APFS was late freeing the blocks or the build was still settling.

Sharing 206 files took 8.6 to 10.4 s. That wasn't profiled; the script starts one `git hash-object` per file.

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
| No change, again | 3 s | 30 |

The 30 tasks in both no-change builds are CMake configure and KMP dependency checks that run every time.
Of 467 files under the scanner's `.cxx` and `build` and the app's `build/outputs`, none had a different
mtime after the build that followed sharing. The extra 11 s happened once: the next no-change build took
3 s.

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

13 of the 14 Android trees are baled, so the sweep could share them right away; the hot one waits until
`up` bales it. This assumes no copies share blocks today. The `w3`/`w4` run supports that for checkouts written by
`git worktree add`.

## After implementation

Run on 2026-10-30 with bale's own `bale share` and `bale new` (release build of `feat/shared-checkout` at
`939ff9e`), same Mac. The Android repo was at `c934c9138`, where 117 tracked files of 1 MiB or more hold
943 MiB. `BALE_HOME` and every tree sat on a 20 GiB sparse APFS disk image made for the run. A first
attempt on the Data volume itself was unreadable: `df` free moved by up to 2 GiB between 10 s samples while
nothing of this run was writing, most likely from other processes on the Mac. On the image `df` held
within 480 KiB over 30 s after each step. One run per case.

Trees `w1`..`w3` were `git worktree add --detach` checkouts of the repo, registered with `bale adopt`.

| Step | `df` free change | Time |
|---|---|---|
| `git worktree add` (each of `w1`..`w3`) | -1,002 MiB | 3.9 to 4.9 s |
| `bale share w1`, empty store | +6 MiB; 117 files shared, 115 stored | 7.3 s |
| `bale share w2`, store filled | +940 MiB | 7.4 s |
| `bale share w3`, store filled | +939 MiB | 6.6 s |

`w2` and `w3` together freed 1,879 MiB. The first share frees almost nothing: it clones each file into the
store and back, so the tree and the store share the same blocks. The store held 115 files, 933 MiB by `du`;
two of the 117 paths are duplicates of other blobs. `git status --porcelain` was empty in all three trees.

`bale new` creates a branch, and the repo's only branch at `HEAD` is checked out in its main checkout, so it
ran against a `git clone --shared --no-checkout` of the repo on the image instead, with the store above:

| Command | `df` free change | Time | First `git status` |
|---|---|---|---|
| `git worktree add --detach` | -1,002 MiB | 3.75 s | 0.20 s |
| `bale new` | -62 MiB | 4.42 s | 0.08 s |

`bale new` took 0.67 s longer here; where that time goes wasn't profiled. On the Data volume, in the
unreadable first attempt, the same pair took 5.35 s and 5.25 s.
