# Usage trace

bale keeps a local log of how your worktrees are used, so a benchmark can
replay it later. It records:

- each `bale sweep` run: free and total space of the volume holding
  bale's state and of every volume holding a registered tree, the space
  all of Xcode's shared DerivedData takes (`global_dd_total_bytes`), the
  part of it built from no registered tree or with no `WorkspacePath`
  (`global_dd_unattributed_bytes`), and how long the run took;
- each registered tree on every sweep: its id, path, repo, level, pin,
  the tool and session that made it, its last source edit, the space its
  checkout and its build output take on disk, the space of Xcode's shared
  DerivedData built from it (`global_dd_bytes`, which `bale up` also
  removes on macOS), and what sweep decided;
- each operation (`new`, `adopt`, `up`, `cold`, `open`, `gc`): its outcome,
  a fixed reason code when it did not succeed, how long it took, what
  started it (`cli`, `sweep` or a tool's hook), and for `up` and `cold` the
  free space gained. A `cold` run by a tool's remove or archive hook also
  carries `"intent": "remove"`, and a tree that was already gone is
  recorded with outcome `missing`;
- each Xcode build found in a tree's DerivedData build log manifest: start,
  stop, duration, error count and title, once per build. Manifests are
  read on every sweep and by `bale up` and `bale cold` before they remove
  DerivedData. Gradle builds are not recorded.

Sizing is bounded so it cannot stall a sweep. A tree's checkout gets
30 seconds. Each shared DerivedData folder is walked at most once per
sweep, however many trees it was built from, with 120 seconds of its own;
a folder built from several trees counts in full on each of their lines
and once in the sweep totals. Folders no tree claims share one 120-second
budget. A walk cut short records the bytes it counted so far and a flag:
`sizing_truncated` for the checkout, `global_dd_truncated` for shared
DerivedData (on the tree line, and on the sweep line for the total), and
`global_dd_unattributed_truncated` on the sweep line. `sizing_ms` is the
checkout walk alone.

No file contents are recorded, and the only paths are tree paths and repo
git dirs. Paths and ids longer than the trace allows are cut, and the line
is marked `"clipped": true`.

The trace lives in `$BALE_HOME/trace/` (`~/.bale/trace/` by default), one
JSON-lines file per UTC day. `bale trace path` prints the directory. Files
older than 60 days are deleted at the end of each sweep. The trace never
leaves your machine. To delete it, run `rm -r "$(bale trace path)"`.
