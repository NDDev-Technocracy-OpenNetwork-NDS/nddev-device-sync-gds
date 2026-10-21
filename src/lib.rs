//! Bounded Rust replacement of the local Git status provider.
//! Core owns classification; shared Agent IO owns worker lifetime.

#[cfg(feature = "github-catalog")]
pub mod github;
pub mod view;
pub use view::*;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub mod rust_status;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use rust_status::IsolatedGds as Gds;

#[cfg(all(
    feature = "commit-sync",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub mod commits;

pub const MANIFEST: &str = include_str!("../contracts/module.json");
