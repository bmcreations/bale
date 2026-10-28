//! `bale open`: restore a cold workspace at its original path from
//! `refs/bale/<id>/` and `$BALE_HOME/<id>/`, then remove the saved state.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::git::{self, Git};
use crate::lock::WorkspaceLock;
use crate::ops::cold::{
    copy_file, copy_symlink, create_private, hex, sha_file, SavedFile, FILES, MANIFEST,
};
use crate::ops::up::{remove_entry, Outcome};
use crate::registry::{canonical_or_ancestor, Level, Record, Registry};
use crate::BaleHome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReport {
    pub outcome: Outcome,
    /// Why HEAD was left detached instead of on the recorded branch.
    pub detached_reason: Option<String>,
}

impl OpenReport {
    fn plain(outcome: Outcome) -> OpenReport {
        OpenReport {
            outcome,
            detached_reason: None,
        }
    }
}

/// Restores a cold workspace at its path, verifies it, then deletes the
/// saved state. `Hot` is a no-op; `Baled` only unlocks. A record left
/// `Opening` by a crash resumes when the path holds nothing but what open
/// itself writes.
pub fn open(home: &BaleHome, id: &str) -> Result<OpenReport> {
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(OpenReport::plain(Outcome::Skipped("locked".into())));
    };
    let registry = Registry::open(home.clone());
    let Some(mut record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    match record.level {
        Level::Hot => return Ok(OpenReport::plain(Outcome::Done(Level::Hot))),
        Level::Baled => {
            // Unlock first: if it fails the record stays Baled and a rerun retries.
            git::unlock_worktree_if_ours(&record.path, &record.path)?;
            record.level = Level::Hot;
            record.last_activity = SystemTime::now();
            registry.put(&record)?;
            return Ok(OpenReport::plain(Outcome::Done(Level::Hot)));
        }
        Level::Cooling => {
            bail!("{id} was interrupted while going cold; run `bale cold {id}` to finish it first")
        }
        Level::Baling => {
            bail!("{id} was interrupted while baling; run `bale up {id}` to finish it first")
        }
        Level::Cold | Level::Opening => {}
    }

    let saved = check_preconditions(home, &record)?;
    let path = record.path.clone();
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("checking {}", path.display())),
        Ok(m) if m.file_type().is_symlink() => bail!(
            "{} is a symlink; move it away, then run `bale open {id}` again",
            path.display()
        ),
        Ok(m) if m.is_dir() && is_empty_dir(&path)? => {}
        Ok(_) if record.level == Level::Opening => check_resumable(&record, &saved)?,
        Ok(_) => bail!(
            "{} exists and is not empty; move it away, then run `bale open {id}` again",
            path.display()
        ),
    }

    if record.level != Level::Opening {
        record.level = Level::Opening;
        registry.put(&record)?;
    }
    restore(home, &record, &saved)?;
    let detached_reason = attach_branch(&record)?;

    if detached_reason.is_some() {
        record.branch = None;
    }
    record.level = Level::Hot;
    record.last_activity = SystemTime::now();
    registry.put(&record)?;

    // The restore is verified and the record is Hot; nothing below can lose work.
    let cleanup = (|| -> Result<()> {
        if let Some(sdk) = sdk_candidate(|k| std::env::var_os(k), dirs::home_dir()) {
            rewrite_sdk_dir(&path.join("local.properties"), &sdk)?;
        }
        git::unlock_worktree_if_ours(&path, &path)?;
        delete_saved_state(home, &record)
    })();
    cleanup.with_context(|| format!("{} is open, but finishing up failed", path.display()))?;
    Ok(OpenReport {
        outcome: Outcome::Done(Level::Hot),
        detached_reason,
    })
}

/// The commits and files a restore needs, checked before anything is created.
struct Saved {
    head: String,
    wip: String,
    files: Vec<SavedFile>,
}

