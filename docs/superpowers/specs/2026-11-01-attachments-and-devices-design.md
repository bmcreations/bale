# Attachments, and simulators that belong to a workspace

Status: draft for review. Replaces "Later: devices for mobile projects" in `2026-10-27-bale-design.md`.
The device measurements are iOS only; Android uses the same design, with its clone step still to be measured.

## Problem

A Flipcash checkout runs its tests on its own simulator. `Scripts/test.sh` in that repo makes
`Flipcash Tests <checkout dir>` with `xcrun simctl create … 'iPhone 17'` on first use, and
`Scripts/prune-orphan-deriveddata` deletes those devices once no worktree has that directory name. bale
already knows when a tree goes idle, gets baled, goes cold or is removed, but it never touches the device.
A tree removed without a hook leaves its device behind until that script runs: "Flipcash Tests
feat-tab-deeplinks" is still booted though its tree is gone. bale's records don't catch this either. The
sweep doesn't forget a record whose path is gone; only a manual `bale gc` does (`docs/architecture.md`), and
13 of the 14 Flipcash records marked `hot` on 2026-11-01 point at paths that no longer exist.

A test of sending and receiving needs two devices on one branch, and a change that crosses platforms may
need two per platform.

Measured on 2026-11-01 (`du -sk` of `~/Library/Developer/CoreSimulator/Devices`, 7 devices, 52.5 GiB):

| Device | GiB | Where it goes |
|---|---|---|
| Flipcash (daily, booted) | 21.28 | 12.54 in `PRBPosterExtensionDataStore`, iOS's wallpaper store |
| iPhone Duo | 11.70 | 6.15 in the same store |
| iOS18 Check | 7.29 | not broken down |
| iPhone 17 | 4.29 | not broken down |
| Flipcash Tests group-ready-bubble | 3.48 | 1.63 `var/MobileAsset`, 0.87 app bundle |
| Flipcash Tests feat-tab-deeplinks | 2.55 | booted; its tree is gone |
| Flipcash Tests code-ios-app-flaky-tests | 1.96 | |

A device made by `simctl create` and booted once is 0.39 GiB. The per-tree devices grow to 2 to 3.5 GiB
from the installed app and from MobileAsset downloads.

## What copy-on-write gives

Both measured in a separate device set (`xcrun simctl --set <temp dir>`), on the same APFS volume as the
DerivedData:

- `simctl clone` of a shut-down device changed free space by 0 MiB.
- `simctl install` of the 0.78 GiB `Flipcash.app` from DerivedData changed free space by 15 to 51 MiB over
  three installs, so the install clones the bundle rather than copying it.

The bundle stays shared only while the DerivedData copy exists. Once `up` deletes the build output or a
rebuild replaces the files, the simulator's copy owns its 0.78 GiB alone. Not measured: whether a clone of a
base that already holds MobileAsset keeps sharing it, or downloads its own copy on first boot.

## Design

Two parts with one interface between them. bale gets **attachments**: a tree can own resources outside its
directory, and bale releases them when the tree is baled, colded or removed. A new binary, **`bale-devices`**,
makes and releases simulators and emulators and registers them as attachments. bale never learns what a
device is, and `bale-devices` never reads bale's internals.

### Attachments, in bale

An attachment is recorded on the tree's record:

```
attachments: [
  { kind: "ios-sim", id: "<udid>", release: ["bale-devices", "release", "ios", "<udid>"], label: "sender" },
]
```

- `bale attach TARGET --kind K --id ID --release CMD… [--label L]` adds one. `kind` and `id` together are
  unique within a tree; attaching the same pair again replaces the entry.
- `bale detach TARGET --kind K --id ID` removes one without running its release.
- `bale show --json` (new) and `bale ls --json` list them, so a caller can find what a tree owns.
- The sweep's bale step, `bale cold`, removal through a tool's hook, and `bale gc` run each release command,
  under the tree's lock and after the busy check, as every delete path does.
- `bale up` run by hand keeps attachments unless given `--attachments`, so clearing build output doesn't
  also delete the devices a tree is testing on. The sweep passes the same option to the operation when it
  bales a tree. The README's `up` row documents the flag.
- The sweep gets a step: for each record whose path is gone, take its lock and run its releases. There is no
  busy check, since there is no directory to hold. The sweep still doesn't forget the record; it only clears
  attachments, so a gone tree's device is deleted on the next sweep rather than on the next manual `gc`.
- A release that exits 0 removes the entry. Any other exit keeps it, and the sweep retries it. A release
  has 30 s.
- `bale gc` releases a gone tree's attachments before it forgets the record, and keeps the record while any
  release still fails, so a device can't outlive the only record of it.
- Each release is a trace line: kind, outcome, duration, and the free space it gained.

The release command is an argument list, not a shell string, so an id with spaces can't run anything else.
bale runs it with `BALE_HOME` as the working directory, because a gone tree has no directory, and sets
`BALE_TREE_ID` and `BALE_TREE_PATH`.

