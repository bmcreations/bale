//! The build systems bale knows. Each one is a type in its own module that
//! implements [`BuildSystem`]; to add a system, write that module and list
//! the type in [`ALL`].

mod gradle;
mod swift_pm;
mod xcode;

pub use gradle::Gradle;
pub use swift_pm::SwiftPm;
pub use xcode::Xcode;

/// Every system, in the order `detect` checks markers.
pub static ALL: &[&dyn BuildSystem] = &[&Gradle, &Xcode, &SwiftPm];

/// What bale knows about one build system's output in a tree.
pub trait BuildSystem: Sync {
    /// A short lowercase name, unique among [`ALL`].
    fn name(&self) -> &'static str;

    /// Names that mark a tree as using this system, at its root or one level
    /// down.
    fn markers(&self) -> &'static [Marker];

    /// Directory names, anywhere in the tree, that the build recreates. `up`
    /// deletes them (unless they hold tracked or unignored files), and idle
    /// time ignores changes inside them.
    fn regenerable(&self) -> &'static [&'static str];

    /// The names in [`regenerable`](Self::regenerable) whose large files
    /// `stash` may replace with empty stubs. Only list a directory that holds
    /// nothing but the build's own output: a build that reads an emptied input
    /// builds the wrong thing rather than failing. Keep the default until a
    /// stash and restore of a real build has been measured to rebuild nothing.
    fn stash(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether an Xcode DerivedData root, by any name, counts as regenerable,
    /// with its `Build/` stashable.
    fn derived_data(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for dyn BuildSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A file or directory name that shows a build system is in use.
#[derive(Debug, Clone, Copy)]
pub enum Marker {
    /// The whole name, such as `settings.gradle`.
    Name(&'static str),
    /// A name ending, such as `.xcodeproj`.
    Suffix(&'static str),
}

impl Marker {
    pub(crate) fn matches(self, name: &str) -> bool {
        match self {
            Marker::Name(n) => name == n,
            Marker::Suffix(s) => name.ends_with(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_system_is_complete() {
        let mut names = Vec::new();
        for sys in ALL {
            assert!(!sys.markers().is_empty(), "{sys:?} has no markers");
            for name in sys.stash() {
                assert!(
                    sys.regenerable().contains(name),
                    "{sys:?} stashes {name}, which is not regenerable"
                );
            }
            assert!(!names.contains(&sys.name()), "{sys:?} is listed twice");
            names.push(sys.name());
        }
    }
}
