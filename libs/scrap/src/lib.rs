//! Screen capture: DXGI on Windows, Quartz on macOS, X11/Wayland on Linux.
//!
//! # The `stub` feature
//!
//! With `stub` enabled this crate compiles to a **capture-free stand-in** that still
//! exposes the handful of types the rest of the tree names at compile time. Enabled
//! by the root crate's `terminal-only` feature (see `docs/terminal-only/DESIGN.md`).
//!
//! Why it has to be a feature of *this* crate rather than a separate crate: the call
//! sites write `scrap::…`, so the crate has to be named `scrap`, and two packages in
//! one workspace may not share a name.
//!
//! What actually gets dropped is the build script's vcpkg probing, and that is the
//! whole point: `libvpx`, `aom` and `libyuv` are all capture-side encoders, so a
//! headless remote-terminal build must not need vcpkg at all.

#[cfg(feature = "stub")]
mod stub;
#[cfg(feature = "stub")]
pub use stub::*;

#[cfg(not(feature = "stub"))]
#[cfg(quartz)]
extern crate block;
#[cfg(not(feature = "stub"))]
#[macro_use]
extern crate cfg_if;
#[cfg(not(feature = "stub"))]
pub use hbb_common::libc;
#[cfg(not(feature = "stub"))]
#[cfg(dxgi)]
extern crate winapi;

#[cfg(not(feature = "stub"))]
pub use common::*;

#[cfg(not(feature = "stub"))]
#[cfg(quartz)]
pub mod quartz;

#[cfg(not(feature = "stub"))]
#[cfg(x11)]
pub mod x11;

#[cfg(not(feature = "stub"))]
#[cfg(all(x11, feature = "wayland"))]
pub mod wayland;

#[cfg(not(feature = "stub"))]
#[cfg(dxgi)]
pub mod dxgi;

#[cfg(not(feature = "stub"))]
#[cfg(target_os = "android")]
pub mod android;

#[cfg(not(feature = "stub"))]
mod common;
