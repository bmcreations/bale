//! `bale install` and `bale uninstall`: the sweep schedule, or with `--hooks`
//! only the given tools' hooks (built by `bale-hooks`). Every effect is first
//! built as a [`Plan`], which `--print-only` prints and a real run applies.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use bale_core::registry::{Level, Registry};
use bale_core::BaleHome;
use bale_hooks::install::{Plan, Step};
use bale_hooks::{display_path, Tool};

const LAUNCHD_LABEL: &str = "dev.bale.sweep";

/// Builds the whole install. Without hooks it installs the sweep schedule;
/// otherwise only the listed tools' hooks, each needing `repo` where its
/// config lives in the repo.
pub fn plan(hooks: &[Tool], repo: Option<&Path>) -> Result<Plan> {
    let exe = bale_exe()?;
    let home = home_dir()?;
    if hooks.is_empty() {
        return schedule_plan(&home, &exe);
    }
    let mut plan = Plan::default();
    for &tool in hooks {
        plan.extend(bale_hooks::install_plan(tool, &exe, &home, repo)?);
    }
    Ok(plan)
}

/// Builds the whole uninstall: without hooks it removes the sweep schedule,
/// otherwise only bale's entries in the listed tools' hooks. bale's saved
/// state is never touched; the plan ends saying so.
pub fn uninstall_plan(hooks: &[Tool], repo: Option<&Path>, bale: &BaleHome) -> Result<Plan> {
    let home = home_dir()?;
    let mut plan = Plan::default();
    if hooks.is_empty() {
        plan.extend(unschedule_plan(&home)?);
    }
    for &tool in hooks {
        plan.extend(bale_hooks::uninstall_plan(tool, &home, repo)?);
    }
    plan.extend(Plan::new(vec![Step::Note(kept_note(bale)?)]));
    Ok(plan)
}

/// Says that bale's saved state stays, with how many worktrees are cold.
fn kept_note(bale: &BaleHome) -> Result<String> {
    let cold = Registry::open(bale.clone())
        .all_lenient()?
        .into_iter()
        .filter(|(_, r)| r.as_ref().is_ok_and(|r| r.level == Level::Cold))
        .count();
    let count = match cold {
        1 => "1 worktree is cold".to_string(),
        n => format!("{n} worktrees are cold"),
    };
    Ok(format!(
        "note: bale's saved state in {} is kept: {count}, and `bale open` still restores \
         a cold worktree",
        display_path(&bale.0)
    ))
}

/// Stops and deletes the sweep schedule [`schedule_plan`] installs. Each
/// stop command's failure is ignored: the schedule may not be installed.
fn unschedule_plan(home: &Path) -> Result<Plan> {
    let run = |args: &[&str]| Step::Run {
        args: args.iter().map(|s| s.to_string()).collect(),
        ignore_failure: true,
        attempts: 1,
    };
    let steps = if cfg!(target_os = "macos") {
        let uid = current_uid()?;
        vec![
            run(&[
                "launchctl",
                "bootout",
                &format!("gui/{uid}/{LAUNCHD_LABEL}"),
            ]),
            Step::Remove(
                home.join("Library")
                    .join("LaunchAgents")
                    .join(format!("{LAUNCHD_LABEL}.plist")),
            ),
        ]
    } else if cfg!(windows) {
        vec![run(&["schtasks", "/Delete", "/TN", "bale-sweep", "/F"])]
    } else {
        let units = home.join(".config").join("systemd").join("user");
        vec![
            run(&[
                "systemctl",
                "--user",
                "disable",
                "--now",
                "bale-sweep.timer",
            ]),
            Step::Remove(units.join("bale-sweep.service")),
            Step::Remove(units.join("bale-sweep.timer")),
            run(&["systemctl", "--user", "daemon-reload"]),
        ]
    };
    Ok(Plan::new(steps))
}

/// The running binary's absolute path as invoked (not canonicalized, so a
/// package manager's stable symlink such as `/opt/homebrew/bin/bale` is kept);
/// schedulers have a minimal PATH. Warns when it looks like a build or
/// versioned directory that will move.
fn bale_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the bale binary")?;
    let exe = PathBuf::from(display_path(&exe));
    if is_unstable_location(&exe) {
        eprintln!(
            "warning: {} is a build or versioned path; hooks and the schedule written now \
             stop working when it moves. Install from a stable path such as the one on PATH.",
            exe.display()
        );
    }
    Ok(exe)
}

/// Under a cargo `target/` dir, or a Homebrew `Cellar/<formula>/<version>` dir.
fn is_unstable_location(exe: &Path) -> bool {
    let parts: Vec<&std::ffi::OsStr> = exe.parent().map(|p| p.iter().collect()).unwrap_or_default();
    parts.iter().any(|p| *p == "target")
        || parts
            .iter()
            .position(|p| *p == "Cellar")
            .is_some_and(|i| parts.len() > i + 2)
}