/// Everything open relies on must exist before the path is touched: both
/// refs, the manifest (with every saved copy matching it), and an admin dir
/// whose `gitdir` points back at `<path>/.git`.
fn check_preconditions(home: &BaleHome, record: &Record) -> Result<Saved> {
    let id = &record.id;
    let resolve = |name: &str| -> Result<String> {
        let r = format!("refs/bale/{id}/{name}");
        Git::at(&record.repo_common_dir)
            .run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{r}^{{commit}}"),
            ])
            .map(|s| s.trim().to_string())
            .with_context(|| {
                format!(
                    "{r} does not resolve in {}; nothing to open",
                    record.repo_common_dir.display()
                )
            })
    };
    let head = resolve("head")?;
    let wip = resolve("wip")?;

    let dest = home.saved_dir(id);
    let manifest = dest.join(MANIFEST);
    let text = fs::read_to_string(&manifest)
        .with_context(|| format!("reading the manifest {}", manifest.display()))?;
    let files: Vec<SavedFile> = serde_json::from_str(&text)
        .with_context(|| format!("parsing the manifest {}", manifest.display()))?;
    for f in &files {
        if !is_plain_relative(&f.rel) {
            bail!("manifest entry {} is not a relative path", f.rel.display());
        }
        check_entry(&dest.join(FILES).join(&f.rel), f)
            .with_context(|| format!("saved copy of {} is damaged", f.rel.display()))?;
    }

    let admin = &record.admin_dir;
    if !admin.is_dir() {
        bail!("git admin dir {} is missing", admin.display());
    }
    let gitdir_file = admin.join("gitdir");
    let gitdir = fs::read_to_string(&gitdir_file)
        .with_context(|| format!("reading {}", gitdir_file.display()))?;
    let gitdir = PathBuf::from(gitdir.trim_end_matches(['\n', '\r']));
    let want = record.path.join(".git");
    if canonical_or_ancestor(&gitdir) != canonical_or_ancestor(&want) {
        bail!(
            "{} points at {}, not {}",
            gitdir_file.display(),
            gitdir.display(),
            want.display()
        );
    }
    Ok(Saved { head, wip, files })
}

fn is_plain_relative(rel: &Path) -> bool {
    rel.components().next().is_some() && rel.components().all(|c| matches!(c, Component::Normal(_)))
}

fn is_empty_dir(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)
        .with_context(|| format!("reading {}", path.display()))?
        .next()
        .is_none())
}

/// Errors unless `path` holds exactly what `f` describes: a regular file with
/// its hash, or a symlink with its target. Never follows a link.
fn check_entry(path: &Path, f: &SavedFile) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    match &f.symlink_target {
        Some(target) => {
            let actual = fs::read_link(path).ok();
            if !meta.file_type().is_symlink()
                || actual.as_ref() != Some(target)
                || hex(&Sha256::digest(target.to_string_lossy().as_bytes())) != f.sha256
            {
                bail!(
                    "{} is not the saved symlink to {}",
                    path.display(),
                    target.display()
                );
            }
        }
        None => {
            if !meta.file_type().is_file() || sha_file(path)? != f.sha256 {
                bail!("{} does not match its saved hash", path.display());
            }
        }
    }
    Ok(())
}

fn git_file_line(record: &Record) -> String {
    format!("gitdir: {}\n", record.admin_dir.display())
}

