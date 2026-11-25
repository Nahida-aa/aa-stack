//! 与 gpui 无关的通用工具。

#[cfg(not(target_family = "wasm"))]
pub mod archive;
#[cfg(not(target_family = "wasm"))]
pub mod fs;

mod scope_guard;

pub use scope_guard::{Deferred, defer};
