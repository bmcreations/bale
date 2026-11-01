use super::{BuildSystem, Marker};

pub struct SwiftPm;

impl BuildSystem for SwiftPm {
    fn name(&self) -> &'static str {
        "swiftpm"
    }

    fn markers(&self) -> &'static [Marker] {
        &[Marker::Name("Package.swift")]
    }

    fn regenerable(&self) -> &'static [&'static str] {
        &[".build"]
    }

    fn derived_data(&self) -> bool {
        true
    }
}
