mod hook;
mod install;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use bale_core::ops::cold::cold;
use bale_core::ops::gc::{gc, GcReport};
use bale_core::ops::migrate::{apply, plan, PlanRow, BALE};
use bale_core::ops::new::new_workspace;
use bale_core::ops::open::open;
use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::{resolve, up_report, Outcome};
use bale_core::registry::Registry;
use bale_core::BaleHome;
use clap::{Parser, Subcommand};

/// Manage agent git worktrees: bale idle ones by removing build output.
#[derive(Parser)]
#[command(name = "bale", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Remove a worktree's regenerable build output in place.
    Up {
        /// Worktree path or workspace id.
        target: String,
    },
    /// Save a worktree's uncommitted work and ignored files, then delete its checkout.
    Cold {
        /// Worktree path or workspace id.
        target: String,
    },
    /// Restore a cold worktree at its path, then remove its saved state.
    Open {
        /// Worktree path or workspace id.
        target: String,
    },
    /// Apply the idle policy to every registered worktree.
    Sweep {
        /// Show what would run without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Adopt a repo's existing linked worktrees: show the plan, or bale them with --apply.
    Migrate {
        /// Repo paths (the main checkout or any of its worktrees).
        #[arg(required = true)]
        repos: Vec<PathBuf>,
        /// Register and bale the worktrees; without it nothing changes.
        #[arg(long)]
        apply: bool,
    },
    /// Create a linked worktree and register it.
    New {
        /// Worktree directory name.
        name: String,
        /// The repo to add it to (default: the current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Branch to check out, created when missing (default: the name).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Forget worktrees that are gone, and clear stale leftovers on hot ones.
    Gc {
        /// Show what would change without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Entry points for Claude Code and git hooks.
    Hook {
        #[command(subcommand)]
        kind: HookKind,
    },
    /// Install the 30-minute sweep schedule, or with flags only the given hooks.
    Install {
        /// Add WorktreeCreate and WorktreeRemove hooks to ~/.claude/settings.json.
        #[arg(long)]
        claude_hooks: bool,
        /// Install a post-checkout hook in this repo that registers new worktrees.
        #[arg(long, value_name = "REPO")]
        git_hook: Option<PathBuf>,
        /// Print the files and commands without changing anything.
        #[arg(long)]
        print_only: bool,
    },
    /// List registered worktrees.
    Ls {
        /// Print the records as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum HookKind {
    /// Claude Code WorktreeCreate: JSON on stdin, prints the worktree path.
    ClaudeWorktreeCreate,
    /// Claude Code WorktreeRemove: JSON on stdin, colds the worktree.
    ClaudeWorktreeRemove,
    /// git post-checkout: registers a newly added linked worktree.
    GitPostCheckout {
        prev: String,
        new: String,
        flag: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = BaleHome::from_env();
    let result = match cli.command {
        Command::Up { target } => resolve(&home, &target).and_then(|id| {
            let baled = up_report(&home, &id)?;
            let code = report(&id, baled.outcome);
            for dir in baled.kept {
                println!("  kept {}: {KEPT_WHY}", dir.display());
            }
            Ok(code)
        }),
        Command::Cold { target } => {
            resolve(&home, &target).and_then(|id| Ok(report(&id, cold(&home, &id)?)))
        }
        Command::Open { target } => resolve(&home, &target).and_then(|id| {
            let opened = open(&home, &id)?;
            let code = report(&id, opened.outcome);
            if let Some(reason) = opened.detached_reason {
                println!("  HEAD left detached: {reason}");
            }
            for note in opened.notes {
                println!("  {note}");
            }
            Ok(code)
        }),
        Command::Sweep { dry_run } => cmd_sweep(&home, dry_run).map(|()| ExitCode::SUCCESS),
        Command::Migrate { repos, apply } => cmd_migrate(&home, &repos, apply),
        Command::New { name, repo, branch } => {
            new_workspace(&home, &repo, &name, branch.as_deref()).map(|r| {
                println!("{}: {}", r.id, r.path.display());
                ExitCode::SUCCESS
            })
        }
        Command::Gc { dry_run } => gc(&home, dry_run).map(|report| {
            for (to_stderr, line) in gc_lines(&report, dry_run) {
                if to_stderr {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
            if gc_failed(&report, dry_run) {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }),
        Command::Hook { kind } => match kind {
            HookKind::ClaudeWorktreeCreate => hook::claude_worktree_create(&home),
            HookKind::ClaudeWorktreeRemove => hook::claude_worktree_remove(&home),
            HookKind::GitPostCheckout { prev, flag, .. } => {
                Ok(hook::git_post_checkout(&home, &prev, &flag))
            }
        },
        Command::Install {
            claude_hooks,
            git_hook,
            print_only,
        } => install::plan(claude_hooks, git_hook.as_deref()).and_then(|plan| {
            if print_only {
                plan.print();
                Ok(ExitCode::SUCCESS)
            } else {
                plan.apply().map(|()| ExitCode::SUCCESS)
            }
        }),
        Command::Ls { json } => cmd_ls(&home, json).map(|()| ExitCode::SUCCESS),
    };
    result.unwrap_or_else(|e| {
        eprintln!("bale: {e:#}");
        ExitCode::from(1)
    })
}

/// Why `up` left a regenerable-named dir in place.
const KEPT_WHY: &str = "holds files git tracks or does not ignore";

/// Prints the outcome; exit 0 done, 3 busy, 4 skipped.
fn report(id: &str, outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::Done(level) => {
            println!("{id}: {}", level.name());
            ExitCode::SUCCESS
        }
        Outcome::Busy(holders) => {
            println!("{id}: busy, left untouched");
            for h in holders {
                println!("  pid {} {} {}", h.pid, h.command, h.path.display());
            }
            ExitCode::from(3)
        }
        Outcome::Skipped(reason) => {
            println!("{id}: skipped ({reason})");
            ExitCode::from(4)
        }
    }
}

/// One line per workspace: on stdout, or on stderr when it needs attention.
fn cmd_sweep(home: &BaleHome, dry_run: bool) -> Result<()> {
    for (id, action, result) in sweep(home, dry_run)? {
        for (to_stderr, line) in sweep_lines(&id, &action, &result) {
            if to_stderr {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// The lines for one sweep row, each flagged `true` when it goes to stderr.
fn sweep_lines(id: &str, action: &Action, result: &str) -> Vec<(bool, String)> {
    let name = match action {
        Action::Keep => "keep",
        Action::Up => "up",
        Action::Cold => "cold",
        Action::Report(_) => "report",
    };
    let line = format!("{id}  {name}  {result}");
    if matches!(action, Action::Report(_)) || result.starts_with("error: ") {
        vec![(true, format!("bale: {line}"))]
    } else {
        vec![(false, line)]
    }
}

/// Whether gc exits 1: a real run with error rows. A dry run attempts
/// nothing, so its error rows are shown but do not fail it.
fn gc_failed(report: &GcReport, dry_run: bool) -> bool {
    !dry_run && !report.errors.is_empty()
}

/// One line per id in each list; errors are flagged `true` for stderr.
fn gc_lines(report: &GcReport, dry_run: bool) -> Vec<(bool, String)> {
    let would = if dry_run { "would be " } else { "" };
    let mut lines = Vec::new();
    for (list, verb) in [
        (&report.removed, "removed"),
        (&report.unlocked, "unlocked"),
        (&report.cleaned, "cleaned"),
    ] {
        lines.extend(
            list.iter()
                .map(|id| (false, format!("{id}: {would}{verb}"))),
        );
    }
    for (id, err) in &report.errors {
        lines.push((true, format!("bale: {id}: {}", action_cell(err))));
    }
    lines
}

/// Prints the plan and totals; with `apply`, bales and prints the result.
/// Exits 1 when any row failed.
fn cmd_migrate(home: &BaleHome, repos: &[PathBuf], do_apply: bool) -> Result<ExitCode> {
    let rows = plan(home, repos)?;
    for line in plan_lines(&rows) {
        println!("{line}");
    }
    if cfg!(target_os = "macos") {
        println!("note: regenerable excludes global Xcode DerivedData, which bale also removes");
    }
    // The table truncates error text; print it whole.
    for r in rows
        .iter()
        .filter(|r| r.action.starts_with("report (error: "))
    {
        eprintln!("bale: {}: {}", r.path.display(), r.action);
    }
    if !do_apply {
        println!("dry run: nothing changed; re-run with --apply");
        return Ok(ExitCode::SUCCESS);
    }
    let report = apply(home, &rows)?;
    println!(
        "baled {}, busy {}, reported {}, errors {}",
        report.baled,
        report.busy,
        report.reported,
        report.errors.len()
    );
    println!(
        "free space: {} before, {} after",
        human(report.free_before),
        human(report.free_after)
    );
    for (path, err) in &report.errors {
        eprintln!("bale: {}: {err}", path.display());
    }
    // A tree that could not be probed was not baled; don't report a clean run.
    let probe_errors = rows.iter().any(|r| r.action.starts_with("report (error: "));
    Ok(if report.errors.is_empty() && !probe_errors {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Widest action cell before truncation.
const ACTION_WIDTH: usize = 60;

/// `action` on one line (newlines become spaces), cut to `ACTION_WIDTH`
/// characters with a trailing `...`.
fn action_cell(action: &str) -> String {
    let flat: String = action
        .split(['\r', '\n'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= ACTION_WIDTH {
        return flat;
    }
    let cut: String = flat.chars().take(ACTION_WIDTH - 3).collect();
    format!("{cut}...")
}

/// The plan table followed by a totals line.
fn plan_lines(rows: &[PlanRow]) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<10}  {:<11}  {:<24}  {:<20}  PATH",
        "TOTAL", "REGENERABLE", "FLAGS", "ACTION"
    )];
    for r in rows {
        let flags: Vec<&str> = [
            (r.dirty, "dirty"),
            (r.busy, "busy"),
            (r.temp_location, "temp"),
            (r.prunable, "prunable"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        let flags = if flags.is_empty() {
            "-".to_string()
        } else {
            flags.join(",")
        };
        lines.push(format!(
            "{:<10}  {:<11}  {:<24}  {:<20}  {}",
            human(r.total_bytes),
            human(r.regenerable_bytes),
            flags,
            action_cell(&r.action),
            r.path.display()
        ));
    }
    let regen: u64 = rows.iter().map(|r| r.regenerable_bytes).sum();
    let to_bale = rows.iter().filter(|r| r.action == BALE).count();
    lines.push(format!(
        "{} worktrees, {} regenerable, {} to bale",
        rows.len(),
        human(regen),
        to_bale
    ));
    lines
}

/// Bytes in binary units with one decimal, e.g. `1.5 GiB`.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn cmd_ls(home: &BaleHome, json: bool) -> Result<()> {
    let records = Registry::open(home.clone()).all()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    println!(
        "{:<12}  {:<7}  {:<10}  {:<20}  PATH",
        "ID", "LEVEL", "ACTIVE", "BRANCH"
    );
    for r in records {
        println!(
            "{:<12}  {:<7}  {:<10}  {:<20}  {}",
            r.id,
            r.level.name(),
            ago(r.last_activity),
            r.branch.as_deref().unwrap_or("(detached)"),
            r.path.display()
        );
    }
    Ok(())
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(lines: &[(bool, String)], text: &str) -> usize {
        lines.iter().filter(|(_, l)| l.contains(text)).count()
    }

    #[test]
    fn report_text_is_printed_once_on_stderr() {
        let text = "git status failed in /x: fatal: not a git repository";
        let lines = sweep_lines("abc", &Action::Report(text.into()), text);
        assert_eq!(count(&lines, text), 1, "{lines:?}");
        assert!(lines.iter().all(|(err, _)| *err), "{lines:?}");
    }

    #[test]
    fn error_result_is_printed_once_on_stderr() {
        let lines = sweep_lines("abc", &Action::Up, "error: boom");
        assert_eq!(count(&lines, "boom"), 1, "{lines:?}");
        assert!(lines.iter().all(|(err, _)| *err), "{lines:?}");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn plan_table_shows_flags_and_totals() {
        let row = |action: &str, dirty: bool, regen: u64| PlanRow {
            repo: PathBuf::from("/r"),
            path: PathBuf::from("/w"),
            total_bytes: 2048,
            regenerable_bytes: regen,
            dirty,
            busy: false,
            temp_location: false,
            prunable: false,
            action: action.into(),
        };
        let lines = plan_lines(&[row("bale", true, 1024), row("report (temp)", false, 0)]);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(
            lines[1].contains("dirty") && lines[1].contains("bale"),
            "{lines:?}"
        );
        assert!(lines[2].contains("report (temp)"), "{lines:?}");
        let all = PlanRow {
            dirty: true,
            busy: true,
            temp_location: true,
            prunable: true,
            ..row("x", true, 0)
        };
        let line = &plan_lines(&[all])[1];
        let action_at = line.find("  x  ").expect(line);
        assert!(
            line[..action_at].ends_with("dirty,busy,temp,prunable"),
            "{line}"
        );
        assert_eq!(lines[3], "2 worktrees, 1.0 KiB regenerable, 1 to bale");
    }

    #[test]
    fn error_action_stays_on_one_bounded_line() {
        let long = format!("report (error: first\nsecond\r\n{})", "x".repeat(300));
        let row = PlanRow {
            repo: PathBuf::from("/r"),
            path: PathBuf::from("/w"),
            total_bytes: 0,
            regenerable_bytes: 0,
            dirty: false,
            busy: false,
            temp_location: false,
            prunable: false,
            action: long,
        };
        let lines = plan_lines(&[row]);
        let line = &lines[1];
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        assert!(line.contains("first second"), "{line:?}");
        assert!(line.ends_with("  /w"), "{line:?}");
        assert!(line.chars().count() < 160, "{line:?}");
    }

    #[test]
    fn gc_lines_list_each_id_and_send_errors_to_stderr() {
        let report = GcReport {
            removed: vec!["a".into()],
            unlocked: vec!["b".into()],
            cleaned: vec!["b".into()],
            errors: vec![("c".into(), "boom\nmore".into())],
        };
        assert_eq!(
            gc_lines(&report, false),
            vec![
                (false, "a: removed".to_string()),
                (false, "b: unlocked".to_string()),
                (false, "b: cleaned".to_string()),
                (true, "bale: c: boom more".to_string()),
            ]
        );
        assert_eq!(gc_lines(&report, true)[0].1, "a: would be removed");
    }

    #[test]
    fn gc_fails_only_for_errors_in_a_real_run() {
        let clean = GcReport::default();
        let errored = GcReport {
            errors: vec![("c".into(), "boom".into())],
            ..GcReport::default()
        };
        assert!(!gc_failed(&clean, false));
        assert!(!gc_failed(&clean, true));
        assert!(gc_failed(&errored, false));
        assert!(!gc_failed(&errored, true));
    }

    #[test]
    fn ordinary_row_goes_to_stdout() {
        let lines = sweep_lines("abc", &Action::Up, "baled");
        assert_eq!(lines, vec![(false, "abc  up  baled".to_string())]);
    }
}
