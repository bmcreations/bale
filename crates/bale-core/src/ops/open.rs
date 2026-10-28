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
use crate::ops::cold::{hex, sha_file, SavedFile, FILES};
use crate::ops::disk::{
    apply_manifest, checkout_wip, ensure_checkout, file_mode, read_manifest, Manifest,
};
use crate::ops::up::{remove_entry, Outcome};
use crate::registry::{canonical_or_ancestor, is_workspace_id, Level, Record, Registry};
use crate::BaleHome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReport {
    pub outcome: Outcome,
    /// Why HEAD was left detached instead of on the recorded branch.
    pub detached_reason: Option<String>,
    /// Things the user should know or do next, one line each.
    pub notes: Vec<String>,
}

impl OpenReport {
    fn plain(outcome: Outcome) -> OpenReport {
        OpenReport {
            outcome,
            detached_reason: None,
            notes: Vec::new(),
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
        git::unlock_worktree_if_ours(&path, &path)?;
        delete_saved_state(home, &record)
    })();
    cleanup.with_context(|| format!("{} is open, but finishing up failed", path.display()))?;

    let mut notes = Vec::new();
    if saved.has_changes {
        notes.push("uncommitted changes are restored unstaged".to_string());
    }
    if let Some(note) = submodule_note(&path) {
        notes.push(note);
    }
    // Best effort and last: a failure here leaves a note, not an error.
    if let Some(sdk) = sdk_candidate(|k| std::env::var_os(k), dirs::home_dir()) {
        match rewrite_sdk_dir(&path.join("local.properties"), &sdk) {
            Ok(true) => notes.push(format!(
                "local.properties: sdk.dir now points at {}",
                sdk.display()
            )),
            Ok(false) => {}
            Err(e) => notes.push(format!("local.properties: sdk.dir not updated: {e:#}")),
        }
    }
    Ok(OpenReport {
        outcome: Outcome::Done(Level::Hot),
        detached_reason,
        notes,
    })
}

/// The commits and files a restore needs, checked before anything is created.
struct Saved {
    head: String,
    wip: String,
    /// The wip tree differs from the head tree.
    has_changes: bool,
    manifest: Manifest,
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
    let manifest = read_manifest(&dest).context("reading the manifest")?;
    for rel in manifest
        .modes
        .iter()
        .chain(&manifest.dir_modes)
        .map(|m| &m.rel)
        .chain(&manifest.dirs)
    {
        if !is_plain_relative(rel) {
            bail!("manifest entry {} is not a relative path", rel.display());
        }
    }
    for f in &manifest.files {
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
    let tree = |rev: &str| -> Result<String> {
        Ok(Git::at(&record.repo_common_dir)
            .run(&["rev-parse", &format!("{rev}^{{tree}}")])?
            .trim()
            .to_string())
    };
    let has_changes = tree(&head)? != tree(&wip)?;
    Ok(Saved {
        head,
        wip,
        has_changes,
        manifest,
    })
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

    let foreign = differences(path, saved, true)?;
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
    match fs::symlink_metadata(&dot_git) {
        // Present only on a resume, after `check_resumable` accepted it.
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dot_git)
                .with_context(|| format!("creating {}", dot_git.display()))?;
            f.write_all(git_file_line(record).as_bytes())?;
            f.sync_all()?;
        }
        Err(e) => return Err(e).with_context(|| format!("checking {}", dot_git.display())),
    }

    // Nothing below may run in any other repository.
    ensure_checkout(path, &record.admin_dir)?;
    let g = Git::at(path);
    // Check out with HEAD as cold left it (on the recorded branch, or
    // detached), as cold's check did: config can depend on the branch.
    match &record.branch {
        Some(branch) => g.run(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])?,
        None => g.run(&["update-ref", "--no-deref", "HEAD", &saved.head])?,
    };
    checkout_wip(&g, &saved.wip)?;
    let files = home.saved_dir(&record.id).join(FILES);
    apply_manifest(path, &saved.manifest, &files)?;
    // Verified under the same HEAD too; below only the index changes.
    verify_restore(record, saved)?;
    // Detach before `reset`: with HEAD still symbolic it would move the branch.
    g.run(&["update-ref", "--no-deref", "HEAD", &saved.head])?;
    g.run(&["reset", "-q", &saved.head])?;
    restage_ignored_additions(&g, saved)
}

