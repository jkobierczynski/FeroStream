//! Reaching the agent: the command line, and the `ssh` command that starts
//! `ferostream-agent --stdio` on the host. OpenSSH does the login and the
//! encryption; FeroStream frames travel over its stdin and stdout.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// Usage text, shown for `--help` and for a bad command line.
pub const USAGE: &str = "\
usage: ferostream [options] [user@]host [--] [program [args...]]

Runs a program on a Linux host and shows its windows here. The program
defaults to xterm. The host needs ferostream-agent (in ~/.local/bin or on
PATH) and Xvfb.

options:
  -p PORT        SSH port
  -l USER        user name on the host
  -i FILE        identity (private key) file
  -J HOST        jump host(s), as ssh -J
  -o OPTION      any ssh option (repeatable), as ssh -o
  --ssh PATH     the ssh program (default: Windows OpenSSH)
  --agent PATH   the agent's path on the host
  --clipboard WAY  share copied text: both (default), to-host, from-host
                 or off. Text copied on Windows goes to the host when you
                 switch to one of its windows.
  -h, --help     this text";

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub ssh: PathBuf,
    /// Options passed to ssh before the destination.
    pub ssh_args: Vec<String>,
    pub destination: String,
    /// The agent's path on the host; `None` looks in `~/.local/bin`, then PATH.
    pub agent: Option<String>,
    /// The program and its arguments (sent to the agent, never through a
    /// shell).
    pub program: Vec<String>,
    pub clipboard: Clipboard,
}

/// Which ways copied text may travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clipboard {
    /// Windows to the host.
    pub to_host: bool,
    /// The host to Windows.
    pub from_host: bool,
}

impl Clipboard {
    fn parse(s: &str) -> Result<Clipboard, String> {
        let (to_host, from_host) = match s {
            "both" => (true, true),
            "to-host" => (true, false),
            "from-host" => (false, true),
            "off" | "none" => (false, false),
            _ => {
                return Err(format!(
                    "--clipboard takes both, to-host, from-host or off, not {s}"
                ));
            }
        };
        Ok(Clipboard { to_host, from_host })
    }
}

/// The remote command when no agent path is given: `~/.local/bin` first,
/// because a non-interactive SSH shell often lacks it on PATH. Wrapped in
/// `sh -c` so it works whatever the user's login shell is.
pub const DEFAULT_REMOTE: &str = "sh -c 'p=$HOME/.local/bin/ferostream-agent; test -x \"$p\" || p=ferostream-agent; exec \"$p\" --stdio'";

/// `Ok(None)` means help was asked for.
pub fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut ssh = None;
    let mut ssh_args = Vec::new();
    let mut agent = None;
    let mut clipboard = Clipboard {
        to_host: true,
        from_host: true,
    };
    let mut it = args.iter();
    let destination = loop {
        let Some(a) = it.next() else {
            return Err("no host given".into());
        };
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "-h" | "--help" | "/?" => return Ok(None),
            "-p" | "-l" | "-i" | "-J" | "-o" => {
                let v = value(a)?;
                ssh_args.push(a.clone());
                ssh_args.push(v);
            }
            "--ssh" => ssh = Some(PathBuf::from(value(a)?)),
            "--agent" => agent = Some(value(a)?),
            "--clipboard" => clipboard = Clipboard::parse(&value(a)?)?,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => break a.clone(),
        }
    };
    let mut program: Vec<String> = it.cloned().collect();
    if program.first().is_some_and(|a| a == "--") {
        program.remove(0);
    }
    if program.is_empty() {
        program.push("xterm".into());
    }
    Ok(Some(Options {
        ssh: ssh.unwrap_or_else(default_ssh),
        ssh_args,
        destination,
        agent,
        program,
        clipboard,
    }))
}

/// Windows' own OpenSSH when it is installed, else `ssh` from PATH.
fn default_ssh() -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        let p = PathBuf::from(root).join(r"System32\OpenSSH\ssh.exe");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("ssh")
}

/// The ssh command line (without stdio set up).
pub fn ssh_command(o: &Options) -> Command {
    let mut c = Command::new(&o.ssh);
    // -T: no terminal (binary-clean stream); -e none: no escape character.
    c.args(["-T", "-e", "none"]);
    c.args(&o.ssh_args);
    c.arg("--");
    c.arg(&o.destination);
    match &o.agent {
        Some(path) => c.args([path.as_str(), "--stdio"]),
        None => c.arg(DEFAULT_REMOTE),
    };
    c
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_options_host_and_program() {
        let o = parse_args(&args(
            "-p 2222 -o ServerAliveInterval=30 --agent /opt/fs/agent me@box gedit notes.txt",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(o.ssh_args, args("-p 2222 -o ServerAliveInterval=30"));
        assert_eq!(o.destination, "me@box");
        assert_eq!(o.agent.as_deref(), Some("/opt/fs/agent"));
        assert_eq!(o.program, args("gedit notes.txt"));
    }

    #[test]
    fn program_defaults_to_xterm_and_dashdash_is_dropped() {
        let o = parse_args(&args("box")).unwrap().unwrap();
        assert_eq!(o.program, args("xterm"));
        let o = parse_args(&args("box -- -weird")).unwrap().unwrap();
        assert_eq!(o.program, args("-weird"));
    }

    #[test]
    fn refuses_bad_lines() {
        assert!(parse_args(&args("")).is_err());
        assert!(parse_args(&args("-p")).is_err());
        assert!(parse_args(&args("-x box")).is_err());
        assert_eq!(parse_args(&args("--help")), Ok(None));
        assert!(parse_args(&args("--clipboard sideways box")).is_err());
    }

    #[test]
    fn clipboard_ways() {
        let both = parse_args(&args("box")).unwrap().unwrap().clipboard;
        assert!(both.to_host && both.from_host);
        let o = parse_args(&args("--clipboard from-host box"))
            .unwrap()
            .unwrap();
        assert!(!o.clipboard.to_host && o.clipboard.from_host);
        let o = parse_args(&args("--clipboard off box")).unwrap().unwrap();
        assert!(!o.clipboard.to_host && !o.clipboard.from_host);
    }

    #[test]
    fn ssh_command_line() {
        let o = parse_args(&args("--ssh ssh -p 22 box")).unwrap().unwrap();
        let c = ssh_command(&o);
        let a: Vec<_> = c
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&a[..6], &["-T", "-e", "none", "-p", "22", "--"]);
        assert_eq!(a[6], "box");
        assert_eq!(a[7], DEFAULT_REMOTE);
    }
}
