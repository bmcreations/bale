use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use bale_core::ops::cold::cold;
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

/// One line per workspace; rows needing attention also go to stderr.
fn cmd_sweep(home: &BaleHome, dry_run: bool) -> Result<()> {
    for (id, action, result) in sweep(home, dry_run)? {
        let name = match action {
            Action::Keep => "keep",
            Action::Up => "up",
            Action::Cold => "cold",
            Action::Report(_) => "report",
        };
        let line = format!("{id}  {name}  {result}");
        println!("{line}");
        if matches!(action, Action::Report(_)) || result.starts_with("error: ") {
            eprintln!("bale: {line}");
        }
    }
    Ok(())
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
