# Shared Swift Packages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every iOS tree on the machine holds its own copy of the same Swift packages. On Flipcash that is 2.3 GiB of the 11.1 GiB in a tree's DerivedData, and 8 trees held 18.5 GiB of copies on 2026-10-29. Make trees with the same `Package.resolved` share one copy, so an active tree costs 2.3 GiB less than it does now.

**Architecture:** bale keeps one packages folder per key under `$BALE_HOME/spm/<key>`, where the key is the hash of the tree's `Package.resolved` plus the Xcode build version. Before a tree's first build, bale creates the tree's DerivedData folder itself, under the name Xcode would give it, and puts a `SourcePackages` symlink in it pointing at the shared folder. A shared folder is only ever created as an APFS clone of a packages folder that has already finished resolving. Two trees resolving into the same empty folder at once fail (Task 0), and a clone costs no space until one side changes. When a tree is registered and its key has no shared folder, bale seeds one from any other checkout of the same repo whose packages folder is finished for that key, usually the main checkout. Cloning is instant, so this fits in the WorktreeCreate hook. That covers agents creating several trees within one sweep interval. A tree whose key has no finished folder anywhere builds normally, and the sweep seeds the shared folder from it afterwards. The pre-tool hook relinks a tree whose `Package.resolved` changed, and links `-derivedDataPath` folders before the build that creates them.

**Tech Stack:** Rust 2021, existing `bale-core` / `bale-hooks` / `bale-cli` crates, `md-5` (new), `sha2`, `serde_json`, `plist` (already used by `derived_data`), `libc::clonefile` on macOS.

**Scope:** Xcode only. Gradle and Cargo already keep dependencies in one place per machine (`~/.gradle/caches`, `~/.cargo/registry`).

## Global Constraints

- Never touch an existing tree's packages folder. Turning a real `SourcePackages` folder into a symlink recompiled 4,160 Swift files (315 s at load 80) on the next build (Task 0). bale only links a tree when its DerivedData folder doesn't exist yet: a new tree, an adopted tree with no build, or a tree whose build output `up` just removed.
- Never let Xcode resolve into an empty shared folder. Create a shared folder only by cloning a folder that finished resolving, into a temp name, then renaming it into place.
- Nothing bale does to a tree may follow the `SourcePackages` link. `up`, reap, `size_tree` and stash already don't (checked 2026-10-29: `remove_entry` removes the link itself, and every `WalkDir` uses `follow_links(false)`). Keep it that way and test it.
- A failure to link leaves the tree building normally. Hooks never change their exit code or output because of this feature.
- macOS only, behind `#[cfg(target_os = "macos")]` like the rest of the DerivedData code. Config key `share_swift_packages` in the machine `config.toml`, default `true`.
- Commits as Brandon McAnsh <git@bmcreations.dev>, no trailers.

## Review Focus

1. **Divergence.** An agent in tree B bumps a package. If B still pointed at A's folder, Xcode in B would rewrite the shared checkouts and break A. The pre-tool hook must relink B to the new key before the build starts (Task 6). Agents that build without a pre-tool hook (Codex in T3) can still do this. Task 1 step 5 tested it: tree A did not fail or recompile, it re-checked out its own revision into the shared folder (55 s, 0 compiles), and B did the same on its next build. No guard is needed, but trees without the hook flip the shared checkout back and forth until someone relinks them.
2. **Two trees moving to a new key at once.** Both clone the old folder and race on the rename. Exactly one rename must win and the other tree must use the winner (Task 4).
3. **Wrong folder name.** If bale's name differs from Xcode's, Xcode makes its own folder and the tree silently gets no sharing. This happened in the first Task 0 run: bale hashed `/private/tmp/...` and Xcode hashed `/tmp/...`. Test the name against real folders (Task 3).
4. **GC removing a folder a tree still links to** (Task 7).

---

### Task 0: Check that Xcode accepts a pre-made folder and a shared folder (blocks Task 1)

**Done 2026-10-29** on the Flipcash fixture (`scheme Flipcash`, iOS Simulator, `CODE_SIGNING_ALLOWED=NO`). Kept as a record of the method.

