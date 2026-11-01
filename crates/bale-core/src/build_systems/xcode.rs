use super::{BuildSystem, Marker};

/// Xcode projects and workspaces. Most of Xcode's output lives in DerivedData,
/// whose ownership, reaping and shared Swift packages are handled by
/// `derived_data`, `spm` and the sweep rather than here.
pub struct Xcode;

impl BuildSystem for Xcode {
    fn name(&self) -> &'static str {
        "xcode"
    }

    fn markers(&self) -> &'static [Marker] {
        &[Marker::Suffix(".xcodeproj"), Marker::Suffix(".xcworkspace")]
    }

    fn regenerable(&self) -> &'static [&'static str] {
        &[".build"]
    }

    fn derived_data(&self) -> bool {
        true
    }
}
