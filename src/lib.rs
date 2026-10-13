//! Bounded Rust replacement of the local Git status provider.
//! Core owns classification; shared Agent IO owns worker lifetime.

pub mod view;
pub use view::*;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub mod rust_status;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use rust_status::IsolatedGds as Gds;

pub const MANIFEST: &str = include_str!("../contracts/module.json");
