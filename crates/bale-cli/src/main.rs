use clap::Parser;

/// Manage agent git worktrees: bale idle ones by removing build output.
#[derive(Parser)]
#[command(name = "bale", version)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
