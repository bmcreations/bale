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
    /// Rewrites a config file atomically. A JSON file, which serde_json
    /// reformats, has `before` backed up to `<name>.bak-bale` the first time;
    /// other formats are edited in place and get no backup.
    Settings {
        path: PathBuf,
        before: Option<String>,
        after: String,
        /// The original file's permissions, restored after the rewrite.
        mode: Option<fs::Permissions>,
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
                Step::Settings {
                    path,
                    before,
                    after,
                    ..
                } => {
                    println!("update {}:", display_path(path));
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    if before.is_some() && is_json(path) {
                        println!("{}", reformat_note(path));
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
                Step::Settings {
                    path,
                    before,
                    after,
                    mode,
                } => {
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    if before.is_some() && is_json(path) {
                        println!("{}", reformat_note(path));
                    }
                    let backup = backup_path(path);
                    if before.is_some() && is_json(path) && fs::symlink_metadata(&backup).is_err() {
                        fs::copy(path, &backup)
                            .with_context(|| format!("backing up {}", path.display()))?;
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

/// Whether `path` is a JSON file, which serde_json rewrites in its own layout.
fn is_json(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
}

/// `<name>.bak-bale` beside `path`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak-bale");
    path.with_file_name(name)
}

fn reformat_note(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    format!("note: {name} is reformatted (2-space JSON); {name}.bak-bale keeps the original")
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
fn windows_quote(a: &str) -> String {
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
    fn backup_sits_beside_the_file() {
        assert_eq!(
            backup_path(Path::new("/h/.claude/settings.json")),
            Path::new("/h/.claude/settings.json.bak-bale")
        );
    }
}
