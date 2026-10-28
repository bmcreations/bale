# bale: tool integrations as a layer over bale

Addendum to `2026-10-27-bale-design.md`. Two changes: every tool integration, Claude Code's included, moves
out of `bale` into a separate layer, and that layer gains adapters for Conductor, Cursor and Codex.

## Problem

`bale-cli` today contains the Claude Code hooks (`bale hook claude-worktree-create|remove`), the git
`post-checkout` hook, and `bale install --claude-hooks|--git-hook`, which edits `~/.claude/settings.json`
and `.git/hooks`. So the core binary knows one tool's payload format, its process names (`claude`, `node`)
and its config file. Adding three more tools the same way puts four tools' formats in the core.

The core should not know which tool called it. Integrations are a layer on top that translate a tool's
event into `bale` commands, the same way any user or script would call them.

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

`bale` (crates `bale-core`, `bale-cli`) keeps only tool-agnostic commands. Nothing in it names a tool, reads
a tool's payload or edits a tool's config.

`bale-hooks` (new crate, binary `bale-hooks`) holds every integration: one module per tool, each a thin
adapter that parses the tool's input and runs `bale` commands. It talks to the core only through the
`bale` CLI and its exit codes, so an adapter could be rewritten in any language, and a harness that adopts
bale natively needs no adapter at all.

`bale` forwards an unknown subcommand `x` to a `bale-x` executable on `PATH`, as git and cargo do, so
`bale hooks ...` runs `bale-hooks ...`. The core knows the convention, not the layer.

### What the core exposes

The adapters need three things the CLI does not offer today. They are generic, not per tool:

```
bale new <name> [--repo R] [--branch B] [--print-path]   # create + register; prints only the path
bale adopt [<path>]                                      # register an existing worktree; default cwd
bale cold <target> [--ignore-pid <pid>]...               # busy check ignores these pids
```

- `adopt` is the `register` the git hook calls today, with its admin-dir checks.
- `--ignore-pid` replaces the in-process `busy::exclude_pids` call the Claude hook makes. Which pids to
  ignore is the caller's business; the core just honours the list.
- `cold`'s exit codes become a documented contract: 0 cold or already gone, 3 busy (holders on stderr),
  4 skipped (reason on stderr), 1 error. Today they are the Claude hook's private contract.

`bale install` keeps only the scheduled sweep (launchd, systemd timer, schtasks). `--claude-hooks` and
`--git-hook` move to the layer.

### The layer

```
bale-hooks claude create | remove      # stdin JSON, as today's bale hook claude-worktree-*
bale-hooks git post-checkout ...       # today's bale hook git-post-checkout
bale-hooks conductor setup | archive   # reads CONDUCTOR_WORKSPACE_PATH
bale-hooks cursor setup                # cwd, or ROOT_WORKTREE_PATH for the project
bale-hooks codex setup                 # path argument

bale-hooks install claude              # ~/.claude/settings.json, as today
bale-hooks install git <REPO>          # .git/hooks/post-checkout, as today
bale-hooks install conductor <REPO>    # <REPO>/.conductor/settings.toml
bale-hooks install cursor <REPO>       # <REPO>/.cursor/worktrees.json
bale-hooks install codex               # prints the setup command; edits nothing
```

- **setup / post-checkout** call `bale adopt`. They never fail the caller: errors go to stderr, exit 0,
  because a failing setup script can break workspace creation in the tool.
- **remove / archive** compute the caller's pids, call `bale cold --ignore-pid ...` and map its exit code
  to what the tool expects. The pid walk goes up from the adapter and stops at the first ancestor whose
  name is in the tool's list:

  | Tool | Names that end the walk |
  |---|---|
  | claude | `claude`, `node` (as today) |
  | conductor | `Conductor` |
  | cursor | `Cursor`, `Cursor Helper` |
  | codex | `codex`, `Codex` |

  The non-Claude lists are a guess until checked against the running tool.
- **claude create** calls `bale new --print-path`. No other tool lets a command replace worktree creation.
- **install** edits keep the commands already in the file and run bale's after them, for setup, or before
  them, for archive. It prints the diff first, and `--print-only` writes nothing. Conductor and Cursor
  files are usually committed, so the output says so.

Tools without a remove hook (Cursor, Codex) get `adopt` on setup, the git hook and the scheduled sweep. An
idle tree is baled within one sweep interval. No `git` wrapper; the parent spec's reasons stand.

### Migration

`bale install --claude-hooks` has not been run on any machine yet, so nothing installed points at
`bale hook claude-worktree-*`. The old `bale hook` subcommand is removed, not kept as an alias. The
extraction lands before the first `bale-hooks install claude`.

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
to the new crate and run through the `bale` binary. Each new adapter is marked unverified until run against
the real tool:

1. Create a workspace in the tool and confirm `bale ls` shows it registered.
2. For Conductor, archive a workspace with an uncommitted file and an ignored `.env`. Confirm the tree goes
   cold, the archive completes, and `bale open` restores both files.
3. Confirm the busy exclusion stops at the tool's process and still reports a real holder, such as a shell
   left in the tree.

## Out of scope

T3 Code, Gemini CLI, opencode, Amp, Factory, Zed and Copilot. Factory (`cleanup_script`) and Zed
(`create_worktree` task hook) have usable hooks and can be added as adapters later without touching the core.
