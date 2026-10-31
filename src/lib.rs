//! Bounded Rust replacement of the local Git status provider.
//! Core owns classification; shared Agent IO owns worker lifetime.

#[cfg(feature = "catalog-runtime")]
pub mod catalog_runtime;
#[cfg(feature = "github-catalog")]
pub mod github;
#[cfg(all(feature = "catalog-runtime", not(target_arch = "wasm32")))]
pub mod ignore;
pub mod view;
pub use view::*;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub mod rust_status;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use rust_status::IsolatedGithub as Github;

#[cfg(all(
    feature = "commit-sync",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub mod commits;

pub const MANIFEST: &str = include_str!("../contracts/module.json");