/// `reset` unstages everything, which turns a staged, force-added ignored
/// file into a plain ignored one that the next cold would not keep. Adds
/// those back to the index.
fn restage_ignored_additions(g: &Git, saved: &Saved) -> Result<()> {
    let added = g.run(&[
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        "--diff-filter=A",
        &saved.head,
        &saved.wip,
    ])?;
    let added: Vec<String> = added
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(|p| format!(":(literal){p}"))
        .collect();
    let mut ignored = Vec::new();
    for chunk in added.chunks(200) {
        let mut args = vec![
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
        ];
        args.extend(chunk.iter().map(String::as_str));
        let out = g.run(&args)?;
        ignored.extend(
            out.split('\0')
                .filter(|p| !p.is_empty())
                .map(|p| format!(":(literal){p}")),
        );
    }
    for chunk in ignored.chunks(200) {
        let mut args = vec!["add", "-f", "--"];
        args.extend(chunk.iter().map(String::as_str));
        g.run(&args)?;
    }
    Ok(())
}

/// Every saved file, dir and mode matches the manifest, and every other path
/// matches the wip tree.
fn verify_restore(record: &Record, saved: &Saved) -> Result<()> {
    // Raw bytes against what cold's check read: git config that changed
    // since (a filter, eol rules) would otherwise pass git's own diff.
    for h in &saved.manifest.hashes {
        let path = record.path.join(&h.rel);
        let is_file = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
        if !is_file || sha_file(&path)? != h.sha256 {
            bail!(
                "restored {} differs from the file that went cold (did git config change since?)",
                h.rel.display()
            );
        }
    }
    for l in &saved.manifest.links {
        let path = record.path.join(&l.rel);
        let is_link = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link || fs::read_link(&path)? != l.target {
            bail!(
                "restored {} is not the symlink to {} that went cold (did git config change since?)",
                l.rel.display(),
                l.target.display()
            );
        }
    }
    for f in &saved.manifest.files {
        check_entry(&record.path.join(&f.rel), f)
            .with_context(|| format!("restored {} does not match", f.rel.display()))?;
    }
    for d in &saved.manifest.dirs {
        if !fs::symlink_metadata(record.path.join(d)).is_ok_and(|m| m.is_dir()) {
            bail!("restored dir {} is missing", d.display());
        }
    }
    for m in &saved.manifest.modes {
        let meta = fs::symlink_metadata(record.path.join(&m.rel))
            .with_context(|| format!("restored {} is missing", m.rel.display()))?;
        if file_mode(&meta).is_some_and(|mode| mode != m.mode) {
            bail!(
                "restored {} has mode {:o}, not {:o}",
                m.rel.display(),
                file_mode(&meta).unwrap_or(0),
                m.mode
            );
        }
    }
    let root_mode = saved.manifest.root_mode.map(|mode| (Path::new(""), mode));
    for (rel, mode) in saved
        .manifest
        .dir_modes
        .iter()
        .map(|m| (m.rel.as_path(), m.mode))
        .chain(root_mode)
    {
        let meta = fs::symlink_metadata(record.path.join(rel))
            .with_context(|| format!("restored dir {} is missing", rel.display()))?;
        if file_mode(&meta).is_some_and(|m| m != mode) {
            bail!(
                "restored dir {} has mode {:o}, not {:o}",
                rel.display(),
                file_mode(&meta).unwrap_or(0),
                mode
            );
        }
    }
    let differ = differences(&record.path, saved, false)?;
    if !differ.is_empty() {
        bail!(
            "restored tree differs from refs/bale/{}/wip and the manifest: {}",
            record.id,
            differ.join(", ")
        );
    }
    Ok(())
}

