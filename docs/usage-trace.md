# Usage trace

bale keeps a local log of how your worktrees are used, so a benchmark can
replay it later. It records:

- each `bale sweep` run: free and total space of every volume holding a
  registered tree, and how long the run took;
- each registered tree on every sweep: its id, path, repo, level, pin,
  the tool and session that made it, its last source edit, the space its
  build output and checkout take on disk, and what sweep decided;
- each operation (`new`, `adopt`, `up`, `cold`, `open`, `gc`): its outcome,
  how long it took, what started it (`cli`, `sweep` or a tool's hook), and
  for `up` and `cold` the free space gained;
- each Xcode build found in a tree's DerivedData build log manifest: start,
  stop, duration, error count and title, once per build. Gradle builds are
  not recorded.

No file contents are recorded, and the only paths are tree paths and repo
git dirs.

The trace lives in `$BALE_HOME/trace/` (`~/.bale/trace/` by default), one
JSON-lines file per UTC day. `bale trace path` prints the directory. Files
older than 60 days are deleted at the end of each sweep. The trace never
leaves your machine. To delete it, run `rm -r "$(bale trace path)"`.
