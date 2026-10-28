//! Install plans: every effect of `bale install` is first built as a
//! [`Plan`], which `--print-only` prints and a real run applies, so the two
//! cannot drift apart. The tool modules build plans that edit their config;
//! the CLI builds the scheduler's.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::display_path;

/// One effect of an install.
pub enum Step {
    Mkdir(PathBuf),
    Write {
        path: PathBuf,
        content: String,
        executable: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Deletes a file; one already gone counts as deleted.
    Remove(PathBuf),
    /// Rewrites a config file atomically. With `backup`, set when install
    /// adds bale to a file that had no bale entry, the file as it is now
    /// replaces any older backup there (one that does not exist yet removes
    /// it): serde_json reformats a JSON file, and the copy keeps the layout,
    /// and uninstall reads a backup's presence as "the user made this file".
    Settings {
        path: PathBuf,
        before: Option<String>,
        after: String,
        /// The original file's permissions, restored after the rewrite.
        mode: Option<fs::Permissions>,
        backup: Option<PathBuf>,
    },
    Run {
        args: Vec<String>,
        ignore_failure: bool,
        /// Total tries, 500 ms apart, before a failure counts.
        attempts: u32,
    },
    Note(String),
}

#[derive(Default)]
pub struct Plan(Vec<Step>);

impl Plan {
    pub fn new(steps: Vec<Step>) -> Plan {
        Plan(steps)
    }

    /// Prints what applying would do.
    pub fn print(&self) {
        for step in &self.0 {
            match step {
                Step::Mkdir(p) => println!("mkdir -p {}", display_path(p)),
                Step::Write { path, content, .. } => {
                    println!("write {}:", display_path(path));
                    for line in content.lines() {
                        println!("    {line}");
                    }
                }
                Step::Rename { from, to } => {
                    println!("mv {} {}", display_path(from), display_path(to))
                }
                Step::Remove(p) => println!("rm {}", display_path(p)),
                Step::Settings {
                    path,
                    before,
                    after,
                    backup,
                    ..
                } => {
                    println!("update {}:", display_path(path));
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    if let Some(note) = reformat_note(path, before, backup) {
                        println!("{note}");
                    }
                }
                Step::Run {
                    args,
                    ignore_failure,
                    attempts,
                } => {
                    let tail = if *ignore_failure {
                        "  (failure ignored)".to_string()
                    } else if *attempts > 1 {
                        format!("  (up to {attempts} tries)")
                    } else {
                        String::new()
                    };
                    println!("run: {}{tail}", shell_words(args));
                }
                Step::Note(n) => println!("{n}"),
            }
        }
    }

