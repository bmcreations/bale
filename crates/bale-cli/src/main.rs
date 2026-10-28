use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::{Context, Result};
use bale_core::ops::up::{register, up, Outcome};
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
        Command::Up { target } => cmd_up(&home, &target),
        Command::Ls { json } => cmd_ls(&home, json).map(|()| ExitCode::SUCCESS),
    };
    result.unwrap_or_else(|e| {
        eprintln!("bale: {e:#}");
        ExitCode::from(1)
    })
}

/// An existing path is registered and its id returned; anything else must be
/// the id of a registered workspace.
fn resolve(home: &BaleHome, arg: &str) -> Result<String> {
    let path = Path::new(arg);
    if path.exists() {
        return Ok(register(home, path)?.id);
    }
    Registry::open(home.clone())
        .get(arg)?
        .map(|r| r.id)
        .with_context(|| format!("{arg} is neither an existing path nor a registered workspace id"))
}

fn cmd_up(home: &BaleHome, target: &str) -> Result<ExitCode> {
    let id = resolve(home, target)?;
    match up(home, &id)? {
        Outcome::Done(level) => {
            println!("{id}: {}", level_name(level));
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Busy(holders) => {
            println!("{id}: busy, left untouched");
            for h in holders {
                println!("  pid {} {} {}", h.pid, h.command, h.path.display());
            }
            Ok(ExitCode::from(3))
        }
        Outcome::Skipped(reason) => {
            println!("{id}: skipped ({reason})");
            Ok(ExitCode::from(4))
        }
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
