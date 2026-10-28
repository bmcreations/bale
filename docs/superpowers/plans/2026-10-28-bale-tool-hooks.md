# bale tool hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every tool integration into a `bale-hooks` library crate over a tool-agnostic `bale-core`, add Conductor, Cursor and Codex adapters, and add `bale uninstall`, all in the one `bale` binary.

**Architecture:** `bale-core` gains three generic pieces (busy exclusion as a parameter, public adopt, new returning its path). `bale-hooks` holds one module per tool: parse its input, find its caller pids, edit its config. `bale-cli` parses arguments and wires the two.

**Tech Stack:** Rust 2021 workspace; clap, serde_json (preserve_order), toml_edit (new, for Conductor's file), anyhow.

**Spec:** `docs/superpowers/specs/2026-10-28-bale-multi-tool-hooks-design.md` (addendum) over `docs/superpowers/specs/2026-10-27-bale-design.md`.

## Global Constraints

- `bale-core` never names a tool. `scripts/check-core-agnostic.sh` greps `crates/bale-core/src` case-insensitively for `claude|conductor|cursor|codex`, ignoring everything from a file's `#[cfg(test)]` line onward, and fails on a hit. CI runs it.
- `bale-hooks` depends on `bale-core` public API only; no `pub(crate)` widening done just for it unless the item is a generic core feature.
- Hook commands that adopt (setup, post-checkout) always exit 0; errors go to stderr.
- Remove/archive hooks: exit 0 cold or already gone, 3 busy, 4 skipped, 1 error (same as `bale cold`).
- Old names `bale hook claude-worktree-create|remove` and `bale install --claude-hooks` are removed with no alias.
- Install edits keep existing commands; bale's runs after them for setup, before them for archive. Print a diff first; `--print-only` writes nothing.
- Every test that runs git or bale uses a temp HOME and BALE_HOME and `GIT_CONFIG_NOSYSTEM=1`. No test or probe touches the real `~`.
- Gate per task: `cargo test --workspace`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` on host, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.
- Commits: conventional, no attribution trailers.

## Review Focus

1. A remove hook run from Claude Code must not count Claude, its node process or the hook's shell as holders, but must still report a real shell left in the tree. Test in Task 1 and Task 3.
2. An install into a settings/config file that already has user hooks must keep them byte-for-byte apart from bale's addition, and a second install must be a no-op. Tests in Tasks 4–6.
3. A setup hook run outside a git worktree (main checkout, plain dir) must exit 0 and change nothing. Tests in Tasks 3, 5, 6, 7.
4. `bale uninstall` must remove only bale's own entries, leaving other hooks and the user's scripts. Test in Task 8.
5. Conductor archive on a busy tree must exit 3 and leave the tree; Conductor's own archive then decides. Test in Task 5.

---

### Task 1: Busy exclusion as a parameter

**Files:** Modify `crates/bale-core/src/busy.rs`, `crates/bale-core/src/ops/cold.rs`, `crates/bale-cli/src/main.rs`, `crates/bale-cli/src/hook.rs`. Tests: `crates/bale-core/tests/cold.rs`, `crates/bale-cli/tests/cli.rs`.

**Interfaces:**
- Produces: `bale_core::ops::cold::cold_ignoring(home: &BaleHome, id: &str, ignore_pids: &[u32]) -> Result<Outcome>`; `cold` calls it with `&[]`. `busy_holders(root, ignore: &[u32])` and `busy_holders_many(roots, ignore: &[u32])` filter those pids. `exclude_pids` and the `EXCLUDED` static are deleted.
- CLI: `bale cold <arg> [--ignore-pid <PID>]...`.

- [ ] Test `cold_ignoring_skips_listed_holder` (unix): spawn `sleep 30` with cwd in the worktree; `cold_ignoring(.., &[child.id()])` returns `Done(Level::Cold)`; plain `cold` on a fresh twin returns `Busy` listing that pid.
- [ ] CLI test `cold_ignore_pid_flag`: same via the binary, exit 0 with the flag, 3 without.
- [ ] Run, see both fail. Implement: thread `ignore` through every busy call in cold (pre-rename, post-move, recover's unmarked check) and in `up`/sweep pass `&[]`. Hook remove passes its `invoking_pids` to `cold_ignoring` instead of `exclude_pids`.
- [ ] Gate, commit `refactor: pass busy exclusions to cold instead of a global`.

### Task 2: Core adopt and new --print-path

**Files:** Modify `crates/bale-cli/src/main.rs`. Tests: `crates/bale-cli/tests/cli.rs`.

**Interfaces:**
- CLI `bale adopt [<PATH>]` (default cwd) calls `bale_core::ops::up::register`, prints `<id>: <level>`, exit 0; error exit 1.
- CLI `bale new <REPO> <NAME> [--branch B] --print-path` prints only the path (no trailing text besides `\n`).

- [ ] Tests: `adopt_registers_a_linked_worktree` (then `bale ls --json` holds the path); `adopt_refuses_main_checkout` (exit 1, nothing registered); `adopt_defaults_to_cwd`; `new_print_path_prints_only_the_path` (stdout trimmed equals canonical created path).
- [ ] Red, implement, green. Gate. Commit `feat: add bale adopt and new --print-path`.

### Task 3: Create bale-hooks; move the Claude and git adapters

**Files:** Create `crates/bale-hooks/{Cargo.toml,src/lib.rs,src/proc.rs,src/claude.rs,src/git.rs}`. Move code out of `crates/bale-cli/src/hook.rs` (delete the file). Modify root `Cargo.toml` members, `crates/bale-cli/{Cargo.toml,src/main.rs}`. Create `scripts/check-core-agnostic.sh`; add a CI step running it. Move hook tests from `crates/bale-cli/tests/cli.rs` and `hook.rs` unit tests into `crates/bale-hooks` (unit) or keep binary-level ones in `cli.rs` with new command names.

**Interfaces:**
- `bale_hooks::proc::caller_pids(own: u32, stop_names: &[&str], info: impl Fn(u32) -> Option<(u32, String)>) -> Vec<u32>`: walk parents from `own`, collect each pid, stop after the first whose name matches `stop_names` (case-sensitive; also match basename). Keep today's grandparent-only-if-shell rule when no stop name is found within 3 levels. `proc::process_info(pid)` moves here.
- `bale_hooks::Tool` enum `{Claude, Git, Conductor, Cursor, Codex}` with `fn stop_names(self) -> &'static [&'static str]`: claude `["claude","node"]`, conductor `["Conductor"]`, cursor `["Cursor","Cursor Helper"]`, codex `["codex","Codex"]`, git `[]`.
- `bale_hooks::claude::{create(home) -> Result<ExitCode>, remove(home) -> Result<ExitCode>}`; `bale_hooks::git::post_checkout(home, prev, new, flag) -> ExitCode`; `bale_hooks::remove_tree(home, path, tool) -> ExitCode` shared cold-with-caller-pids helper with the exit contract and the busy text (moved `busy_text`, `CHANGED_DURING_COLD`).
- CLI: `bale hook claude create|remove`, `bale hook git post-checkout <prev> <new> <flag>`. Claude create uses `new_workspace` and prints its path.

- [ ] Port existing hook tests to the new names first and see them fail (command not found). Add `caller_pids_stops_at_tool_name` unit test with a fake `info` table for each Tool.
- [ ] Move the code; `bale-cli` keeps only arg wiring. Add `scripts/check-core-agnostic.sh`; prove it by adding the word `claude` to a core non-test file, running it (fails), reverting.
- [ ] Gate plus the script. Commit `refactor: move Claude and git hooks into bale-hooks`.

### Task 4: Install editing moves to bale-hooks; `--hooks` flag

**Files:** Create `crates/bale-hooks/src/install.rs` (shared `Step`/`Plan`, `diff`, `write_atomic`, quoting helpers move here or stay in cli and are passed in; pick one home, no duplication). Move `claude_hooks_plan` to `claude.rs`, `git_hook_plan` to `git.rs`. Modify `crates/bale-cli/src/{install.rs,main.rs}`.

**Interfaces:**
- `bale_hooks::install_plan(tool: Tool, exe: &Path, home: &Path, repo: Option<&Path>) -> Result<Plan>`; errors "--repo is required for <tool>" for conductor/cursor without `repo`.
- CLI `bale install --hooks <LIST> [--repo <REPO>] [--print-only]`, list comma-separated from `claude,git,conductor,cursor,codex` (`git` needs `--repo`, replacing `--git-hook <REPO>`, which is removed). No `--hooks` installs the schedule as today.
- Claude settings hook commands become `<exe> hook claude create` / `remove`; the existing detection of bale's own entries (`bale_hook_path`) recognises both old and new forms so a reinstall replaces, never duplicates.

- [ ] Port install tests to new flags (red). Add `install_hooks_claude_keeps_user_hooks` (fixture settings.json with a user WorktreeCreate and an unrelated PreToolUse hook; after install both survive unchanged, bale's added; second install produces no diff) and `install_conductor_without_repo_errors`.
- [ ] Implement, gate, commit `refactor: install tool hooks through bale-hooks with --hooks`.

### Task 5: Conductor adapter

**Files:** Create `crates/bale-hooks/src/conductor.rs`; add `toml_edit` (exact pinned version) to `crates/bale-hooks/Cargo.toml`. Modify CLI wiring.

**Interfaces:**
- `bale hook conductor setup`: path from `CONDUCTOR_WORKSPACE_PATH` (fallback cwd); `register`; always exit 0.
- `bale hook conductor archive`: same path; `remove_tree(home, path, Tool::Conductor)`; unregistered path → `register` first, then cold.
- Install with `--repo R`: edit `R/.conductor/settings.toml` `[scripts]`: `setup` becomes `<existing> && <exe> hook conductor setup` (or just bale's if empty), `archive` becomes `<exe> hook conductor archive; <existing>`. Use `toml_edit` so comments and order survive. Note in output that the file is usually committed.

- [ ] Tests: `conductor_setup_adopts_from_env`; `conductor_setup_outside_worktree_exits_0`; `conductor_archive_colds_and_open_restores` (uncommitted file + ignored `.env`); `conductor_archive_busy_exits_3` (unix, holder `sleep` in tree); `install_conductor_keeps_existing_scripts_and_comments`; `install_conductor_twice_is_noop`.
- [ ] Red, implement, gate, commit `feat: add the Conductor adapter`.

### Task 6: Cursor adapter

**Files:** Create `crates/bale-hooks/src/cursor.rs`; CLI wiring.

**Interfaces:**
- `bale hook cursor setup`: path = cwd; `register`; exit 0.
- Install with `--repo R`: edit `R/.cursor/worktrees.json`; for key `setup-worktree` (array of command strings) append `<exe> hook cursor setup`. If the file uses `setup-worktree-unix`/`-windows` instead, append to the one matching the install OS and leave the other. Create the file with `{"setup-worktree": [...]}` if absent. Preserve key order (serde_json preserve_order). If a key holds a script path string rather than an array, refuse with a message naming the key.

- [ ] Tests: `cursor_setup_adopts_cwd`; `cursor_setup_outside_worktree_exits_0`; `install_cursor_creates_file`; `install_cursor_appends_to_existing_array`; `install_cursor_refuses_script_path_value`; twice is a no-op.
- [ ] Red, implement, gate, commit `feat: add the Cursor adapter`.

### Task 7: Codex adapter

**Files:** Create `crates/bale-hooks/src/codex.rs`; CLI wiring.

**Interfaces:**
- `bale hook codex setup [<PATH>]`: path arg or cwd; `register`; exit 0.
- Install `--hooks codex`: writes nothing; prints the command `<exe> hook codex setup` and one line saying to add it to the Codex environment's setup script, and that the file and variable names are unconfirmed.

- [ ] Tests: `codex_setup_adopts_path_arg`; `codex_setup_outside_worktree_exits_0`; `install_codex_writes_nothing` (temp HOME unchanged, stdout has the command).
- [ ] Red, implement, gate, commit `feat: add the Codex adapter`.

### Task 8: bale uninstall

**Files:** Modify `crates/bale-cli/src/{install.rs,main.rs}`, each `bale-hooks` tool module (an `uninstall_plan`).

**Interfaces:**
- `bale uninstall [--hooks <LIST>] [--repo <REPO>] [--print-only]`. No `--hooks`: remove the schedule (launchd `bootout` + plist delete; systemd `disable --now` + unit files delete; schtasks `/Delete /F`). With `--hooks`: remove only entries bale's install added (identified the same way install detects them); a file left empty of bale's entries keeps the user's content; a file bale created that is now empty is deleted. Git hook: remove only a hook carrying `GIT_HOOK_MARKER`.
- Saved state in `$BALE_HOME` and cold trees are untouched; output says how many records are cold and that `bale open` still works.

- [ ] Tests: `uninstall_claude_keeps_user_hooks`; `uninstall_conductor_restores_scripts`; `uninstall_cursor_removes_only_bale_entry`; `uninstall_git_hook_leaves_foreign_hook`; `uninstall_schedule_print_only_lists_steps` (assert the launchctl/systemctl/schtasks command for the host OS).
- [ ] Red, implement, gate, commit `feat: add bale uninstall`.

### Task 9: Docs

**Files:** Modify `README.md` (create if absent), `docs/superpowers/specs/2026-10-27-bale-design.md` (install sentence that says install sets up "the optional hooks" → points at the addendum and `--hooks`).

- [ ] README sections: install and uninstall; `bale hook` table per tool with what each does and its verified/unverified status (Claude, git verified by tests; Conductor, Cursor, Codex unverified until run against the tool); a short "For agents" section listing `bale new`, `bale open`, `bale cold`, `bale ls` with one line each, so an agent reading the repo or a CLAUDE.md pointer knows the commands.
- [ ] Commit `docs: document tool hooks, uninstall and agent commands`.
