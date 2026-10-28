mod install;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use bale_core::ops::cold::cold_ignoring;
use bale_core::ops::gc::{gc, GcReport};
use bale_core::ops::meta::set_pinned;
use bale_core::ops::migrate::{apply, plan, PlanRow, BALE};
use bale_core::ops::new::new_workspace;
use bale_core::ops::open::open;
use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::{register, resolve, up_report, Outcome};
use bale_core::par::par_map;
use bale_core::registry::{workspace_id, Level, Record, Registry, Scope};
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
        /// Do not treat this process as holding the tree (repeatable).
        #[arg(long = "ignore-pid", value_name = "PID")]
        ignore_pid: Vec<u32>,
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
        /// Print only the new worktree's path.
        #[arg(long)]
        print_path: bool,
    },
    /// Register an existing linked worktree.
    Adopt {
        /// The worktree checkout (default: the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Forget worktrees that are gone, and clear stale leftovers on hot ones.
    Gc {
        /// Show what would change without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Entry points for Claude Code, git and Conductor hooks.
    Hook {
        #[command(subcommand)]
        kind: HookKind,
    },
    /// Install the 30-minute sweep schedule, or with --hooks only those tools' hooks.
    Install {
        /// Comma-separated tools to hook: claude, git, conductor, cursor, codex.
        #[arg(long, value_name = "LIST", value_delimiter = ',', num_args = 1)]
        hooks: Vec<bale_hooks::Tool>,
        /// The repo whose hooks to edit; required for git, conductor and cursor.
        #[arg(long, value_name = "REPO", requires = "hooks")]
        repo: Option<PathBuf>,
        /// Print the files and commands without changing anything.
        #[arg(long)]
        print_only: bool,
    },
    /// Remove the sweep schedule, or with --hooks only bale's entries in those tools' hooks.
    /// Saved state and cold worktrees are kept.
    ///
    /// A config file left empty is deleted when bale finds no backup of it
    /// and git does not track it. That includes a ~/.claude/settings.json
    /// installed by an older bale whose settings.json.bak-bale was since
    /// deleted: bale treats it as its own.
    Uninstall {
        /// Comma-separated tools to unhook: claude, git, conductor, cursor, codex.
        #[arg(long, value_name = "LIST", value_delimiter = ',', num_args = 1)]
        hooks: Vec<bale_hooks::Tool>,
        /// The repo whose hooks to edit; required for git, conductor and cursor.
        #[arg(long, value_name = "REPO", requires = "hooks")]
        repo: Option<PathBuf>,
        /// Print the files and commands without changing anything.
        #[arg(long)]
        print_only: bool,
    },
    /// Keep sweep from upping or colding a worktree.
    Pin {
        /// Worktree path or workspace id (default: the current directory).
        #[arg(default_value = ".")]
        target: String,
    },
    /// Let sweep manage a pinned worktree again.
    Unpin {
        /// Worktree path or workspace id (default: the current directory).
        #[arg(default_value = ".")]
        target: String,
    },
    /// Show a registered worktree: its level, pin, origin session and git scope.
    Show {
        /// Worktree path or workspace id (default: the current directory).
        #[arg(default_value = ".")]
        target: String,
    },
    /// List registered worktrees.
    Ls {
        /// Print the records as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The local usage trace bale records for benchmarking.
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
}

#[derive(Subcommand)]
enum TraceCommand {
    /// Print the directory the trace is kept in.
    Path,
}

#[derive(Subcommand)]
enum HookKind {
    /// Claude Code's WorktreeCreate and WorktreeRemove hooks.
    Claude {
        #[command(subcommand)]
        event: ClaudeHook,
    },
    /// git hooks.
    Git {
        #[command(subcommand)]
        event: GitHook,
    },
    /// Conductor's setup and archive scripts.
    Conductor {
        #[command(subcommand)]
        event: ConductorHook,
    },
    /// Cursor's worktree setup script.
    Cursor {
        #[command(subcommand)]
        event: CursorHook,
    },
    /// Codex's environment setup script.
    Codex {
        #[command(subcommand)]
        event: CodexHook,
    },
}

#[derive(Subcommand)]
enum ClaudeHook {
    /// WorktreeCreate: JSON on stdin, prints the worktree path.
    Create,
    /// WorktreeRemove: JSON on stdin, colds the worktree.
    Remove,
}

#[derive(Subcommand)]
enum ConductorHook {
    /// setup: registers $CONDUCTOR_WORKSPACE_PATH (or the cwd); always exits 0.
    Setup,
    /// archive: colds $CONDUCTOR_WORKSPACE_PATH (or the cwd).
    Archive,
}

#[derive(Subcommand)]
enum CursorHook {
    /// setup: registers the cwd; always exits 0.
    Setup,
}

#[derive(Subcommand)]
enum CodexHook {
    /// setup: registers PATH (default: the cwd); always exits 0.
    Setup {
        /// The worktree to register.
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum GitHook {
    /// post-checkout: registers a newly added linked worktree.
    PostCheckout {
        prev: String,
        new: String,
        flag: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = BaleHome::from_env();
    let result = bale_core::trace::with_trigger("cli", || run(cli.command, home));
    result.unwrap_or_else(|e| {
        eprintln!("bale: {e:#}");
        ExitCode::from(1)
    })
}

fn run(command: Command, home: BaleHome) -> Result<ExitCode> {
    match command {
        Command::Up { target } => resolve(&home, &target).and_then(|id| {
            let baled = up_report(&home, &id)?;
            let code = report(&id, baled.outcome);
            for dir in baled.kept {
                println!("  kept {}: {KEPT_WHY}", dir.display());
            }
            Ok(code)
        }),
        Command::Cold { target, ignore_pid } => resolve(&home, &target)
            .and_then(|id| Ok(report_cold(&id, cold_ignoring(&home, &id, &ignore_pid)?))),
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
        Command::New {
            name,
            repo,
            branch,
            print_path,
        } => new_workspace(&home, &repo, &name, branch.as_deref()).map(|r| {
            if print_path {
                println!("{}", r.path.display());
            } else {
                println!("{}: {}", r.id, r.path.display());
            }
            ExitCode::SUCCESS
        }),
        Command::Adopt { path } => register(&home, &path).map(|r| {
            println!("{}: {}", r.id, r.level.name());
            ExitCode::SUCCESS
        }),
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
            HookKind::Claude {
                event: ClaudeHook::Create,
            } => bale_hooks::claude::create(&home),
            HookKind::Claude {
                event: ClaudeHook::Remove,
            } => bale_hooks::claude::remove(&home),
            HookKind::Git {
                event: GitHook::PostCheckout { prev, new, flag },
            } => Ok(bale_hooks::git::post_checkout(&home, &prev, &new, &flag)),
            HookKind::Conductor {
                event: ConductorHook::Setup,
            } => Ok(bale_hooks::conductor::setup(&home)),
            HookKind::Conductor {
                event: ConductorHook::Archive,
            } => Ok(bale_hooks::conductor::archive(&home)),
            HookKind::Cursor {
                event: CursorHook::Setup,
            } => Ok(bale_hooks::cursor::setup(&home)),
            HookKind::Codex {
                event: CodexHook::Setup { path },
            } => Ok(bale_hooks::codex::setup(&home, path)),
        },
        Command::Install {
            hooks,
            repo,
            print_only,
        } => install::plan(&hooks, repo.as_deref(), &home)
            .and_then(|plan| run_plan(&plan, print_only)),
        Command::Uninstall {
            hooks,
            repo,
            print_only,
        } => install::uninstall_plan(&hooks, repo.as_deref(), &home)
            .and_then(|plan| run_plan(&plan, print_only)),
        Command::Pin { target } => cmd_pin(&home, &target, true),
        Command::Unpin { target } => cmd_pin(&home, &target, false),
        Command::Show { target } => cmd_show(&home, &target).map(|()| ExitCode::SUCCESS),
        Command::Ls { json } => cmd_ls(&home, json).map(|()| ExitCode::SUCCESS),
        Command::Trace {
            command: TraceCommand::Path,
        } => {
            println!("{}", home.trace_dir().display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Prints `plan` with `print_only`, else applies it.
fn run_plan(plan: &bale_hooks::install::Plan, print_only: bool) -> Result<ExitCode> {
    if print_only {
        plan.print();
        Ok(ExitCode::SUCCESS)
    } else {
        plan.apply().map(|()| ExitCode::SUCCESS)
    }
}

/// Why `up` left a regenerable-named dir in place.
const KEPT_WHY: &str =
    "holds files git tracks or does not ignore, or is in a submodule or nested repo";

/// Prints the outcome; exit 0 done, 3 busy, 4 skipped.
fn report(id: &str, outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::Done(level) => {
            println!("{id}: {}", level.name());
            ExitCode::SUCCESS
        }
        Outcome::Busy(holders) => {
            print!("{}", busy_report(id, &holders, false));
            ExitCode::from(3)
        }
        Outcome::Skipped(reason) => {
            println!("{id}: skipped ({reason})");
            ExitCode::from(4)
        }
    }
}

/// `report` for `bale cold`, whose empty `Busy` means the tree changed or
/// was in use while cold was saving it, not that nothing was found.
fn report_cold(id: &str, outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::Busy(holders) => {
            print!("{}", busy_report(id, &holders, true));
            ExitCode::from(3)
        }
        other => report(id, other),
    }
}

/// What `report` prints for a busy tree.
fn busy_report(id: &str, holders: &[bale_core::busy::Holder], during_cold: bool) -> String {
    if holders.is_empty() && during_cold {
        return format!("{id}: {}\n", bale_hooks::CHANGED_DURING_COLD);
    }
    let mut text = format!("{id}: busy, left untouched\n");
    for h in holders {
        text.push_str(&format!(
            "  pid {} {} {}\n",
            h.pid,
            h.command,
            h.path.display()
        ));
    }
    text
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
    for (id, path) in &report.checks {
        lines.push((
            false,
            format!("{id}: check dir {} {would}removed", path.display()),
        ));
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
    bale_core::human_bytes(bytes)
}

fn cmd_ls(home: &BaleHome, json: bool) -> Result<()> {
    let records = Registry::open(home.clone()).all()?;
    if json {
        let scopes = par_map(&records, |r| scope_of(r).map(|(s, _)| s));
        let rows = records
            .iter()
            .zip(scopes)
            .map(|(r, scope)| {
                let mut v = serde_json::to_value(r)?;
                v["origin"] = serde_json::to_value(&r.origin)?;
                v["pinned"] = r.pinned.into();
                v["scope"] = serde_json::to_value(scope)?;
                Ok(v)
            })
            .collect::<Result<Vec<_>>>()?;
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!(
        "{:<12}  {:<8}  {:<10}  {:<20}  {:<50}  PATH",
        "ID", "LEVEL", "ACTIVE", "BRANCH", "SESSION"
    );
    let home_dir = install::home_dir().ok();
    let labels = par_map(&records, |r| session_label(r, home_dir.as_deref()));
    for (r, label) in records.iter().zip(labels) {
        let level = format!("{}{}", if r.pinned { "*" } else { "" }, r.level.name());
        println!(
            "{:<12}  {:<8}  {:<10}  {:<20}  {:<50}  {}",
            r.id,
            level,
            ago(r.last_activity),
            r.branch.as_deref().unwrap_or("(detached)"),
            truncate(&label, SESSION_WIDTH),
            r.path.display()
        );
    }
    Ok(())
}

/// Widest `SESSION` cell in `bale ls`, in chars.
const SESSION_WIDTH: usize = 50;

/// What `bale ls` names a tree's session: its title when known, else the
/// newest commit subject, else `-`. A live tree costs one or two git
/// processes for the subject, and none when it has a title.
fn session_label(r: &Record, home_dir: Option<&Path>) -> String {
    title(r, home_dir)
        .or_else(|| match r.level {
            Level::Hot | Level::Baled => bale_core::git::latest_subject(&r.path).ok().flatten(),
            _ => r.scope.as_ref().and_then(|s| s.subjects.first().cloned()),
        })
        .unwrap_or_else(|| "-".into())
}

/// The tree's session title, from its origin or its tool.
fn title(r: &Record, home_dir: Option<&Path>) -> Option<String> {
    match home_dir {
        Some(home) => bale_hooks::describe(r, home),
        None => r.origin.as_ref().and_then(|o| o.title.clone()),
    }
}

/// `text` cut to at most `max` chars, ending in `…` when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// The tree's scope and whether it is live: read from git for a hot or
/// baled tree, else the snapshot cold saved, if any.
fn scope_of(r: &Record) -> Option<(Scope, bool)> {
    match r.level {
        Level::Hot | Level::Baled => bale_core::git::scope(&r.path).ok().map(|s| (s, true)),
        _ => r.scope.clone().map(|s| (s, false)),
    }
}

fn cmd_pin(home: &BaleHome, target: &str, pinned: bool) -> Result<ExitCode> {
    let id = resolve(home, target)?;
    set_pinned(home, &id, pinned)?;
    println!("{id}: {}", if pinned { "pinned" } else { "unpinned" });
    Ok(ExitCode::SUCCESS)
}

/// The record for an id or a registered path; never registers anything.
fn lookup(home: &BaleHome, target: &str) -> Result<Record> {
    let registry = Registry::open(home.clone());
    if let Some(r) = registry.get(target)? {
        return Ok(r);
    }
    let path = std::path::absolute(target)?;
    registry
        .get(&workspace_id(&path))?
        .with_context(|| format!("{target} is not registered; run `bale adopt` first"))
}

fn cmd_show(home: &BaleHome, target: &str) -> Result<()> {
    let r = lookup(home, target)?;
    let home_dir = install::home_dir().ok();
    print!("{}", show_text(&r, title(&r, home_dir.as_deref())));
    Ok(())
}

/// `bale show`'s text for `r`, with `title` the session title if known.
fn show_text(r: &Record, title: Option<String>) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: &str| out.push_str(&format!("{label:<9} {value}\n"));
    line("id:", &r.id);
    line("path:", &r.path.display().to_string());
    line("level:", r.level.name());
    line("pinned:", if r.pinned { "yes" } else { "no" });
    match &r.origin {
        Some(o) if o.session.is_empty() => line("origin:", &o.tool),
        Some(o) => line("origin:", &format!("{} session {}", o.tool, o.session)),
        None => line("origin:", "unknown"),
    }
    if let Some(title) = title {
        line("title:", &title);
    }
    match scope_of(r) {
        None => out.push_str("scope:    none saved\n"),
        Some((s, live)) => out.push_str(&scope_text(&s, live)),
    }
    out
}

fn scope_text(s: &Scope, live: bool) -> String {
    let mut out = String::from(if live {
        "scope (live from git):\n"
    } else {
        "scope (saved when it went cold):\n"
    });
    let mut line = |label: &str, value: &str| out.push_str(&format!("  {label:<9} {value}\n"));
    let branch = s.branch.as_deref().unwrap_or("(detached)");
    match &s.upstream {
        Some(u) => line(
            "branch:",
            &format!(
                "{branch} (upstream {u}, {} ahead, {} behind)",
                s.ahead, s.behind
            ),
        ),
        None => line("branch:", &format!("{branch} (no upstream)")),
    }
    if s.unpushed > 0 {
        let noun = if s.unpushed == 1 { "commit" } else { "commits" };
        line("unpushed:", &format!("{} {noun} on no remote", s.unpushed));
    }
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(s.last_commit_unix.max(0) as u64);
    line("commit:", &format!("last {}", ago(when)));
    let changes = if s.changed_files == 0 {
        "none uncommitted".to_string()
    } else {
        format!(
            "{} {} changed, {} {}(+), {} {}(-)",
            s.changed_files,
            plural(s.changed_files, "file"),
            s.insertions,
            plural(s.insertions, "insertion"),
            s.deletions,
            plural(s.deletions, "deletion"),
        )
    };
    line("changes:", &changes);
    for subject in &s.subjects {
        out.push_str(&format!("    {subject}\n"));
    }
    out
}

fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
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

    #[test]
    fn show_omits_an_empty_session() {
        let origin = |session: &str| bale_core::registry::Origin {
            tool: "cursor".into(),
            session: session.into(),
            title: None,
        };
        let mut r = Record {
            id: "0123456789ab".into(),
            repo_common_dir: "/nowhere/.git".into(),
            path: "/nowhere/wt".into(),
            admin_dir: "/nowhere/.git/worktrees/wt".into(),
            branch: None,
            head: String::new(),
            level: bale_core::registry::Level::Cold,
            last_activity: std::time::SystemTime::UNIX_EPOCH,
            note: None,
            trash: Vec::new(),
            origin: Some(origin("")),
            pinned: false,
            scope: None,
        };
        assert!(show_text(&r, None).contains("origin:   cursor\n"));
        r.origin = Some(origin("s1"));
        assert!(show_text(&r, None).contains("origin:   cursor session s1\n"));
    }

    #[test]
    fn busy_report_without_holders_says_the_tree_changed() {
        assert_eq!(
            busy_report("wt-1", &[], true),
            format!("wt-1: {}\n", bale_hooks::CHANGED_DURING_COLD)
        );
    }

    /// up's empty `Busy` is a rename refused by the OS, not a change.
    #[test]
    fn busy_report_outside_cold_stays_plain() {
        assert_eq!(
            busy_report("wt-1", &[], false),
            "wt-1: busy, left untouched\n"
        );
    }

    #[test]
    fn busy_report_lists_holders() {
        let holder = bale_core::busy::Holder {
            pid: 7,
            command: "gradle".into(),
            path: PathBuf::from("/w/t"),
        };
        assert_eq!(
            busy_report("wt-1", &[holder], true),
            "wt-1: busy, left untouched\n  pid 7 gradle /w/t\n"
        );
    }

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
            checks: vec![("d".into(), PathBuf::from("/w/.bale-check-d-a1B2c3"))],
            errors: vec![("c".into(), "boom\nmore".into())],
        };
        assert_eq!(
            gc_lines(&report, false),
            vec![
                (false, "a: removed".to_string()),
                (false, "b: unlocked".to_string()),
                (false, "b: cleaned".to_string()),
                (
                    false,
                    "d: check dir /w/.bale-check-d-a1B2c3 removed".to_string()
                ),
                (true, "bale: c: boom more".to_string()),
            ]
        );
        assert_eq!(gc_lines(&report, true)[0].1, "a: would be removed");
        assert_eq!(
            gc_lines(&report, true)[3].1,
            "d: check dir /w/.bale-check-d-a1B2c3 would be removed"
        );
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
