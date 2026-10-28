use anyhow::Result;
use std::path::{Path, PathBuf};

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
    #[cfg(target_os = "linux")]
    {
        linux::holders(root)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        lsof::holders(root)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(Vec::new())
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod lsof {
    use super::*;
    use anyhow::{bail, Context};
    use std::process::Command;

    pub fn holders(root: &Path) -> Result<Vec<Holder>> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", root.display()))?;
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
        Ok(parse(&stdout, &root, std::process::id()))
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
    use anyhow::Context;
    use std::fs;
    use std::io::ErrorKind;

    fn skippable(e: &std::io::Error) -> bool {
        // ESRCH (process gone) surfaces as NotFound; some kernels report it raw.
        matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound)
            || e.raw_os_error() == Some(3)
    }

    pub fn holders(root: &Path) -> Result<Vec<Holder>> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", root.display()))?;
        let own = std::process::id();
        let mut result = Vec::new();
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
            if let Some(path) = first_match(&entry.path(), &root)? {
                let command = fs::read_to_string(entry.path().join("comm"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                result.push(Holder { pid, command, path });
            }
        }
        Ok(result)
    }

    /// First of cwd, exe, or an open fd that resolves under `root`.
    fn first_match(proc_dir: &Path, root: &Path) -> Result<Option<PathBuf>> {
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
        for link in links {
            let target = match fs::read_link(&link) {
                Ok(t) => t,
                Err(e) if skippable(&e) => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", link.display())),
            };
            // Deleted files read as "<path> (deleted)"; canonicalize fails, use raw target.
            let target = target.canonicalize().unwrap_or(target);
            if target.starts_with(root) {
                return Ok(Some(target));
            }
        }
        Ok(None)
    }
}