/// The home dir, from `HOME` or `USERPROFILE` (the latter first on Windows),
/// so tests can redirect it.
pub(crate) fn home_dir() -> Result<PathBuf> {
    let order = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    order
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

/// `PATH`, and `BALE_HOME` when set, captured at install time so the
/// scheduled sweep sees the same environment.
fn schedule_env() -> Vec<(&'static str, String)> {
    ["PATH", "BALE_HOME"]
        .into_iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k, v))
        })
        .collect()
}

fn schedule_plan(home: &Path, exe: &Path) -> Result<Plan> {
    let exe_s = display_path(exe);
    let env = schedule_env();
    let mut steps = Vec::new();
    if cfg!(target_os = "macos") {
        let agents = home.join("Library").join("LaunchAgents");
        let logs = home.join("Library").join("Logs").join("bale");
        let plist = agents.join(format!("{LAUNCHD_LABEL}.plist"));
        let uid = current_uid()?;
        steps.push(Step::Mkdir(agents));
        steps.push(Step::Mkdir(logs.clone()));
        steps.push(Step::Write {
            path: plist.clone(),
            content: launchd_plist(&exe_s, &display_path(&logs.join("sweep.log")), &env),
            executable: false,
        });
        steps.push(Step::Run {
            args: vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{LAUNCHD_LABEL}"),
            ],
            ignore_failure: true,
            attempts: 1,
        });
        steps.push(Step::Run {
            args: vec![
                "launchctl".into(),
                "bootstrap".into(),
                format!("gui/{uid}"),
                display_path(&plist),
            ],
            ignore_failure: false,
            attempts: 5,
        });
    } else if cfg!(windows) {
        steps.push(Step::Run {
            args: [
                "schtasks",
                "/Create",
                "/SC",
                "MINUTE",
                "/MO",
                "30",
                "/TN",
                "bale-sweep",
                "/TR",
            ]
            .iter()
            .map(|s| s.to_string())
            .chain([format!("\"{exe_s}\" sweep"), "/F".to_string()])
            .collect(),
            ignore_failure: false,
            attempts: 1,
        });
    } else {
        let units = home.join(".config").join("systemd").join("user");
        steps.push(Step::Mkdir(units.clone()));
        steps.push(Step::Write {
            path: units.join("bale-sweep.service"),
            content: format!(
                "[Unit]\nDescription=bale sweep\n\n[Service]\nType=oneshot\n{}ExecStart={} sweep\n",
                env.iter()
                    .map(|(k, v)| format!(
                        "Environment={}\n",
                        systemd_env_quote(&format!("{k}={v}"))
                    ))
                    .collect::<String>(),
                systemd_quote(&exe_s)
            ),
            executable: false,
        });
        steps.push(Step::Write {
            path: units.join("bale-sweep.timer"),
            content: "[Unit]\nDescription=Run bale sweep every 30 minutes\n\n[Timer]\n\
                      OnBootSec=5min\nOnUnitActiveSec=30min\n\n[Install]\nWantedBy=timers.target\n"
                .to_string(),
            executable: false,
        });
        for args in [
            &["systemctl", "--user", "daemon-reload"][..],
            &["systemctl", "--user", "enable", "--now", "bale-sweep.timer"],
        ] {
            steps.push(Step::Run {
                args: args.iter().map(|s| s.to_string()).collect(),
                ignore_failure: false,
                attempts: 1,
            });
        }
    }
    if !cfg!(windows) && !env.is_empty() {
        let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        steps.push(Step::Note(format!(
            "note: the schedule runs with the {} captured now; rerun `bale install` \
             after changing {}",
            keys.join(" and "),
            if keys.len() > 1 { "them" } else { "it" }
        )));
    }
    Ok(Plan::new(steps))
}

fn current_uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    if !out.status.success() {
        bail!("id -u failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn launchd_plist(exe: &str, log: &str, env: &[(&str, String)]) -> String {
    let env_dict = if env.is_empty() {
        String::new()
    } else {
        let entries: String = env
            .iter()
            .map(|(k, v)| {
                format!(
                    "    <key>{}</key>\n    <string>{}</string>\n",
                    xml_escape(k),
                    xml_escape(v)
                )
            })
            .collect();
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{entries}  </dict>\n")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>sweep</string>
  </array>
  <key>StartInterval</key>
  <integer>1800</integer>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
{}</dict>
</plist>
"#,
        xml_escape(exe),
        xml_escape(log),
        xml_escape(log),
        env_dict
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A systemd ExecStart word: double-quoted, with `\`, `"`, `%` and `$` escaped.
fn systemd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// A systemd `Environment=` assignment: double-quoted, `\`, `"` and `%` escaped.
fn systemd_env_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_quote_escapes_specials() {
        assert_eq!(systemd_quote("/a b/%x$y"), "\"/a b/%%x$$y\"");
    }

    #[test]
    fn cellar_version_dir_is_unstable() {
        assert!(is_unstable_location(Path::new(
            "/opt/homebrew/Cellar/bale/0.3.1/bin/bale"
        )));
        assert!(!is_unstable_location(Path::new("/opt/homebrew/bin/bale")));
    }
}