bale-core names no tool here: kinds and commands are data it stores and runs.

### `bale-devices`, a separate binary

A workspace crate built into its own binary and shipped in the same release archive and Homebrew formula as
`bale`, so the two can't drift apart in version. Someone with no mobile work never runs it. It talks to bale
only through `bale attach`, `bale detach` and `bale show --json`.

**Making a device.** `bale-devices clone TREE --platform ios|android [--role NAME]` prints the device id
for that role, making it first if needed. The role defaults to `default`; a send/receive test asks for two
roles. It clones the device from a base, names it from the repo's pattern (below), and calls
`bale attach TREE --kind ios-sim --id <udid> --label <role> --release bale-devices release ios <udid>`.
A script can call it when `bale-devices` is on `PATH` and fall back to `simctl create` when it isn't. A
device from that fallback is attached to nothing, so nothing releases it until `adopt` runs.

**Base devices.** One per device type and runtime, shared by every tree and role, named
`bale base <type> <runtime>`, in the default device set so Xcode and loupe see it. `bale-devices` makes it
with `simctl create`, boots it once so first-boot data is written, and shuts it down. Clone needs the base
shut down; `bale-devices` never boots a base while making a clone.

**Adopting existing devices.** A repo config entry `device_name = "Flipcash Tests {dir} {role}"` lets
`bale-devices adopt` attach existing devices to the tree whose directory name fills `{dir}`. The sweep can't
call `bale-devices`, so adopting runs in two places: once by hand, for devices made before `bale-devices`
was installed, and inside `clone`, which attaches a device that already matches the tree's name and role
instead of making a new one. A name with no
role part, like today's `Flipcash Tests <dir>`, is adopted as role `default`, so `test.sh` keeps working
unchanged. An adopted device isn't a clone, so it doesn't share with a base.

**Releasing.** `bale-devices release ios <udid>` deletes the device, and exits non-zero without deleting
when:

- the device is booted. `bale-devices` never boots, shuts down or pauses a device;
- loupe is running and lists the device with a claim or a hands-off mark (`GET /api/targets`). Without
  loupe, the booted check is the only guard.

A device that's already gone exits 0, so the attachment clears.

**Limits.** A repo config `max_devices = 4` per tree, all platforms, stops a script in a loop from filling
the disk. `clone` refuses past it and names the devices the tree already has.

**Report.** `bale-devices report` lists every device by size with its largest folders, and marks the ones no
tree owns. It deletes nothing, so the wallpaper-store growth on a daily device shows up without anything
acting on it.

### Lifecycle

| Tree event | What happens to its devices |
|---|---|
| hot, stashed | Nothing. |
| baled | bale runs each release; the next `clone` makes the role again from the base. |
| cold or removed | Same. |
| tree gone | `bale gc` releases them before forgetting the record. |

Releases are independent: if one of a pair is booted, the other is deleted and the booted one is retried
next sweep. Device state isn't kept across baling; a fresh clone shares with the base where an erased device
doesn't.

## Expected effect on this machine

The per-tree devices hold 8 GiB today, one per tree. A tree with a sender and a receiver made the `test.sh`
way would hold twice that. Cloned, the second device should cost little more than the first: both clone the
same base, and both install the app from the same DerivedData, which also clones. The space left per device
would be what it writes after cloning, which isn't measured yet.

The one-off wins are outside this design and are the user's call: 18.7 GiB of wallpaper store on two daily
devices, and about 16 GB of iOS 18.0 and 18.1 runtimes that no device uses.

Booted devices also take memory, which this design doesn't manage. Several trees with two or four booted
devices each will hit memory and swap before disk.

## To measure before building

1. Boot a clone of a booted-once base, named `Flipcash Tests <dir>` so `test.sh` finds it, run
   `Scripts/test.sh` on it, and record free space and `du` before
   and after. That answers what a tree's device costs and whether MobileAsset is shared.
2. Clone two devices from the same base, install the app on both, boot both, and record free space, to check
   that a second role is close to free.
3. Check that `xcodebuild test -destination id=<clone>` and loupe work on a cloned device the same as on a
   created one.
4. For Android, check whether `cp -c -R` of an AVD folder, or a new AVD on the shared system image, boots,
   and what it costs once booted.
5. Check that deleting a shut-down device that loupe has open in a viewer doesn't wedge loupe or
   CoreSimulator.

## Decided

- Flipcash's `test.sh` calls `bale-devices clone` when it's on `PATH` and falls back to `simctl create`.
- Role names are free text.
- `max_devices` defaults to 4.
- bale-devices never deletes runtimes. The unused iOS 18.0 and 18.1 runtimes stay installed.

## Open questions

- Whether the daily devices should get a step that clears `PRBPosterExtensionDataStore` while shut down.
  Deleting that folder isn't tested, so this design only reports it.