    /// Carries out every step, stopping at the first failure.
    pub fn apply(&self) -> Result<()> {
        for step in &self.0 {
            match step {
                Step::Mkdir(p) => {
                    fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?
                }
                Step::Write {
                    path,
                    content,
                    executable,
                } => {
                    write_atomic(path, content)?;
                    if *executable {
                        make_executable(path)?;
                    }
                    println!("wrote {}", display_path(path));
                }
                Step::Rename { from, to } => {
                    fs::rename(from, to).with_context(|| {
                        format!("renaming {} to {}", from.display(), to.display())
                    })?;
                    println!("moved {} to {}", display_path(from), display_path(to));
                }
                Step::Remove(p) => {
                    match fs::remove_file(p) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                            return Err(e).with_context(|| format!("deleting {}", p.display()))
                        }
                        _ => {}
                    }
                    println!("removed {}", display_path(p));
                }
                Step::Settings {
                    path,
                    before,
                    after,
                    mode,
                    backup,
                } => {
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    if let Some(note) = reformat_note(path, before, backup) {
                        println!("{note}");
                    }
                    match (before, backup) {
                        (Some(_), Some(backup)) => {
                            if let Some(dir) = backup.parent() {
                                fs::create_dir_all(dir)
                                    .with_context(|| format!("creating {}", dir.display()))?;
                            }
                            fs::copy(path, backup)
                                .with_context(|| format!("backing up {}", path.display()))?;
                        }
                        // bale creates the file: a backup left from an
                        // earlier file at this path no longer describes it,
                        // but may be the user's only copy of that file.
                        (None, Some(backup)) if fs::symlink_metadata(backup).is_ok() => {
                            let old = retired_backup_path(backup);
                            fs::rename(backup, &old)
                                .with_context(|| format!("renaming {}", backup.display()))?;
                            println!(
                                "note: {} keeps the backup of an earlier {}",
                                display_path(&old),
                                display_path(path)
                            );
                        }
                        _ => {}
                    }
                    write_atomic(path, after)?;
                    if let Some(mode) = mode {
                        fs::set_permissions(path, mode.clone())
                            .with_context(|| format!("restoring the mode of {}", path.display()))?;
                    }
                    println!("updated {}", display_path(path));
                }
                Step::Run {
                    args,
                    ignore_failure,
                    attempts,
                } => {
                    let mut tries = 0;
                    let out = loop {
                        tries += 1;
                        let out = Command::new(&args[0])
                            .args(&args[1..])
                            .output()
                            .with_context(|| format!("running {}", shell_words(args)))?;
                        if out.status.success() || tries >= *attempts {
                            break out;
                        }
                        // launchd can still be tearing down the job bootout removed.
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    };
                    if !out.status.success() && !ignore_failure {
                        bail!(
                            "{} failed: {}",
                            shell_words(args),
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                    }
                    println!("ran {}", shell_words(args));
                }
                Step::Note(n) => println!("{n}"),
            }
        }
        Ok(())
    }

    pub fn extend(&mut self, other: Plan) {
        self.0.extend(other.0);
    }
}

/// The path a config's backup is named for: `tail` under `root` (the repo or
/// home) with `root` canonicalized, so a symlinked or `..` spelling of it
/// names the same backup. Only `root` is resolved: the config's own
/// directory may not exist before the first install.
pub(crate) fn backup_key(root: &Path, tail: &Path) -> PathBuf {
    bale_core::paths::canonicalize(root)
        .or_else(|_| std::path::absolute(root))
        .unwrap_or_else(|_| root.to_path_buf())
        .join(tail)
}

/// Whether git tracks `path` in its repo; false when git or a repo is missing.
pub(crate) fn tracked_by_git(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The note that uninstall kept a file git tracks, which it would otherwise
/// have deleted as bale's own.
pub(crate) fn tracked_note(path: &Path) -> Step {
    Step::Note(format!(
        "note: git tracks {}; kept it with bale's entries removed",
        display_path(path)
    ))
}

/// Where install moves a backup that no longer describes the file:
/// `<name>.<unix-secs>.old` beside it.
fn retired_backup_path(backup: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = backup.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{secs}.old"));
    backup.with_file_name(name)
}

/// The note for an entry install rewrote to run this exe: `what` in `file`
/// now runs `new` instead of `old`. A file teammates share flips to the last
/// installer's path, so the note says who loses it.
pub(crate) fn repoint_note(file: &Path, what: &str, old: &str, new: &Path) -> Step {
    let new = display_path(new);
    Step::Note(if old == new {
        format!(
            "note: {}: rewrote {what} for bale at {new}",
            display_path(file)
        )
    } else {
        format!(
            "note: {}: {what} now runs bale at {new} instead of {old}; where bale is only \
             at {old}, the entry is skipped",
            display_path(file)
        )
    })
}

/// Where a config's original is kept: in `backups` (bale's
/// `$BALE_HOME/backups`), named for the sha256 of `key` (from
/// [`backup_key`], or else made absolute), so two repos' files of the same
/// name keep separate backups, then the path's last characters for a
/// reader, short enough for any filesystem.
pub(crate) fn backup_path(backups: &Path, path: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    const TAIL: usize = 80;
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let whole = path.to_string_lossy();
    let hash: String = Sha256::digest(whole.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    // ASCII only, so a char is a byte.
    let readable: Vec<char> = whole
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let tail: String = readable[readable.len().saturating_sub(TAIL)..]
        .iter()
        .collect();
    backups.join(format!("{hash}-{}.bak-bale", tail.trim_start_matches('-')))
}

/// Where installs before backups moved to `$BALE_HOME` kept them: beside `path`.
fn old_backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak-bale");
    path.with_file_name(name)
}

/// The note that install reformats a JSON file and where its original is
/// kept. toml_edit keeps a TOML file's layout, so it gets none.
fn reformat_note(path: &Path, before: &Option<String>, backup: &Option<PathBuf>) -> Option<String> {
    let backup = backup.as_ref().filter(|_| before.is_some())?;
    reformat_text(path, Some(backup))
}

/// The reformat note for a JSON `path`, naming `backup` when there is one.
pub(crate) fn reformat_text(path: &Path, backup: Option<&Path>) -> Option<String> {
    if path.extension().is_none_or(|e| e != "json") {
        return None;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Some(match backup {
        Some(backup) => format!(
            "note: {name} is reformatted (2-space JSON); {} keeps the original",
            display_path(backup)
        ),
        None => format!("note: {name} is reformatted (2-space JSON)"),
    })
}

/// A config file's text and permissions, or `None` when it does not exist.
/// A symlink is refused: bale would replace it with a regular file.
pub(crate) fn read_config(path: &Path) -> Result<Option<(String, fs::Permissions)>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; bale will not replace it with a regular file. Remove \
             bale's entries from the file it points at by hand",
            path.display()
        );
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some((text, meta.permissions())))
}

