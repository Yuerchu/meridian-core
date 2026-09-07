// Ported from Codex (Apache-2.0): codex-windows-sandbox
// Core Win32 API layer for restricted-token process sandboxing.
// Stripped: elevated backend, WFP, setup system, ConPTY, otel.
#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
pub mod acl;
#[cfg(windows)]
pub mod cap;
#[cfg(windows)]
pub mod desktop;
#[cfg(windows)]
pub mod job;
#[cfg(windows)]
pub(crate) mod logging;
#[cfg(windows)]
pub(crate) mod path_normalization;
#[cfg(windows)]
pub mod proc_thread_attr;
#[cfg(windows)]
pub mod process;
#[cfg(windows)]
pub mod token;
#[cfg(windows)]
pub mod winutil;