/// A resumed open may continue only when `path` holds a subset of what open
/// writes: our `.git` file, files equal to the wip tree, and saved files that
/// match the manifest.
fn check_resumable(record: &Record, saved: &Saved) -> Result<()> {
    let path = &record.path;
    let dot_git = path.join(".git");
    let meta = fs::symlink_metadata(&dot_git).with_context(|| {
        format!(
            "{} has content but no .git from bale open; move it away, then rerun",
            path.display()
        )
    })?;
    let ours = meta.file_type().is_file()
        && fs::read_to_string(&dot_git).ok().as_deref() == Some(git_file_line(record).as_str());
    if !ours {
        bail!(
            "{} is not the .git file bale open writes; move {} away, then rerun",
            dot_git.display(),
            path.display()
        );
    }

    let tmp = tempfile::TempDir::new().context("creating a temporary index dir")?;
    let g = Git::at(path).env("GIT_INDEX_FILE", tmp.path().join("index").as_os_str());
    g.run(&["read-tree", &saved.wip])?;
    g.run(&["update-index", "-q", "--refresh"])?;
    let mut foreign = Vec::new();
    let changed = g.run(&["diff-files", "--name-status", "-z"])?;
    let mut fields = changed.split('\0').filter(|s| !s.is_empty());
    while let (Some(status), Some(name)) = (fields.next(), fields.next()) {
        // Not written yet is fine; anything else differs from the wip tree.
        if status != "D" {
            foreign.push(name.to_string());
        }
    }
    let others = g.run(&["ls-files", "-z", "--others"])?;
    for name in others.split('\0').filter(|s| !s.is_empty()) {
        let rel = Path::new(name);
        let known = saved
            .files
            .iter()
            .find(|f| f.rel == rel)
            .is_some_and(|f| check_entry(&path.join(rel), f).is_ok());
        if !known {
            foreign.push(name.to_string());
        }
    }
    if !foreign.is_empty() {
        bail!(
            "{} holds files bale open did not write ({}); move them away, then rerun",
            path.display(),
            foreign.join(", ")
        );
    }
    Ok(())
}

/// Steps 1-4 and their verification: dir, `.git`, detached HEAD at the saved
/// head, the wip tree as unstaged changes, then the saved files.
fn restore(home: &BaleHome, record: &Record, saved: &Saved) -> Result<()> {
    let path = &record.path;
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    let dot_git = path.join(".git");
    if fs::symlink_metadata(&dot_git).is_err() {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dot_git)
            .with_context(|| format!("creating {}", dot_git.display()))?;
        f.write_all(git_file_line(record).as_bytes())?;
        f.sync_all()?;
    }

    let g = Git::at(path);
    // Detach first: with HEAD still symbolic, `reset` would move the branch.
    g.run(&["update-ref", "--no-deref", "HEAD", &saved.head])?;
    g.run(&["read-tree", "-u", "--reset", &saved.wip])?;
    g.run(&["reset", "-q", &saved.head])?;

    let files = home.saved_dir(&record.id).join(FILES);
    for f in &saved.files {
        let to = path.join(&f.rel);
        if fs::symlink_metadata(&to).is_ok() {
            // Written by an earlier, interrupted open; checked below.
            continue;
        }
        if let Some(parent) = f.rel.parent() {
            make_parents(path, parent)?;
        }
        let from = files.join(&f.rel);
        if f.symlink_target.is_some() {
            copy_symlink(&from, &to, f.rel.clone())?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    verify_restore(record, saved)
}

/// Creates `rel` under `root` one component at a time, refusing to pass
/// through anything that is not a real directory.
fn make_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{} is in the way of a saved file", cur.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cur).with_context(|| format!("creating {}", cur.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("checking {}", cur.display())),
        }
    }
    Ok(())
}

/// Every saved file matches its manifest entry and the working tree builds
/// exactly the wip tree.
fn verify_restore(record: &Record, saved: &Saved) -> Result<()> {
    for f in &saved.files {
        check_entry(&record.path.join(&f.rel), f)
            .with_context(|| format!("restored {} does not match", f.rel.display()))?;
    }
    let tmp = tempfile::TempDir::new().context("creating a temporary index dir")?;
    let g = Git::at(&record.path).env("GIT_INDEX_FILE", tmp.path().join("index").as_os_str());
    g.run(&["read-tree", &saved.wip])?;
    g.run(&["add", "-A"])?;
    let rebuilt = g.run(&["write-tree"])?;
    let want = g.run(&["rev-parse", &format!("{}^{{tree}}", saved.wip)])?;
    if rebuilt.trim() != want.trim() {
        bail!(
            "restored tree {} is not refs/bale/{}/wip's {}",
            rebuilt.trim(),
            record.id,
            want.trim()
        );
    }
    Ok(())
}

