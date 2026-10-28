#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Pids that `busy_holders*` ignore for the rest of this process.
static EXCLUDED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Makes every later busy check in this process ignore `pids`. A hook uses
/// it for the processes that invoked it (its shell and Claude Code), which
/// hold the tree only because they are waiting on the hook.
pub fn exclude_pids(pids: &[u32]) {
    let mut excluded = EXCLUDED.lock().unwrap_or_else(|e| e.into_inner());
    excluded.extend_from_slice(pids);
}

fn without_excluded(mut lists: Vec<Vec<Holder>>) -> Vec<Vec<Holder>> {
    let excluded = EXCLUDED.lock().unwrap_or_else(|e| e.into_inner());
    if !excluded.is_empty() {
        for list in &mut lists {
            list.retain(|h| !excluded.contains(&h.pid));
        }
    }
    lists
}

/// A process that has its cwd or an open file under the probed root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub command: String,
    pub path: PathBuf,
}

/// On Linux only cwd, open fds and the executable are checked; files held
/// solely by mmap are not detected. Windows returns empty.
///
/// Processes (other than this one) with cwd or an open file under `root`.
/// Windows returns empty: busy is detected by the rename in the cold path.
pub fn busy_holders(root: &Path) -> Result<Vec<Holder>> {
    #[cfg(unix)]
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", root.display()))?;
    #[cfg(not(unix))]
    let root = root.to_path_buf();
    Ok(busy_holders_many(&[root])?.pop().unwrap_or_default())
}

/// `busy_holders` for each of `roots`, in order, from one scan of the
/// process table. Roots must already be canonical (paths are matched as
/// given). Errs only when the scan fails.
pub fn busy_holders_many(roots: &[PathBuf]) -> Result<Vec<Vec<Holder>>> {
    #[cfg(target_os = "linux")]
    let lists = linux::holders(roots)?;
    #[cfg(all(unix, not(target_os = "linux")))]
    let lists = lsof::holders(roots)?;
    #[cfg(not(unix))]
    let lists = vec![Vec::new(); roots.len()];
    Ok(without_excluded(lists))
}

#[cfg(all(unix, not(target_os = "linux")))]
mod lsof {
    use super::*;
    use anyhow::bail;
    use std::process::Command;

    /// Holders per canonical root, from one `lsof` run.
    pub fn holders(roots: &[PathBuf]) -> Result<Vec<Vec<Holder>>> {
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let out = Command::new("lsof")
            .args(["-n", "-w", "-F", "pcfn"])
            .output()
            .context("running lsof (is it installed?)")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        // lsof exits 1 when nothing matched or some entries were unreadable;
        // parse whatever it printed rather than discarding it.
        if !out.status.success() && out.status.code() != Some(1) {
            bail!(
                "lsof failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let own = std::process::id();
        Ok(roots.iter().map(|r| parse(&stdout, r, own)).collect())
    }

    /// Parses `lsof -F pcfn` output. One Holder per pid, first matching path.
    pub(super) fn parse(stdout: &str, root: &Path, own_pid: u32) -> Vec<Holder> {
        let mut result: Vec<Holder> = Vec::new();
        let mut pid: Option<u32> = None;
        let mut command = String::new();
        for line in stdout.lines() {
            let Some(tag) = line.chars().next() else {
                continue;
            };
            let value = &line[tag.len_utf8()..];
            match tag {
                'p' => {
                    pid = value.parse().ok();
                    command.clear();
                }
                'c' => command = value.to_string(),
                'n' => {
                    let Some(p) = pid else { continue };
                    if p == own_pid || result.iter().any(|h| h.pid == p) {
                        continue;
                    }
                    let path = PathBuf::from(value);
                    if path.starts_with(root) {
                        result.push(Holder {
                            pid: p,
                            command: command.clone(),
                            path,
                        });
                    }
                }
                _ => {}
            }
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_cwd_and_files_once_per_pid() {
            let out = "p10\ncsleep\nfcwd\nn/r/app\nf3\nn/r/app/x\np11\ncother\nfcwd\nn/elsewhere\np12\ncme\nf4\nn/r/y\n";
            let h = parse(out, Path::new("/r"), 12);
            assert_eq!(h.len(), 1);
            assert_eq!(h[0].pid, 10);
            assert_eq!(h[0].command, "sleep");
            assert_eq!(h[0].path, PathBuf::from("/r/app"));
        }

        #[test]
        fn open_file_with_cwd_outside_root_is_a_holder() {
            let out = "p20\ncjava\nfcwd\nn/elsewhere\nf5\nn/r/build/out.bin\n";
            let h = parse(out, Path::new("/r"), 1);
            assert_eq!(h.len(), 1);
            assert_eq!(h[0].path, PathBuf::from("/r/build/out.bin"));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;

    fn skippable(e: &std::io::Error) -> bool {
        // ESRCH (process gone) surfaces as NotFound; some kernels report it raw.
        matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound)
            || e.raw_os_error() == Some(3)
    }

    /// Holders per canonical root, from one pass over `/proc`.
    pub fn holders(roots: &[PathBuf]) -> Result<Vec<Vec<Holder>>> {
        let mut result = vec![Vec::new(); roots.len()];
        if roots.is_empty() {
            return Ok(result);
        }
        let own = std::process::id();
        for entry in fs::read_dir("/proc").context("reading /proc")? {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if skippable(&e) => continue,
                Err(e) => return Err(e).context("reading /proc entry"),
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == own {
                continue;
            }
            let targets = link_targets(&entry.path())?;
            let mut command: Option<String> = None;
            for (root, held) in roots.iter().zip(result.iter_mut()) {
                let Some(path) = targets.iter().find(|t| t.starts_with(root)) else {
                    continue;
                };
                let command = command
                    .get_or_insert_with(|| {
                        fs::read_to_string(entry.path().join("comm"))
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default()
                    })
                    .clone();
                held.push(Holder {
                    pid,
                    command,
                    path: path.clone(),
                });
            }
        }
        Ok(result)
    }

    /// The resolved targets of cwd, exe and each open fd, in that order.
    fn link_targets(proc_dir: &Path) -> Result<Vec<PathBuf>> {
        let mut links = vec![proc_dir.join("cwd"), proc_dir.join("exe")];
        match fs::read_dir(proc_dir.join("fd")) {
            Ok(rd) => {
                for e in rd {
                    match e {
                        Ok(e) => links.push(e.path()),
                        Err(e) if skippable(&e) => {}
                        Err(e) => return Err(e).context("reading /proc fd entry"),
                    }
                }
            }
            Err(e) if skippable(&e) => {}
            Err(e) => return Err(e).context("reading /proc fd dir"),
        }
        let mut targets = Vec::new();
        for link in links {
            let target = match fs::read_link(&link) {
                Ok(t) => t,
                Err(e) if skippable(&e) => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", link.display())),
            };
            // Deleted files read as "<path> (deleted)"; canonicalize fails, use raw target.
            targets.push(target.canonicalize().unwrap_or(target));
        }
        Ok(targets)
    }
}

#[cfg(test)]
mod exclusion_tests {
    use super::*;

    #[test]
    fn excluded_pids_are_dropped_from_every_list() {
        // A pid no real process has, so other tests in this binary are unaffected.
        exclude_pids(&[u32::MAX - 7]);
        let h = |pid| Holder {
            pid,
            command: "x".into(),
            path: PathBuf::from("/r"),
        };
        let lists = without_excluded(vec![vec![h(u32::MAX - 7), h(10)], vec![h(u32::MAX - 7)]]);
        assert_eq!(lists, vec![vec![h(10)], vec![]]);
    }
}
