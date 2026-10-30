# Untracked builds: acceptance run

Run on 2026-10-30 with `target/release/bale` built from `feat/untracked-builds` (b745360). Xcode 27.1, macOS 27.2. Every bale command ran with `BALE_HOME`, `BALE_CLAUDE_TMP` and `HOME` pointing into one temp folder on the same APFS volume as the sources, so bale saw only the fake scratch tree and the temp trees below. The real `/private/tmp/claude-501` folders were only read (`cp -cRp`); their `info.plist` checksums were the same after the run. One run per case.

## Setup

- A temp git repo with an `App.xcodeproj` and linked worktrees `B`, `C`, `H`, `O` and `O/inner` (nested inside `O`), each registered with `bale adopt`.
- Build folders cloned with `cp -cRp` (APFS clones, mtimes kept) into `<BALE_CLAUDE_TMP>/claude-501/<project>/<session>/scratchpad/`. For the rows that need an owner, `WorkspacePath` in the clone's `info.plist` was rewritten with PlistBuddy to the temp tree's `App.xcodeproj`. For the rows with a gone owner or an unregistered one, the `info.plist` is the original.
- Sources: `dd-a-warn`, `dd-b`, `dd-c`, `dd-c-test`, `dd-dev` (owner worktrees deleted), `dd-pin` and `dd-main-sim` (code-ios-app session), `dd-ios` (wt-ios session), `dd-device`, `dd-ios`, `dd-mac` (loupe `t3-b82b5e57`), and `wt-ios/.dd`.
- Default config except where a row says otherwise (`idle_baled` 4 h, reap grace 1 h). Every cloned `Build/` is older than 1 h.

Sizes are `du -sk` of the clone, which counts shared blocks in full, so "freed" is what the folder reported, not bytes returned to the volume: a deleted clone releases only blocks no other file shares, and the source copies keep them.

## Cases

| Case | Folder (GiB) | After | Freed | Command | Result |
|---|---|---|---|---|---|
| Owner gone | `dd-a-warn` 6.64, `dd-b` 5.89, `dd-c` 6.13, `dd-c-test` 10.99, `dd-dev` 5.85 (35.49) | gone | 35.49 | `bale sweep` | Reaped, "its project is gone". Learned and reaped in the same sweep. A `--dry-run` first listed the same five and changed nothing. |
| Scratch dd of a registered tree (`B`), `dd-pin` | 4.20 | gone | 4.20 | `bale sweep` (B hot, 0 min old), then `bale up B` | Sweep kept it. `up` removed it and left B baled. |
| In-tree dd (`C/.dd`) | 11.19 | gone | 11.19 | `bale sweep`, then `bale up C` | Sweep kept it. `up` removed it. The folder is not git-ignored. |
| Nested registered tree (`O/inner`), scratch `dd-main-sim` | 5.96 | kept after `up O`; gone after `up O/inner` | 0, then 5.96 | `bale up O`, then `bale up O/inner` | `up O` baled the outer tree and left the inner tree's folder untouched. `up O/inner` removed it. |
| Project exists, tree active (`H`), `dd-ios` | 5.88 | 5.88 | 0 | `bale sweep` | Kept ("nothing to do"). |
| Owner exists but unregistered (loupe), `dd-device` 0.22, `dd-ios` 0.29, `dd-mac` 0.24 | 0.76 | 0.76 | 0 | `bale sweep` | Kept, as the plan decides. These three clones measure 0.76 GiB by `du`, not the 0.2 GiB in the plan's table. |
| Folder created after the tree was baled (`B`, `dd-late` from `dd-mac`) | 0.24 | gone on the second sweep | 0.24 | `idle_baled_hours = 0`, `bale sweep` twice | First sweep: B kept, folder still there. Second sweep: "up baled" and the folder was gone. |

In the last row, the first sweep decides on each tree before it discovers and learns the new scratch folder, so the tree is baled again one sweep (5 minutes on macOS) after the folder appears. This answers Task 3's question: sweep does run `up` again on an already baled tree, one sweep late for a folder it has not met yet. A folder already learned is picked up in the same sweep.

Nothing was removed that the plan says to keep, and nothing the plan says to remove survived.

## Replay of the 21:28 sequence

Tree `A` (registered, cwd) and a new worktree `B` were made from the iOS fixture (`t0/ios`, Flipcash, `Code.xcodeproj`), `A` with `git -C <fixture> worktree add --detach` and `bale adopt`, `B` with the same `git worktree add` and no `adopt`. The build script `build.sh` stands in for `./Scripts/test.sh`: it runs `xcodebuild -project Code.xcodeproj -scheme Flipcash -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO -IDECustomDerivedDataLocation=<temp HOME>/Library/Developer/Xcode/DerivedData "$@"`. The default DerivedData location is redirected with that flag because xcodebuild ignores `HOME` for it. Machine load average was 3 to 10.

1. `A` got `xcodebuild -resolvePackageDependencies` (152 s), so a sibling had a finished `SourcePackages`.
2. `bale ls` did not list `B`.
3. The hook was fed `{"session_id":"replay","cwd":"<A>","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd <B>; <build.sh> build"}}` on stdin: `bale hook claude pre-tool`. Exit 0, no output.
4. `bale ls` listed `B` as hot. Its DerivedData folder (`Code-fnblqfuvogweqxcocgcmtfoboulw`) existed before any build and held `SourcePackages -> $BALE_HOME/spm/7e5f23faeaa13f27`, seeded from `A`'s resolve.
5. `build.sh build` in `B`: exit 0, 179 s, `** BUILD SUCCEEDED **`, 6216 `SwiftCompile` lines.

After the build `SourcePackages` was still that symlink. The log has 0 lines matching `could not resolve` or `failed to resolve`. The one `error:` match is a CoreSimulator log line from `xcodebuild`, not a build error. `du`: `B`'s folder 7.02 GiB, `A`'s resolve-only folder 2.26 GiB, shared `spm/<key>` 2.26 GiB.

An earlier try in this run, with no DerivedData folder yet and no sibling that had resolved, registered `B` but made no link and no `spm/` folder. The link needs the DerivedData directory to exist and a finished sibling checkout of the same `Package.resolved`; in the 21:28 case the parent session's tree is that sibling.

## Cleanup

The temp folder (clones, worktrees `B`, `C`, `H`, `O`, `O/inner`, the bale home and the temp DerivedData) was deleted. The fixture's worktrees `A` and `B` were removed with `git worktree remove`, and `git worktree list` for the fixture shows its two original entries again. `xcodebuild` still used the real SwiftPM caches under `~/Library` for fetching; nothing was done to them.
