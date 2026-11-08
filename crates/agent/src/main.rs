//! `ferostream-agent`: runs a Linux program on a private X display and
//! streams its windows to a FeroStream client over stdin and stdout; the
//! session outlives a dropped connection.

#[cfg(unix)]
mod agent;
#[cfg(unix)]
mod clipboard;
#[cfg(target_os = "linux")]
mod harden;
#[cfg(unix)]
mod keyboard;
#[cfg(unix)]
mod relay;
#[cfg(unix)]
mod runtime;
#[cfg(unix)]
mod scale;
#[cfg(unix)]
mod session;
#[cfg(unix)]
mod wm;
#[cfg(unix)]
mod xserver;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    agent::main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("ferostream-agent runs on Linux (it needs Xvfb).");
    std::process::ExitCode::FAILURE
}
