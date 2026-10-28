# bale: hooks for Conductor, Cursor and Codex

Addendum to `2026-10-27-bale-design.md`. That spec's "Command is the centre; hooks are thin callers"
section lists one harness integration, Claude Code. This adds three more and splits the hook code so each
tool is a small adapter over shared commands.

## Problem

Only the Claude Code hooks are tool-specific today. `bale` itself, the git `post-checkout` hook, `migrate`
and the scheduled sweep all work from git and already cover worktrees made by any tool. What the other
tools miss:

- **Registration at creation.** The git hook registers a new worktree only in repos where it is installed,
  and only when the tool's checkout runs it. A tool's own setup step is a second, more reliable signal.
- **Cold on removal.** Claude Code's `WorktreeRemove` makes the tree cold instead of letting it be
  deleted. Of the three tools, only Conductor has a step that runs before removal.
- **Busy detection inside a hook.** The remove hook excludes its own caller's process chain from the busy
  check. That exclusion stops at a process named `claude`; other tools need the same.

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
`ROOT_WORKTREE_PATH`. None of the three lets a command replace worktree creation, so `bale hook create`
stays Claude-only.

## Design

### Shared hook commands

```
bale hook adopt [<path>]    # register a worktree another tool made; default: cwd
bale hook remove <path>     # make it cold (Claude's WorktreeRemove, Conductor's archive)
bale hook create            # Claude Code only, unchanged
```

`adopt` registers the worktree and does nothing else. It never fails the caller: any error goes to stderr
and the exit code is 0, because a failing setup script can break workspace creation in the tool. It is the
same `register` the git hook calls, so it inherits the admin-dir checks.

`remove` keeps the Claude remove hook's exit contract: 0 when the tree went cold or was already gone, 3
when busy (holders named on stderr), 4 when skipped (reason on stderr), 1 on error. Claude's hook reads its path from a JSON payload; the
adapter extracts it and calls the shared code.

### Busy exclusion per tool

`bale hook remove --tool <name>` names the caller. The exclusion walks up from the hook process and stops
at the first ancestor whose executable name is in that tool's list. It never climbs past it, as today.

| Tool | Names that end the walk |
|---|---|
| claude | `claude`, `node` (as today) |
| conductor | `Conductor` |
| cursor | `Cursor`, `Cursor Helper` |
| codex | `codex`, `Codex` |

The lists are a guess until each is checked against the running tool (see Verification below).

### Install

Conductor and Cursor read their scripts from files inside the repo, so their install takes a repo, the
same shape as today's `--git-hook <REPO>`:

```
bale install --claude-hooks              # global, as today
bale install --conductor-hooks <REPO>    # edits <REPO>/.conductor/settings.toml
bale install --cursor-hooks <REPO>       # edits <REPO>/.cursor/worktrees.json
bale install --codex-hooks               # prints the setup command; edits nothing
```

- Conductor: add `bale hook adopt "$CONDUCTOR_WORKSPACE_PATH"` to `scripts.setup` and
  `bale hook remove --tool conductor "$CONDUCTOR_WORKSPACE_PATH"` to `scripts.archive`.
- Cursor: add `bale hook adopt` to `setup-worktree-unix` and `setup-worktree-windows`. If the file uses the
  plain `setup-worktree` key, add it there instead.
- Codex: the setup script's location is unconfirmed, so `bale install --hooks codex` prints the command
  to paste into the environment's setup script and edits nothing until the format is confirmed.

Every edit keeps the commands already in the file and runs bale's after them, for setup, or before them,
for archive. Install prints the diff before writing, and `--print-only` shows it without writing, as for the other
hooks. These files are usually committed, so the output says so.

### Tools without a remove hook

Cursor and Codex get the git hook, `adopt` on setup and the scheduled sweep. An idle tree is baled
within one sweep interval. No `git` wrapper; the parent spec's reasons stand.

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

Only Codex is installed on the development machine. Each adapter is marked unverified until it has been run
against the real tool:

1. Create a workspace in the tool and confirm `bale ls` shows it registered.
2. For Conductor, archive a workspace with an uncommitted file and an ignored `.env`. Confirm the tree goes
   cold, the archive completes, and `bale open` restores both files.
3. Confirm the busy exclusion stops at the tool's process and still reports a real holder, such as a shell
   left in the tree.

## Out of scope

T3 Code, Gemini CLI, opencode, Amp, Factory, Zed and Copilot. Factory (`cleanup_script`) and Zed
(`create_worktree` task hook) have usable hooks and can follow the same adapter shape later.