/// Paths under `path` that differ from the wip tree and are not a saved
/// file matching the manifest. Judged by the wip's own path list, never the
/// ignore rules in force now, which can change while the tree is cold. A
/// wip file not on disk counts only when `missing_ok` is false.
fn differences(path: &Path, saved: &Saved, missing_ok: bool) -> Result<Vec<String>> {
    let tmp = tempfile::TempDir::new().context("creating a temporary index dir")?;
    let g = Git::at(path).env("GIT_INDEX_FILE", tmp.path().join("index").as_os_str());
    g.run(&["read-tree", &saved.wip])?;
    g.run(&["update-index", "-q", "--refresh"])?;
    let mut differ = Vec::new();
    let changed = g.run(&["diff-files", "--name-status", "-z"])?;
    let mut fields = changed.split('\0').filter(|s| !s.is_empty());
    while let (Some(status), Some(name)) = (fields.next(), fields.next()) {
        if !(missing_ok && status == "D") {
            differ.push(name.to_string());
        }
    }
    // No --exclude-standard: every path outside the wip tree is listed.
    let others = g.run(&["ls-files", "-z", "--others"])?;
    for name in others.split('\0').filter(|s| !s.is_empty()) {
        let rel = Path::new(name);
        let known = saved
            .manifest
            .files
            .iter()
            .find(|f| f.rel == rel)
            .is_some_and(|f| check_entry(&path.join(rel), f).is_ok());
        if !known {
            differ.push(name.to_string());
        }
    }
    Ok(differ)
}

/// A note naming submodules `read-tree` left unchecked-out, if any.
fn submodule_note(path: &Path) -> Option<String> {
    let out = Git::at(path).run(&["submodule", "status"]).ok()?;
    let missing: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix('-'))
        .filter_map(|l| l.split_once(' ').map(|(_, rest)| rest.trim()))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "submodules not checked out ({}): run `git submodule update --init`",
        missing.join(", ")
    ))
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
    delete_saved(home, &Git::at(&record.path), &record.id)
}

/// Deletes `refs/bale/<id>/*`, running git with `g`, then `$BALE_HOME/<id>`.
pub(crate) fn delete_saved(home: &BaleHome, g: &Git, id: &str) -> Result<()> {
    for r in saved_refs(g, id)? {
        g.run(&["update-ref", "-d", &r])?;
    }
    delete_saved_dir(home, id)
}

/// The refs under exactly `refs/bale/<id>/`, listed with `g`. Errs when `id`
/// is not a workspace id, so no broader pattern is ever passed to git.
pub(crate) fn saved_refs(g: &Git, id: &str) -> Result<Vec<String>> {
    if !is_workspace_id(id) {
        bail!("{id:?} is not a workspace id");
    }
    let prefix = format!("refs/bale/{id}/");
    let refs = g.run(&["for-each-ref", "--format=%(refname)", &prefix])?;
    Ok(refs
        .lines()
        .filter(|l| l.starts_with(&prefix))
        .map(str::to_string)
        .collect())
}

/// Removes `$BALE_HOME/<id>`; absent is fine. Errs when `id` is not a
/// workspace id, so it never resolves to bale's own state dirs.
pub(crate) fn delete_saved_dir(home: &BaleHome, id: &str) -> Result<()> {
    if !is_workspace_id(id) {
        bail!("{id:?} is not a workspace id");
    }
    let dest = home.saved_dir(id);
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
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(out.as_bytes())?;
    tmp.as_file().sync_all()?;
    fs::set_permissions(tmp.path(), fs::metadata(file)?.permissions())?;
    tmp.persist(file)
        .with_context(|| format!("writing {}", file.display()))?;
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
