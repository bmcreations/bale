# Per-tree simulators: clone from a base vs create

Measured 2026-11-01 on the Mac this repo is developed on (APFS, one volume for DerivedData and
CoreSimulator). Flipcash iOS at `origin/main` `a97732364`, iPhone 17 on iOS 27.0, Xcode's default
DerivedData. One run of each case. Load average was 3 at the start and 95 by the end, and other agents were
writing to the disk. During the build, free space fell about 21 GiB against about 12 GiB this run can
account for, and another 3.5 GiB before the device steps began. Between the device steps it moved by
under 0.01 GiB, so the device deltas below are usable; the build-phase numbers are not.

## Method

1. Created `bale-bench base`, booted it, built Flipcash for testing against it (8 min 55 s), let it idle
   2 min more, and shut it down. It was booted for about 11 min in total.
2. Cloned the base as `Flipcash Tests devbench` and ran `Scripts/test.sh FlipcashTests/FiatTests` from a
   worktree of that name, so `test.sh` found the clone by name.
3. Control: created a device with `simctl create` and ran the same suite with `test-without-building`.
   The clone's run went through `xcodebuild test`, which also writes a result bundle and build checks to
   DerivedData, so its free-space figure includes some writes the control's doesn't.
4. Cloned the base a second time, booted it and installed `Flipcash.app` from DerivedData.
5. Claimed the first clone in loupe, booted it, took a screenshot, shut it down, released the claim and
   deleted the device with `simctl delete`.

`du` counts cloned blocks in full, so free space (`df`) is the cost; `du` shows what the device holds.

## Results

| Device | `du` | Free space used |
|---|---|---|
| Base, booted once | 2.2 GiB | (not isolated from the build) |
| Clone, right after `simctl clone` | 2.1 GiB | 0 |
| Clone after `test.sh` (boot, install, 7 tests) | 3.0 GiB | 0.16 GiB |
| Created device after the same tests | 1.4 GiB | 0.54 GiB |
| Second clone, booted, runner then `Flipcash.app` installed | 3.2 GiB | 1.33 GiB, then 0.04 GiB for `Flipcash.app` |

- **The base's MobileAsset is shared.** The clone held 1.3 GiB of `private/var/MobileAsset` from the base
  at no cost. The created device, booted for about 2 min, had 73 MiB of it and would download the rest while booted.
  The older `Flipcash Tests` devices hold about 1.6 GiB each, which suggests a created device pays that
  in full once it has been booted long enough.
- **The app install clones from DerivedData.** `Containers/Bundle` was 901 MiB on both devices, and the
  install on the second clone cost 0.04 GiB of free space.
- **The second role's cost isn't settled.** Clone, boot
  and a first install (of `FlipcashUITests-Runner.app`, by mistake) used 1.33 GiB of free space at load 95,
  then installing `Flipcash.app` used 0.04 GiB. The runner install, the boot's own writes and other
  processes aren't separated in that 1.33 GiB. Today's `test.sh` devices hold 2–3.5 GiB each.
- **`test.sh` and `-destination id=` work on a clone** the same as on a created device: both runs passed
  7 tests (`test.sh` took 60 s, the control 49 s without the build step).
- **Loupe works on a clone:** claim, boot, screenshot, shutdown and release all returned 2xx. The
  screenshot showed the boot logo, since loupe's boot returned before the clone finished booting at
  load 95.
- **Deleting a shut-down clone after loupe used it** left loupe answering and CoreSimulator listing
  devices. A device open in a loupe viewer wasn't tested.

## Not measured

- Android (spec step 4). Load reached 95 during the iOS runs, so booting AVDs was left for another run.
- How much a clone grows over days of use. These numbers cover one test run.
- Whether the base needs refreshing after a runtime update.