/// Puts HEAD back on the recorded branch when that is safe; otherwise
/// returns why it stays detached.
fn attach_branch(record: &Record) -> Result<Option<String>> {
    let Some(branch) = &record.branch else {
        return Ok(None);
    };
    let path = &record.path;
    let g = Git::at(path);
    let full = format!("refs/heads/{branch}");
    let head = g.run(&["rev-parse", "--verify", "HEAD"])?;
    let tip = match g.run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{full}^{{commit}}"),
    ]) {
        Ok(t) => t,
        Err(_) => return Ok(Some(format!("branch {branch} no longer exists"))),
    };
    if tip.trim() != head.trim() {
        return Ok(Some(format!(
            "branch {branch} moved to {} since it went cold (was {})",
            short(&tip),
            short(&head)
        )));
    }
    let here = canonical_or_ancestor(path);
    if let Some(other) = git::worktrees(path)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch) && canonical_or_ancestor(&w.path) != here)
    {
        return Ok(Some(format!(
            "branch {branch} is checked out in {}",
            other.path.display()
        )));
    }
    g.run(&["symbolic-ref", "HEAD", &full])?;
    Ok(None)
}

fn short(sha: &str) -> &str {
    let sha = sha.trim();
    &sha[..sha.len().min(12)]
}

/// Deletes every `refs/bale/<id>/*` ref and `$BALE_HOME/<id>`.
fn delete_saved_state(home: &BaleHome, record: &Record) -> Result<()> {
    let g = Git::at(&record.path);
    let refs = g.run(&[
        "for-each-ref",
        "--format=%(refname)",
        &format!("refs/bale/{}/", record.id),
    ])?;
    for r in refs.lines().filter(|l| !l.is_empty()) {
        g.run(&["update-ref", "-d", r])?;
    }
    let dest = home.saved_dir(&record.id);
    match remove_entry(&dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", dest.display())),
    }
}

/// `ANDROID_HOME`, else `ANDROID_SDK_ROOT`, else the platform's default SDK
/// location under `home`.
fn sdk_candidate(env: impl Fn(&str) -> Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    for key in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(v) = env(key).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(v));
        }
    }
    if cfg!(windows) {
        return env("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join("Android").join("Sdk"));
    }
    let home = home?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Android/sdk"))
    } else {
        Some(home.join("Android/Sdk"))
    }
}

/// Points `sdk.dir` in a `local.properties` at `sdk` when the recorded path
/// is gone and `sdk` exists. Returns whether it rewrote the file. A missing
/// file, a symlink, or a file without `sdk.dir` is left alone.
fn rewrite_sdk_dir(file: &Path, sdk: &Path) -> Result<bool> {
    match fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("checking {}", file.display())),
    }
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let mut out = String::with_capacity(text.len());
    let mut rewrote = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        if let Some(old) = sdk_value(body) {
            if !rewrote && !Path::new(&old).exists() && sdk.exists() {
                out.push_str("sdk.dir=");
                out.push_str(&escape_property(&sdk.to_string_lossy()));
                out.push_str(ending);
                rewrote = true;
                continue;
            }
        }
        out.push_str(line);
    }
    if !rewrote {
        return Ok(false);
    }
    let dir = file.parent().context("local.properties has no parent")?;
    let tmp = dir.join("local.properties.bale-tmp");
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", tmp.display())),
    }
    let mut f = create_private(&tmp)?;
    f.write_all(out.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::set_permissions(&tmp, fs::metadata(file)?.permissions())?;
    fs::rename(&tmp, file).with_context(|| format!("writing {}", file.display()))?;
    Ok(true)
}

