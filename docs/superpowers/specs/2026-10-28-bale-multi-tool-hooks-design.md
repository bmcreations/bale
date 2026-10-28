# bale: tool integrations as a layer over an agnostic core

Addendum to `2026-10-27-bale-design.md`. Two changes: every tool integration, Claude Code's included, moves
into a `bale-hooks` crate layered over `bale-core`, and that layer gains adapters for Conductor, Cursor and
Codex. It all still ships as one `bale` binary.

## Problem

`bale-core` is already tool-agnostic: it knows git and the filesystem, nothing about any agent tool. The
tool knowledge sits in `bale-cli`, mixed in with the core's own commands: `hook.rs` parses Claude Code's
payload and knows its process names (`claude`, `node`), and `install.rs` edits `~/.claude/settings.json`
next to the scheduler setup. Adding three more tools the same way spreads four tools' formats through the
CLI crate.

Organizationally the integrations are a layer over an agnostic core. They can still ship in the same
binary and be set up by `bale install`.

## What each tool offers

Researched 2026-10-28 from each tool's documentation. "Unconfirmed" means the source was third-party or
missing.

| Tool | After create | Before remove | Worktree path |
|---|---|---|---|
| Conductor | `scripts.setup` in `.conductor/settings.toml` (or app Settings > Scripts) | `scripts.archive`, runs before archive | `~/conductor/workspaces/<repo>/<workspace>` |
| Cursor | `setup-worktree` / `-unix` / `-windows` in `.cursor/worktrees.json`, looked up in the worktree, then the project root | none; Cursor deletes by `cursor.worktreeMaxCount` (default 25) and on an interval | `~/.cursor/worktrees` (forum report) |
| Codex app | per-environment setup script; file and variable names unconfirmed | none; the app keeps 15 worktrees and snapshots before deleting | `$CODEX_HOME/worktrees`, detached HEAD |

Sources: conductor.build/docs/reference/scripts, cursor.com/docs/configuration/worktrees,
learn.chatgpt.com/docs/environments/git-worktrees.

Conductor passes `CONDUCTOR_WORKSPACE_PATH` and `CONDUCTOR_ROOT_PATH`. Cursor documents
`ROOT_WORKTREE_PATH`. None of the three lets a command replace worktree creation.

## Design

### The boundary

Three crates, one binary:

| Crate | Holds | May depend on |
|---|---|---|
| `bale-core` | registry, up, cold, open, sweep, migrate, new, gc, busy detection | nothing tool-specific |
| `bale-hooks` (new, library) | one module per tool: parse its input, compute its caller pids, edit its config | `bale-core` public API |
| `bale-cli` (binary `bale`) | argument parsing and output; wires core commands and hook adapters | both |

`bale-core` never names a tool. A grep for tool names in `crates/bale-core/src` (outside tests) is part of
the gate. `bale-hooks` uses only `bale-core`'s public API, so anything an adapter needs is a core feature
any caller could use.

### What the core exposes

Generic pieces the adapters need, usable by any caller:

- **`register`** is public as `bale adopt [<path>]`, with its admin-dir checks. Default path is the cwd.
- **Busy exclusion as a parameter.** The global `busy::exclude_pids` becomes an `ignore_pids` argument on
  cold (CLI: `bale cold --ignore-pid <pid>`). Which pids to ignore is the caller's business.
- **`new` returns the path** (CLI: `bale new --print-path`), for callers that must print only the path.
- **Cold's outcomes are a documented contract**: exit 0 cold or already gone, 3 busy (holders on
  stderr), 4 skipped (reason on stderr), 1 error.

### The layer

```
bale hook claude create | remove      # stdin JSON (was claude-worktree-create|remove)
bale hook git post-checkout ...
bale hook conductor setup | archive   # reads CONDUCTOR_WORKSPACE_PATH
bale hook cursor setup                # cwd
bale hook codex setup [<path>]

bale install                          # scheduled sweep, as today
bale install --hooks claude,conductor,cursor,codex [--repo <REPO>]
bale install --git-hook <REPO>        # as today
```

`--repo` is required for conductor and cursor, whose config lives in the repo
(`.conductor/settings.toml`, `.cursor/worktrees.json`). Codex prints the setup command and edits nothing.

- **setup / post-checkout** adopt the tree. They never fail the caller: errors go to stderr, exit 0,
  because a failing setup script can break workspace creation in the tool.
- **remove / archive** compute the caller's pids and cold with them ignored. The walk goes up from the
  hook process and stops at the first ancestor whose name is in the tool's list:

  | Tool | Names that end the walk |
  |---|---|
  | claude | `claude`, `node` (as today) |
  | conductor | `Conductor` |
  | cursor | `Cursor`, `Cursor Helper` |
  | codex | `codex`, `Codex` |

  The non-Claude lists are a guess until checked against the running tool.
- **claude create** calls core `new`. No other tool lets a command replace worktree creation.
- **install** edits keep the commands already in the file and run bale's after them, for setup, or before
  them, for archive. It prints the diff first, and `--print-only` writes nothing. Conductor and Cursor
  files are usually committed, so the output says so.

Tools without a remove hook (Cursor, Codex) get adopt on setup, the git hook and the scheduled sweep. An
idle tree is baled within one sweep interval. No `git` wrapper; the parent spec's reasons stand.

### Migration

`bale install --claude-hooks` has not been run on any machine yet, so nothing installed points at
`bale hook claude-worktree-*`. Those names change to `bale hook claude create|remove` with no alias, and
`--claude-hooks` becomes `--hooks claude`. The move lands before the first hooks install.

## Risks

- **Conductor archive after cold.** Once bale makes the tree cold, its directory is gone and bale holds a
  git worktree lock. If Conductor's archive then runs `git worktree remove`, it may fail on the lock. Not
  documented; must be tried.
- **Cursor's own cleanup.** Cursor can delete worktrees it did not create once the count passes 25. A
  Baled tree still holds uncommitted work in its directory, and bale cannot stop the deletion. bale's `gc`
  cleans up the record afterwards, but the uncommitted work goes with the directory, as it would without
  bale.
- **Codex.** Setup-script details come from a third-party page.

## Verification before each adapter ships

Only Codex is installed on the development machine. The Claude and git adapters keep today's tests, moved
to `bale-hooks`. Each new adapter is marked unverified until run against
the real tool:

1. Create a workspace in the tool and confirm `bale ls` shows it registered.
2. For Conductor, archive a workspace with an uncommitted file and an ignored `.env`. Confirm the tree goes
   cold, the archive completes, and `bale open` restores both files.
3. Confirm the busy exclusion stops at the tool's process and still reports a real holder, such as a shell
   left in the tree.

## Out of scope

T3 Code, Gemini CLI, opencode, Amp, Factory, Zed and Copilot. Factory (`cleanup_script`) and Zed
(`create_worktree` task hook) have usable hooks and can be added as adapters later without touching the core.