/// The steps that write back a JSON config with bale's entries removed.
/// `kept` is the config with them removed; `emptied` names the containers
/// (key paths, innermost first) that the removal left empty, which install
/// may have created.
///
/// A backup is install's copy of the file as the user had it, so its
/// presence means the user made the file:
/// - a backup that holds `kept` (with only the emptied containers it lacks
///   left out) is restored byte for byte, and an old one beside the file is
///   then deleted;
/// - with a backup that does not match, the file is rewritten without the
///   emptied containers and kept, even if nothing is left;
/// - with no backup, bale created the file, and deletes it when nothing is
///   left, unless git tracks it.
///
/// `key` is the path the backup is named for (see [`backup_key`]).
pub(crate) fn json_uninstall_steps(
    backups: &Path,
    key: &Path,
    path: &Path,
    before: String,
    mode: fs::Permissions,
    kept: &serde_json::Value,
    emptied: &[Vec<&str>],
) -> Result<Vec<Step>> {
    let settings = |after: String| Step::Settings {
        path: path.to_path_buf(),
        before: Some(before.clone()),
        after,
        mode: Some(mode.clone()),
        backup: None,
    };
    let old = old_backup_path(path);
    let mut any_backup = false;
    for candidate in [backup_path(backups, key), old.clone()] {
        if fs::symlink_metadata(&candidate).is_err() {
            continue;
        }
        any_backup = true;
        let Ok(text) = fs::read_to_string(&candidate) else {
            continue;
        };
        let Ok(original) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if prune(kept, emptied, Some(&original)) == original {
            let mut steps = vec![settings(text)];
            if candidate == old {
                steps.push(Step::Remove(old));
            }
            return Ok(steps);
        }
    }
    let root = prune(kept, emptied, None);
    if !any_backup && root.as_object().is_some_and(|o| o.is_empty()) {
        if !tracked_by_git(path) {
            return Ok(vec![Step::Remove(path.to_path_buf())]);
        }
        // A committed file is the repo's, whoever made it: keep it, and
        // keep the containers the removal emptied.
        return Ok(vec![
            settings(serde_json::to_string_pretty(kept)? + "\n"),
            tracked_note(path),
        ]);
    }
    Ok(vec![settings(serde_json::to_string_pretty(&root)? + "\n")])
}

