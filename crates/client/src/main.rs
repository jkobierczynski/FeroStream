//! `ferostream.exe`: see [`ferostream_client::transport::USAGE`].

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use ferostream_client::transport::{USAGE, parse_args};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    #[cfg(windows)]
    if std::env::var_os(ferostream_client::askpass::ENV).is_some() {
        // ssh runs us as its SSH_ASKPASS program: the prompt is the argument,
        // the answer goes to stdout.
        return ferostream_client::askpass::run(args.first().map(String::as_str).unwrap_or(""));
    }

    let options = match parse_args(&args) {
        Ok(Some(o)) => o,
        Ok(None) => {
            show(USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            show(&format!("{e}\n\n{USAGE}"));
            return ExitCode::from(2);
        }
    };

    #[cfg(windows)]
    {
        ferostream_client::win32::run(options)
    }
    #[cfg(not(windows))]
    {
        let _ = options;
        eprintln!("ferostream: the client's windows are Win32; build it for Windows.");
        ExitCode::FAILURE
    }
}

fn show(text: &str) {
    #[cfg(windows)]
    ferostream_client::win32::message_box("FeroStream", text, false);
    #[cfg(not(windows))]
    eprintln!("{text}");
}