/// The unescaped value of an `sdk.dir` line (`=`, `:` or whitespace separated).
fn sdk_value(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("sdk.dir")?;
    // `sdk.dirx=...` is another key.
    if !rest.starts_with([' ', '\t', '=', ':']) {
        return None;
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let rest = rest
        .strip_prefix(['=', ':'])
        .unwrap_or(rest)
        .trim_start_matches([' ', '\t']);
    let mut value = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => value.push('\t'),
                Some('n') => value.push('\n'),
                Some(other) => value.push(other),
                None => {}
            }
        } else {
            value.push(c);
        }
    }
    Some(value)
}

/// Escapes a properties value the way Android Studio writes `sdk.dir`.
fn escape_property(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ':' => out.push_str("\\:"),
            '=' => out.push_str("\\="),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("local.properties");
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn rewrites_when_old_is_missing_and_new_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = tmp.path().join("sdk");
        fs::create_dir(&sdk).unwrap();
        let p = props(tmp.path(), "# c\nsdk.dir=/nowhere/sdk\nndk.dir=x\n");
        assert!(rewrite_sdk_dir(&p, &sdk).unwrap());
        let want = format!(
            "# c\nsdk.dir={}\nndk.dir=x\n",
            escape_property(&sdk.to_string_lossy())
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), want);
    }

    #[test]
    fn leaves_file_when_old_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let old = tmp.path().join("old");
        let sdk = tmp.path().join("sdk");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&sdk).unwrap();
        let body = format!("sdk.dir={}\n", escape_property(&old.to_string_lossy()));
        let p = props(tmp.path(), &body);
        assert!(!rewrite_sdk_dir(&p, &sdk).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), body);
    }

    #[test]
    fn leaves_file_when_new_is_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = props(tmp.path(), "sdk.dir=/nowhere/sdk\n");
        assert!(!rewrite_sdk_dir(&p, &tmp.path().join("also-nowhere")).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), "sdk.dir=/nowhere/sdk\n");
    }

    #[test]
    fn missing_file_and_other_keys_are_left_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!rewrite_sdk_dir(&tmp.path().join("local.properties"), tmp.path()).unwrap());
        let p = props(tmp.path(), "sdk.dirx=/nowhere\n");
        assert!(!rewrite_sdk_dir(&p, tmp.path()).unwrap());
    }

    #[test]
    fn reads_escaped_windows_values() {
        assert_eq!(
            sdk_value(r"sdk.dir=C\:\\Users\\me\\Sdk").as_deref(),
            Some(r"C:\Users\me\Sdk")
        );
        assert_eq!(sdk_value("sdk.dir : /a b").as_deref(), Some("/a b"));
        assert_eq!(sdk_value("sdk.dirx=/a"), None);
        assert_eq!(escape_property(r"C:\Users\me"), r"C\:\\Users\\me");
    }

    #[test]
    fn sdk_candidate_prefers_env_then_default() {
        let home = Some(PathBuf::from("/h"));
        let env = |k: &str| match k {
            "ANDROID_HOME" => Some(OsString::from("/a")),
            "ANDROID_SDK_ROOT" => Some(OsString::from("/r")),
            _ => None,
        };
        assert_eq!(sdk_candidate(env, home.clone()), Some(PathBuf::from("/a")));
        let root_only = |k: &str| (k == "ANDROID_SDK_ROOT").then(|| OsString::from("/r"));
        assert_eq!(
            sdk_candidate(root_only, home.clone()),
            Some(PathBuf::from("/r"))
        );
        let local = |k: &str| (k == "LOCALAPPDATA").then(|| OsString::from("/l"));
        let default = sdk_candidate(local, home);
        if cfg!(windows) {
            assert_eq!(
                default,
                Some(PathBuf::from("/l").join("Android").join("Sdk"))
            );
        } else if cfg!(target_os = "macos") {
            assert_eq!(default, Some(PathBuf::from("/h/Library/Android/sdk")));
        } else {
            assert_eq!(default, Some(PathBuf::from("/h/Android/Sdk")));
        }
    }
}