| Run | What | Result |
|---|---|---|
| A | Tree with a built `.dd`, its `SourcePackages` replaced by a symlink to a copy | First build: 4,160 Swift compiles, 196 links, 315 s. Next build: 0 compiles. |
| B | A rebuilds while a second tree builds from scratch, both linked to A's populated folder | Both succeed. Concurrent no-op builds after: 0 compiles each. |
| C | Two new trees, global DerivedData, pre-made `Code-<hash>/` holding only a `SourcePackages` symlink to one empty folder, built concurrently | Xcode kept both pre-made folders and both links, and wrote `info.plist` into them. One build failed: `Could not resolve package dependencies: Couldn't update repository submodules`. The other succeeded and filled the shared folder (2.3 GiB: `artifacts`, `checkouts`, `repositories`, `workspace-state.json`). The failed tree's next build succeeded against the filled folder. |
| D | Same as C, folder name hashed from `/private/tmp/...` | Xcode named the folder from `/tmp/...` and made its own. The pre-made folder went unused. |
| E | New tree, `.dd/SourcePackages` linked to a `cp -c -R` clone of a finished folder (11.7 s to clone 2.3 GiB, 50,020 files) | Normal first build: 6,216 compiles, 212 links, 218 s at load 36-64, same counts as the earlier full builds. No `Creating working copy` or `Checking out` lines. The log has 49 `Updating from <url>` lines, one per pin, and so does every build against an existing folder. The clone's file mtimes matched the source to the nanosecond (all but 191 of 50,020 files, which the builds had touched). |
| F | Tree E (built, 0-compile no-op in 33 s) relinked to a second clone `cp -c -R` of its folder, build again | 4,160 compiles, 196 links, 137 s at load 40; the next build is 0 compiles in 21 s. Same 4,160 as run A although mtimes were preserved, so Xcode keys on the link target (or the checkout inodes), not on timestamps. |
| G | Path with a space in a directory, a space in the project stem, a non-ASCII stem, and a build through an `.xcworkspace`, global DerivedData, minimal generated projects | Hash matched in all four cases using the rule below. Folder names: `Demo-dmnm...` (space in directory), `Demo_App-cmbs...`, `a-b.c_e\u0301+d(1)-ezla...`, `Outer-atnn...` (workspace stem and workspace path). |
| H | Trees A and B share folder X, both built. B's `Package.resolved` moves `swift-http-types` 1.5.1 to 1.5.0 (revision from the shared `repositories/`). B builds (full build, 205 s), then A builds without relinking | A succeeds. It logs `Creating working copy` and `Checking out 1.5.1 of package 'swift-http-types'`, which rewrites the shared checkout back to A's pin, then 0 compiles in 55 s (21 s for an unchanged no-op). B's next build checks out 1.5.0 again, 0 compiles, 29 s. A and B built at the same moment: both succeed (24 s and 66 s, 0 compiles). |

The folder name is `<project stem>-<a><b>`, where `a` and `b` are the first and last 8 bytes of `md5(path)` read as big-endian integers and written as 14 base-26 letters (`a`–`z`), most significant first. `path` is the `.xcodeproj` (or `.xcworkspace`) path with a leading `/private` removed. This matched all 3 DerivedData folders present on 2026-10-29 and both pre-made folders in run C. Run G adds two rules: Xcode hashes the path in Unicode NFD form even when the directory on disk is NFC (a `caf\u00e9.xcodeproj` created in NFC hashed as NFD), and it replaces spaces in the project stem with `_` in the folder name (the hash still uses the real path with the space). Other punctuation in the stem (`-`, `.`, `+`, `(`, `)`, non-ASCII letters) is kept, but only these characters were tried.

