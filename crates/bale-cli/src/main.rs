use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use bale_core::ops::cold::cold;
use bale_core::ops::migrate::{apply, plan, PlanRow, BALE};
use bale_core::ops::open::open;
use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::{resolve, up, Outcome};
use bale_core::registry::{Level, Registry};
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
    /// List registered worktrees.
    Ls {
        /// Print the records as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = BaleHome::from_env();
    let result = match cli.command {
        Command::Up { target } => {
            resolve(&home, &target).and_then(|id| Ok(report(&id, up(&home, &id)?)))
        }
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
        Command::Ls { json } => cmd_ls(&home, json).map(|()| ExitCode::SUCCESS),
    };
    result.unwrap_or_else(|e| {
        eprintln!("bale: {e:#}");
        ExitCode::from(1)
    })
}

/// Prints the outcome; exit 0 done, 3 busy, 4 skipped.
fn report(id: &str, outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::Done(level) => {
            println!("{id}: {}", level_name(level));
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

/// Prints the plan and totals; with `apply`, bales and prints the result.
/// Exits 1 when any row failed.
fn cmd_migrate(home: &BaleHome, repos: &[PathBuf], do_apply: bool) -> Result<ExitCode> {
    let rows = plan(home, repos)?;
    for line in plan_lines(&rows) {
        println!("{line}");
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
    Ok(if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// The plan table followed by a totals line.
fn plan_lines(rows: &[PlanRow]) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<10}  {:<11}  {:<22}  {:<20}  PATH",
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
            "{:<10}  {:<11}  {:<22}  {:<20}  {}",
            human(r.total_bytes),
            human(r.regenerable_bytes),
            flags,
            r.action,
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
            level_name(r.level),
            ago(r.last_activity),
            r.branch.as_deref().unwrap_or("(detached)"),
            r.path.display()
        );
    }
    Ok(())
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Hot => "hot",
        Level::Baling => "baling",
        Level::Baled => "baled",
        Level::Cooling => "cooling",
        Level::Cold => "cold",
        Level::Opening => "opening",
    }
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
        assert_eq!(lines[3], "2 worktrees, 1.0 KiB regenerable, 1 to bale");
    }

    #[test]
    fn ordinary_row_goes_to_stdout() {
        let lines = sweep_lines("abc", &Action::Up, "baled");
        assert_eq!(lines, vec![(false, "abc  up  baled".to_string())]);
    }
}
