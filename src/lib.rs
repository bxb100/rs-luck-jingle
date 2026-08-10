//! D1X printer discovery, protocol, rendering, session, and transport primitives.

pub mod discovery;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod macos_bluetooth;
pub mod markdown;
pub mod protocol;
pub mod render;
pub mod session;
pub mod transport;
