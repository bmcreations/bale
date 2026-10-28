//! git's post-checkout hook (`bale hook git post-checkout`).

use std::process::ExitCode;

use anyhow::Context;
use bale_core::git;
use bale_core::ops::up::register;
use bale_core::BaleHome;

/// post-checkout: registers the cwd when git just created it as a linked
/// worktree (`prev` all zeros, branch checkout). Never fails the checkout.
pub fn post_checkout(home: &BaleHome, prev: &str, _new: &str, flag: &str) -> ExitCode {
    if flag == "1" && !prev.is_empty() && prev.chars().all(|c| c == '0') {
        let result = std::env::current_dir()
            .context("reading the current directory")
            .and_then(|cwd| {
                // A clone fires this hook in the main checkout; skip it quietly.
                if git::admin_dir(&cwd)? == git::common_dir(&cwd)? {
                    return Ok(());
                }
                register(home, &cwd).map(|_| ())
            });
        if let Err(e) = result {
            eprintln!("bale: post-checkout: {e:#}");
        }
    }
    ExitCode::SUCCESS
}
