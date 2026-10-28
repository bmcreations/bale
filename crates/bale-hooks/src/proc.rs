//! Which processes are only waiting on a hook, so their hold on a tree does
//! not count as busy.

use std::path::Path;

/// Ancestors checked for a stop name before falling back.
const MAX_DEPTH: usize = 3;

/// The pids whose hold on the tree comes from running this hook. Walks up
/// from `own` (always included) through at most three ancestors and stops
/// after the first whose name, or its basename, is in `stop_names`
/// (case-sensitive); every pid walked through to it is returned.
///
/// When no stop name turns up within three levels, it returns `own`, its
/// parent and, only when the parent is a shell, the grandparent. A tool
/// runs a hook either with `sh -c`, which only stays around for a compound
/// command, or exec'd from it, so a shell parent means the grandparent is
/// the tool and any other parent is the tool itself. Nothing above that
/// (such as the user's shell that started the tool) is included.
///
/// `info` gives a pid's parent pid and process name or exe; walks stop at
/// pid 1 and wherever it returns `None`.
pub fn caller_pids(
    own: u32,
    stop_names: &[&str],
    info: impl Fn(u32) -> Option<(u32, String)>,
) -> Vec<u32> {
    // `walked[i]` has the name `names[i]` where known.
    let mut walked = vec![own];
    let mut names: Vec<String> = Vec::new();
    let mut cur = own;
    while let Some((parent, name)) = info(cur) {
        let at_stop = walked.len() > 1 && is_named(&name, stop_names);
        names.push(name);
        if at_stop {
            return walked;
        }
        if walked.len() > MAX_DEPTH || parent <= 1 {
            break;
        }
        walked.push(parent);
        cur = parent;
    }
    let mut pids = vec![own];
    if let Some(&parent) = walked.get(1) {
        pids.push(parent);
        if let (Some(name), Some(&grandparent)) = (names.get(1), walked.get(2)) {
            if is_shell(name) {
                pids.push(grandparent);
            }
        }
    }
    pids
}

/// `name` or its basename is one of `stop_names`.
fn is_named(name: &str, stop_names: &[&str]) -> bool {
    stop_names
        .iter()
        .any(|s| *s == name || *s == basename(name))
}

fn basename(name: &str) -> &str {
    Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name)
}

/// `sh`, `bash`, `dash`, `zsh` or `fish`, by basename; a login shell's
/// leading `-` is ignored.
fn is_shell(name: &str) -> bool {
    matches!(
        basename(name).trim_start_matches('-'),
        "sh" | "bash" | "dash" | "zsh" | "fish"
    )
}