/// `kept` without the `emptied` containers that are still empty, except
/// those `original` has at the same key path.
fn prune(
    kept: &serde_json::Value,
    emptied: &[Vec<&str>],
    original: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut out = kept.clone();
    for keys in emptied {
        let Some((last, parents)) = keys.split_last() else {
            continue;
        };
        if original.is_some_and(|o| keys.iter().try_fold(o, |v, k| v.get(k)).is_some()) {
            continue;
        }
        let Some(parent) = parents
            .iter()
            .try_fold(&mut out, |v, k| v.get_mut(k))
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let empty = match parent.get(*last) {
            Some(serde_json::Value::Array(a)) => a.is_empty(),
            Some(serde_json::Value::Object(o)) => o.is_empty(),
            _ => false,
        };
        if empty {
            parent.shift_remove(*last);
        }
    }
    out
}

/// Writes `content` to a temp file beside `path`, then renames it over `path`.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))?;
    tmp.write_all(content.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Args joined for display, double-quoting any that need it.
fn shell_words(args: &[String]) -> String {
    shell_words_for(args, cfg!(windows))
}

/// `args` as one command line. An arg with a space, tab or quote is
/// double-quoted. For sh a backslash in it is escaped; on Windows a
/// backslash is literal except in a run before a `"`, which is doubled, so a
/// path prints as typed.
fn shell_words_for(args: &[String], windows: bool) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains([' ', '"', '\'', '\t']) {
                a.clone()
            } else if windows {
                windows_quote(a)
            } else {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `a` double-quoted the way `CommandLineToArgvW` reads it back.
pub(crate) fn windows_quote(a: &str) -> String {
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// A line diff of `before` to `after`: changed lines with two lines of context.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // LCS table; settings files are small.
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    const CONTEXT: usize = 2;
    let near_change = |k: usize| {
        let lo = k.saturating_sub(CONTEXT);
        let hi = (k + CONTEXT + 1).min(ops.len());
        ops[lo..hi].iter().any(|(c, _)| *c != ' ')
    };
    let mut out = String::new();
    let mut skipped = false;
    for (k, (c, line)) in ops.iter().enumerate() {
        if near_change(k) {
            if skipped {
                out.push_str("  ...\n");
                skipped = false;
            }
            out.push_str(&format!("{c} {line}\n"));
        } else {
            skipped = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_marks_added_lines_with_context() {
        let d = diff("a\nb\nc\nd\ne\nf\n", "a\nb\nc\nX\nd\ne\nf\n");
        assert_eq!(d, "  ...\n  b\n  c\n+ X\n  d\n  e\n");
    }

    #[test]
    fn windows_words_keep_path_backslashes() {
        let args = [
            "/TR".to_string(),
            r#""D:\a b\bale.exe" sweep"#.to_string(),
            r"C:\dir with space\".to_string(),
        ];
        assert_eq!(
            shell_words_for(&args, true),
            r#"/TR "\"D:\a b\bale.exe\" sweep" "C:\dir with space\\""#
        );
    }

    #[test]
    fn unix_words_escape_backslashes() {
        let args = [r"a b\c".to_string()];
        assert_eq!(shell_words_for(&args, false), r#""a b\\c""#);
    }

    #[test]
    fn backup_is_named_for_the_whole_path() {
        let b = Path::new("/b/backups");
        let name = |p: &str| {
            backup_path(b, Path::new(p))
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let n = name("/r1/.cursor/worktrees.json");
        assert!(n.ends_with("-r1-.cursor-worktrees.json.bak-bale"), "{n}");
        assert_eq!(backup_path(b, Path::new("/r1/x")).parent(), Some(b));
        assert_ne!(n, name("/r2/.cursor/worktrees.json"));
        // Paths that read the same once punctuation is replaced still differ.
        assert_ne!(name("/a-b/x"), name("/a/b/x"));
        assert_ne!(name("/a b/x"), name("/a/b/x"));
        // A deep path still gives a name the filesystem takes.
        let deep = format!("/{}/settings.json", "d".repeat(400));
        let n = name(&deep);
        assert!(n.len() < 200, "{}", n.len());
        assert!(n.ends_with("settings.json.bak-bale"), "{n}");
        // A relative `--repo` is named for where it is, not how it was typed.
        let rel = Path::new(".cursor/worktrees.json");
        assert_eq!(
            backup_path(b, rel),
            backup_path(b, &std::env::current_dir().unwrap().join(rel))
        );
    }
}
