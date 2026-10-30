# Untracked Builds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** bale misses Xcode builds it was never told about, and they hold about 72 GiB on this machine as of 2026-10-29 20:30. There are two gaps:

1. **A worktree created with plain `git worktree add` builds before bale knows it.** A worker subagent ran `git worktree add … code-ios-app/.claude/worktrees/chat-pins-contract` at 21:28:54Z. It ran `cd <that tree>; …` at 21:28:59 and built with `./Scripts/test.sh` at 21:31. No WorktreeCreate hook fired, and the pre-tool hook's `cwd` stayed in the parent session's T3 tree (`t3-1ccedc7a`, registered). The build made its own 2.3 GiB `SourcePackages`, and sweep adopted the tree at 21:33, too late to link it.
2. **Build folders in Claude Code's scratch directories.** Agents pass `-derivedDataPath <scratchpad>/dd-*`, and Claude Code keeps scratchpads under `/private/tmp/claude-<uid>/<project>/<session>/scratchpad/`. bale only learns a `-derivedDataPath` root from a pre-tool hook, and only since #8. The 12 Xcode build folders there predate that, so bale knows none of them:

| Owner (`WorkspacePath` in `info.plist`) | Folders | Size |
|---|---|---|
| Worktree deleted (`group-invites-a/b/c`) | `dd-a-warn`, `dd-b`, `dd-c`, `dd-c-test`, `dd-dev` | 35.4 GiB |
| Registered tree, already baled | `dd-pin`, `dd-device`, `dd-main-sim`, `dd-ios` | 25.0 GiB |
| Registered tree, folder inside the tree | `wt-ios/.dd` | 11 GiB |
| Unregistered (loupe `t3-b82b5e57`) | `dd-device`, `dd-ios`, `dd-mac` | 0.2 GiB |

**Architecture:** No new deletion policy. Gap 1: the pre-tool hook reads the paths a command works in and registers an unknown linked worktree among them, so `register` pre-makes its DerivedData folder with the packages link before the first build. Gap 2: sweep discovers scratch build folders and learns them as `-derivedDataPath` roots. The existing rules then apply: reap removes a folder whose project is gone, after the 1 h grace. `up` removes a registered tree's build output after `idle_baled`, which defaults to 4 h. The next build at that path gets linked by the pre-tool hook's existing `-derivedDataPath` handling.

**Tech Stack:** Rust 2021, existing crates. No new dependencies.

## Global Constraints

- In a scratchpad, delete only folders that pass `is_derived_data_root` (an `info.plist` with a `WorkspacePath`, plus `Build/`) and that no process holds (`busy_holders`). Never delete a scratchpad or anything else in it. Sessions get resumed, and agents keep notes and patches there.
- Hooks stay best-effort: no exit code or output depends on this work. The new pre-tool checks run only when the command has an absolute path outside the cwd tree, and they cost one registry lookup per path after the first registration.
- `register`'s refusals still apply: a main checkout is never registered or linked.
- Find the scratch root from `getuid()`, not a literal `501`.
- Commits as Brandon McAnsh <git@bmcreations.dev>, no trailers.

## Decisions (defaults; flag to change)

- Scratch discovery is on by default and uses the same thresholds as everything else: 1 h grace for gone projects, `idle_baled` for registered trees.
- No separate idle rule for build folders whose owner is unregistered (0.2 GiB today). Revisit if that row grows.

## Review Focus

1. **Wrong tree for `-derivedDataPath`.** Today `share_packages` links a `-derivedDataPath` folder to the cwd tree's key. `cd <ios tree B>; xcodebuild -derivedDataPath /tmp/x` from inside tree A would link `/tmp/x` to A's packages, and SPM would then rewrite A's shared checkouts to B's pins. Task 1 resolves the tree from the command first.
2. **`up` deleting a folder an agent is building into.** `busy_holders` covers a running build. A folder between builds counts as idle once its tree passes `idle_baled`, which is how `up` already treats a tree's main DerivedData.

---

### Task 1: Pre-tool registers worktrees the command works in

**Files:** `crates/bale-hooks/src/claude.rs`, tests in the same file.

- [ ] Add `command_paths(command, cwd, user_home) -> Vec<(Kind, PathBuf)>`: the argument after `cd`, `git -C`, `-project`, and `-workspace`, parsed with `shell_word`. Skip arguments containing `$` or backticks, as `derived_data_paths` does. Also return `tool_input.file_path` for Read, Edit, and Write.
- [ ] For each path not under the cwd tree's root, walk up to the nearest directory with a `.git` entry. If it's a gitfile and `registry.get(workspace_id)` is `None`, call `register`. Then set `Origin { tool: "claude", session: payload.session_id, title: None }`, the way `t3.rs::adopt` does.
- [ ] Pick the tree for `share_packages`: a `-project`/`-workspace` path first, then `cd`/`git -C`, then cwd.
- [ ] Run this when cwd is in no tree too. Today `pre_tool` returns early there.
- [ ] Tests: the 21:28:59 command shape (`cd /abs/tree; grep …`) registers the tree and pre-makes its folder. A `$R`-style path is ignored. A main checkout is refused. `cd B; xcodebuild -derivedDataPath X` from inside A links X to B's key.

### Task 2: Sweep discovers scratch build folders

**Files:** `crates/bale-core/src/derived_data.rs`, `crates/bale-core/src/ops/sweep.rs`.

- [ ] `scratch_roots()`: folders at most two levels under `/private/tmp/claude-<uid>/*/*/scratchpad/` that pass `is_derived_data_root`. Use `/tmp` resolved, not hard-coded, and bound the walk with the sweep deadline pattern `adopt_missed` uses.
- [ ] Sweep calls `learn_root` for each one. Nothing else changes in reap; it already handles learned roots whose project is gone.
- [ ] Test: a fake scratch tree with one gone-project build folder is reaped after the grace, and a plain folder next to it survives.

### Task 3: `up` removes a tree's build folders outside Xcode's default root

**Files:** `crates/bale-core/src/ops/up.rs`.

- [ ] `remove_derived_data` also checks every learned root with `belongs_to_tree`, under the same `busy_holders` check.
- [ ] A folder inside the tree, at most two levels down, that passes `is_derived_data_root` (`wt-ios/.dd`) counts as build output even though git doesn't ignore it.
- [ ] Check whether sweep runs `up` again for a tree that is already baled. If it doesn't, the four baled trees in the table keep their 25 GiB until their next hot→baled cycle. Fix that, or record it as accepted.
- [ ] Tests: a learned root built from the tree is removed on `up` and one from another tree is kept. A busy one is kept and reported.

### Task 4: Live acceptance

- [ ] On a temp `BALE_HOME`, run sweep and `up` against copies of the table's cases and record the bytes freed per row.
- [ ] Replay the 21:28 command sequence in a fresh worktree and confirm `SourcePackages` is a link after the first build.
- [ ] Write results to `docs/bench/2026-10-untracked-builds.md`.
