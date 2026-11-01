use super::{BuildSystem, Marker};

pub struct Gradle;

impl BuildSystem for Gradle {
    fn name(&self) -> &'static str {
        "gradle"
    }

    fn markers(&self) -> &'static [Marker] {
        &[
            Marker::Name("settings.gradle"),
            Marker::Name("settings.gradle.kts"),
        ]
    }

    fn regenerable(&self) -> &'static [&'static str] {
        &[
            "build",
            ".gradle",
            ".kotlin",
            ".cxx",
            ".externalNativeBuild",
        ]
    }

    /// Only `build/`: `.gradle/`, `.kotlin/` and `.cxx/` hold caches the next
    /// build reads.
    fn stash(&self) -> &'static [&'static str] {
        &["build"]
    }
}