Shape of the two files (Task 4's `finished` uses them): `Package.resolved` is version 3 with `pins[].identity` and `pins[].state.revision` (49 pins; `version` is absent on `bugsnag-cocoa` and `sqlite.swift`). `workspace-state.json` is version 7 with `object.dependencies[].packageRef.identity` and `.state.checkoutState.revision` (50 entries). The extra entry is a local package, with `packageRef.kind` `fileSystem` and no `checkoutState`. All 49 remote entries matched the pins by identity and revision. A resolve interrupted after 40 s listed 9 of the 49 pins (10 checkouts, 49 repositories, 974 MB), so a count or revision check tells it apart from a finished folder.

Conclusions the tasks below already use: pre-made folders work, a populated shared folder works under concurrent builds, an empty one doesn't, and converting an existing tree costs a full rebuild. Task 1 (runs E-H) added: a clone seeds a folder with no re-resolve, every relink costs the same 4,160 compiles (about 2 minutes at load 40), and a tree that builds against a folder whose pins moved under it re-resolves without failing.

### Task 1: Check the remaining unknowns (blocks Task 2)

**Done 2026-10-29.** Results are runs E-H in the Task 0 table. Effects on later tasks: Task 3 (stem space, NFD), Task 4 (`finished` ignores local packages), Task 6 (no sweep needed for correctness). Raw logs and numbers are in `.superpowers/sdd/2026-10-29-bale-shared-swift-packages/task-1-report.md`.

Run each on the fixture, never in the user's trees, and delete any global DerivedData folder a step creates. Record results in this plan's Task 0 table and edit the affected tasks.

- [x] **Step 1: Seed by clone.** Clone (`cp -c -R`) a finished `SourcePackages` into a new folder, link a new tree to it, build. Expect no package fetch in the log and a normal first build. If Xcode re-resolves, record how long it takes.
- [x] **Step 2: Completeness.** Compare the pins in `workspace-state.json` (`object.dependencies[].packageRef.identity` and `state.checkoutState.revision`, schema unverified) with the tree's `Package.resolved` (`pins[].identity`, `pins[].state.revision`). Confirm bale can tell a finished folder for this `Package.resolved` from a half-resolved one. If the schema differs, write down the fields that work.
- [x] **Step 3: Relink cost.** Build a tree linked to folder X. Clone X to Y, point the link at Y, build again. Record compiles. A full rebuild means Xcode keys on the link target, and every relink costs about 5 minutes. That is acceptable only because relinks follow a `Package.resolved` change. Also check with `stat -f %m` on a few checkout files that the clone kept mtimes. Run A's copy may not have, and then its 4,160 compiles came from timestamps, not the link. Global Constraint 1 stays either way.
- [x] **Step 4: Name edge cases.** Make a fixture whose path has a space and one opened through an `.xcworkspace`, build each once into global DerivedData, and check the folder names against Task 3's function.
- [x] **Step 5: Unhooked divergence.** Two trees share folder X. In tree B, change one pin in `Package.resolved` and build without relinking. Then build tree A. Record whether A fails, re-resolves, or rebuilds, and how long it takes. If A breaks, Task 6 must also cover the sweep (relink idle trees whose pins changed), and the plan should say which agents stay exposed.
- [x] **Step 6:** Commit the results with the plan edits: `docs: record shared packages checks`.

### Task 2: Key and `Package.resolved` lookup

**Files:** Create `crates/bale-core/src/spm.rs`. Modify `lib.rs`.

- [ ] **Step 1: Failing tests** for `resolved_files(tree) -> Vec<PathBuf>`: finds `*.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved` and `*.xcworkspace/xcshareddata/swiftpm/Package.resolved` up to 3 levels deep, skipping `detect::regenerable_dirs` and `.git`. A tree with two projects (loupe's `apple/Mac/LoupeMac.xcodeproj` plus another) returns both, each paired with its project path.
- [ ] **Step 2: Failing tests** for `key(resolved, xcode_build) -> String`: SHA-256 of the file bytes and the build version, hex, first 16 characters. The same file with two Xcode versions gives two keys, and a whitespace change gives a new key. Pin-level normalisation isn't worth it.
- [ ] **Step 3:** `xcode_build()` reads `ProductBuildVersion` from `<developer dir>/../version.plist`, where the developer dir is `$DEVELOPER_DIR` if set, else `xcode-select -p` (`27A9275` on 2026-10-29) without running `xcodebuild`. Cache it per process. If it can't be read, the feature is off for that call.
- [ ] **Step 4:** Implement, run `cargo test -p bale-core spm`, commit `feat(spm): key trees by Package.resolved and Xcode version`.

### Task 3: DerivedData folder name

**Files:** Modify `crates/bale-core/src/derived_data.rs`.

- [ ] **Step 1: Failing tests** for `folder_name(project: &Path) -> String` using the folder names recorded in Task 0 as fixed vectors (`/tmp/.../t0/ios/Code.xcodeproj` gives `Code-akujzjaeriyzdfecyhmsqsaxboch`), plus the `/private` stripping, plus Task 1 step 4's cases: a project in a directory with a space (`Demo-dmnmyroyujtqlrdmydpdvhpiidhk` for `<scratch>/t12/gen/with space dir/Demo.xcodeproj`, `<scratch>` being the `/tmp/claude-501/...` fixture directory), a stem with a space (`Demo App.xcodeproj` gives `Demo_App-...`), a non-ASCII stem (hash the NFD form of the path), and an `.xcworkspace` (stem and hash come from the workspace path).
- [ ] **Step 2:** Implement with `md-5`. Strip a leading `/private` only when the rest starts with `/tmp/`, `/var/` or `/etc/`. NFD-normalise the path before hashing (needs the `unicode-normalization` crate) and replace spaces in the stem with `_` in the folder name only. Run the tests and commit `feat(derived-data): compute Xcode's DerivedData folder name`.
- [ ] **Step 3: Live check (macOS only, ignored by default):** a test that walks the real `~/Library/Developer/Xcode/DerivedData`, reads each `WorkspacePath`, and asserts the computed name matches. Run it once by hand and record the count in the PR.

### Task 4: Shared folder store

**Files:** Extend `spm.rs`. `BaleHome::spm_dir()` returns `$BALE_HOME/spm`.

- [ ] **Step 1: Failing tests:**
  - `finished(dir, resolved)` holds only when every pin in `resolved` (`pins[].identity`, `pins[].state.revision`) appears in `dir/workspace-state.json` as `object.dependencies[].packageRef.identity` with the same `state.checkoutState.revision` (checked on the fixture, Task 1 step 2). Entries in the state file that are not in `resolved` are ignored: local packages show up there with `packageRef.kind` `fileSystem`. A `Package.resolved` that is not version 3 or a state file that is not version 7 counts as not finished. Test with a half-resolved fixture (9 of 49 pins) and with one pin at another revision.
  - `seed(home, key, from)` clones `from` to `spm/.tmp-<key>-<pid>` and renames it to `spm/<key>`. If `spm/<key>` already exists, it drops its temp copy and returns the existing one.
  - Two threads seeding the same key both get the same path, and exactly one clone survives.
  - `from` must not be a symlink.
- [ ] **Step 2:** Clone with `libc::clonefile` (whole tree in one call; `cp -c -R` took 11.7 s for 2.3 GiB and kept file mtimes to the nanosecond; the 4,160-compile relink cost does not come from mtimes). Never fall back to a byte copy: a 2.3 GiB copy would erase the saving, so on failure return an error and leave the tree unlinked.
- [ ] **Step 3:** Run the tests and commit `feat(spm): seed shared packages folders by clone`.

### Task 5: Link before the first build

**Files:** Modify `ops/up.rs` (`register`, and the end of `remove_derived_data`) and `ops/sweep.rs` (adopted trees and seeding).

- [ ] **Step 1: Failing tests (temp HOME, fake DerivedData root):**
  - Registering a tree whose key has a shared folder creates `<dd_root>/<folder_name>/SourcePackages`, a symlink to `spm/<key>`, and nothing else.
  - The same tree with an existing `<folder_name>` is left alone.
  - A key without a shared folder creates nothing.
  - After `up` removes a tree's DerivedData, the link is created.
  - `up` on a linked tree removes the link and leaves `spm/<key>` intact.
  - Registering a tree whose key has no shared folder, while the repo's main checkout has a `finished` real folder for that key, seeds `spm/<key>` from it and links the new tree. The main checkout's folder is untouched.
  - A pre-made folder that never got an `info.plist` (the tree was removed before its first build) is removed by `up` and by tree removal. Existing code can't match it because it matches on `WorkspacePath`, so match it by `folder_name(project)` for each project `resolved_files` returns.
- [ ] **Step 2: Sweep seeding test:** a tree whose own (real) `SourcePackages` is `finished` and whose key has no shared folder gets seeded from. The tree itself keeps its real folder (Global Constraint 1).
- [ ] **Step 3:** Implement. Linking runs in `register` (covers the WorktreeCreate hook, `bale up`, and adoption) and right after `remove_derived_data` succeeds for a project. When the key has no shared folder, `register` first tries to seed from the other worktrees of the same repo (`git worktree list`, real `SourcePackages` only, `finished` for this key). The record gains `spm: Vec<SpmLink>` (project path, `Package.resolved` path with its mtime and size, key, pre-made folder path), which Task 6 compares against. Seeding runs in the sweep over every registered tree and the main checkouts of known repos, so the first tree of a new key seeds it.
- [ ] **Step 4:** Run `cargo test --workspace` and commit `feat(spm): link new trees to shared Swift packages`.

### Task 6: Pre-tool hook: relink and `-derivedDataPath`

**Files:** Modify `crates/bale-hooks/src/claude.rs` (`pre_tool`, next to `learn_dd_roots`).

- [ ] **Step 1: Failing tests:**
  - Tree linked to key K1 whose `Package.resolved` now hashes to K2: if `spm/K2` exists, the link is repointed. Otherwise it is repointed to a clone of K1 seeded as K2, so SPM updates the changed pins rather than fetching everything.
  - Unchanged key: no filesystem writes beyond the existing ones.
  - Bash command with `-derivedDataPath X`, where `X` has no `SourcePackages` and the tree's key has a shared folder: `X/SourcePackages` is created as a link before the command runs. An existing `X/SourcePackages` is untouched.
- [ ] **Step 2:** The relink is worth doing but not urgent for correctness: Task 1 step 5 showed an unrelinked tree builds fine after another tree changed a pin. It re-checks out its own revision (about 30 s extra, 0 compiles), and the shared checkout flips back and forth. So the sweep does not need to relink idle trees; agents without the pre-tool hook stay on the shared folder until their next relink, and a relink costs 4,160 compiles. Concurrent builds with different pins were tested only while both were no-ops. Implement with a cheap path for the common case: compare the file's mtime and size against the record before hashing. Measure the added hook time on an unchanged tree and keep it under 5 ms.
- [ ] **Step 3:** Run the tests and commit `feat(hooks): relink shared packages when Package.resolved changes`.

### Task 7: GC unreferenced shared folders

**Files:** Extend `ops/reap.rs` or add `ops/spm_gc.rs`, called from the sweep.

- [ ] **Step 1: Failing tests:** a shared folder referenced by a link in global DerivedData, a learned root, or a tree-local `.dd` survives. An unreferenced folder older than `reap_grace` is removed. A newer one survives. Leftover `.tmp-*` folders older than the grace period are removed. A pre-made folder with no `info.plist` whose project is gone is removed. A dangling link left in a DerivedData folder is not followed.
- [ ] **Step 2:** Implement with `remove_entry` and the `busy_holders` check reap already uses. Write an `op: "spm_gc"` trace line with bytes freed.
- [ ] **Step 3:** Run the tests and commit `feat(spm): remove shared packages folders nothing links to`.

### Task 8: Acceptance run

- [ ] **Step 1:** On the fixture, with a finished build in the fixture's main checkout, create three new worktrees through the same code path the WorktreeCreate hook uses, start their builds at the same moment, then run no-op builds. Expect: one shared folder, three links, no resolve failure, 0 compiles on the no-ops, and `du` showing about 2.3 GiB less per extra tree than three unlinked trees.
- [ ] **Step 2:** Bump one pin in one tree, run a build through the pre-tool hook path, and confirm it moved to a new key while the other two still build with 0 compiles.
- [ ] **Step 3:** Write the numbers to `docs/bench/2026-10-shared-packages.md` and commit `docs: record shared packages acceptance run`.