/// `(parent pid, command)` for `pid` from `ps`; `None` where unavailable.
pub fn process_info(pid: u32) -> Option<(u32, String)> {
    if cfg!(windows) {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (ppid, comm) = text.trim().split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, comm.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use std::collections::HashMap;

    fn table(rows: &[(u32, u32, &str)]) -> impl Fn(u32) -> Option<(u32, String)> {
        let map: HashMap<u32, (u32, String)> = rows
            .iter()
            .map(|(pid, ppid, name)| (*pid, (*ppid, name.to_string())))
            .collect();
        move |pid| map.get(&pid).cloned()
    }

    fn claude(own: u32, info: impl Fn(u32) -> Option<(u32, String)>) -> Vec<u32> {
        caller_pids(own, Tool::Claude.stop_names(), info)
    }

    #[test]
    fn caller_pids_stops_at_tool_name() {
        // shell(10) -> tool(20) -> zsh(30) -> sh(35) -> bale(40): the tool
        // is three levels up, past two shells.
        let cases: [(Tool, &str); 7] = [
            (Tool::Claude, "/Users/me/.local/bin/claude"),
            (Tool::Claude, "node"),
            (Tool::Conductor, "/Applications/Conductor.app/Contents/MacOS/Conductor"),
            (Tool::Cursor, "Cursor"),
            (
                Tool::Cursor,
                "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper",
            ),
            (Tool::Codex, "codex"),
            (Tool::Codex, "/Applications/Codex.app/Contents/MacOS/Codex"),
        ];
        for (tool, name) in cases {
            let info = table(&[
                (10, 1, "-zsh"),
                (20, 10, name),
                (30, 20, "zsh"),
                (35, 30, "sh"),
                (40, 35, "bale"),
            ]);
            assert_eq!(
                caller_pids(40, tool.stop_names(), info),
                vec![40, 35, 30, 20],
                "{tool:?} {name}"
            );
        }
        // Git has no tool process: today's rule applies.
        let info = table(&[(10, 1, "-zsh"), (30, 10, "sh"), (40, 30, "bale")]);
        assert_eq!(
            caller_pids(40, Tool::Git.stop_names(), info),
            vec![40, 30, 10]
        );
    }

    #[test]
    fn stop_names_are_case_sensitive() {
        // `cursor` is not `Cursor`: fall back to parent and grandparent.
        let info = table(&[
            (10, 1, "zsh"),
            (20, 10, "cursor"),
            (30, 20, "zsh"),
            (35, 30, "sh"),
            (40, 35, "bale"),
        ]);
        assert_eq!(
            caller_pids(40, Tool::Cursor.stop_names(), info),
            vec![40, 35, 30]
        );
    }

    #[test]
    fn stop_name_four_levels_up_falls_back() {
        // claude(5) -> zsh(10) -> sh(20) -> sh(30) -> bale(40): claude is
        // past the three levels the walk checks.
        let info = table(&[
            (5, 1, "claude"),
            (10, 5, "zsh"),
            (20, 10, "sh"),
            (30, 20, "sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(claude(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn stops_at_claude_and_never_reaches_the_users_shell() {
        // zsh(10) -> claude(20) -> sh(30) -> bale(40)
        let info = table(&[
            (10, 1, "-zsh"),
            (20, 10, "/Users/me/.local/bin/claude"),
            (30, 20, "/bin/sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(claude(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn never_climbs_to_a_higher_claude() {
        // claude(5) -> zsh(10) -> node(20) -> sh(30) -> bale(40): an outer
        // Claude Code session above the user's shell stays a holder.
        let info = table(&[
            (5, 1, "claude"),
            (10, 5, "zsh"),
            (20, 10, "node"),
            (30, 20, "sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(claude(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn falls_back_to_the_grandparent() {
        // zsh(10) -> other(20) -> sh(30) -> bale(40)
        let info = table(&[
            (10, 1, "zsh"),
            (20, 10, "other"),
            (30, 20, "sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(claude(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn non_shell_parent_ends_the_chain() {
        // zsh(10) -> node(20) -> bale(40): `sh -c` exec'd bale, so the
        // grandparent is the user's shell and stays a holder.
        let info = table(&[(10, 1, "-zsh"), (20, 10, "node"), (40, 20, "bale")]);
        assert_eq!(claude(40, info), vec![40, 20]);
    }

    #[test]
    fn non_shell_parent_without_stop_name_ends_the_chain() {
        // zsh(10) -> perl(20) -> bale(40)
        let info = table(&[(10, 1, "-zsh"), (20, 10, "perl"), (40, 20, "bale")]);
        assert_eq!(claude(40, info), vec![40, 20]);
    }

    #[test]
    fn login_shell_parent_takes_the_grandparent() {
        // other(20) -> -bash(30) -> bale(40)
        let info = table(&[
            (10, 1, "zsh"),
            (20, 10, "other"),
            (30, 20, "-bash"),
            (40, 30, "bale"),
        ]);
        assert_eq!(claude(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn parent_that_is_claude_ends_the_chain() {
        let info = table(&[(10, 1, "zsh"), (20, 10, "claude"), (40, 20, "bale")]);
        assert_eq!(claude(40, info), vec![40, 20]);
    }

    #[test]
    fn unknown_process_table_excludes_only_self() {
        assert_eq!(claude(40, |_| None), vec![40]);
    }
}
