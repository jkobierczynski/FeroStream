//! FeroStream client: shows the windows of a program running on a Linux
//! host (through `ferostream-agent`) as native Windows windows.
//!
//! The portable parts (command line, transport, pixels, key tables) build
//! and test anywhere; the windows themselves are Win32 and build on Windows
//! only.

#[cfg(windows)]
pub mod clip;
pub mod cliptext;
pub mod decode;
pub mod fb;
pub mod keys;
pub mod link;
pub mod reconnect;
pub mod transport;

#[cfg(windows)]
pub mod askpass;
#[cfg(windows)]
pub mod picture;
#[cfg(windows)]
pub mod win32;
