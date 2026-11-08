//! Reaching the agent: the command line, and the `ssh` command that starts
//! `ferostream-agent --stdio` on the host. OpenSSH does the login and the
//! encryption; FeroStream frames travel over its stdin and stdout.
//!
//! The agent ships next to `ferostream.exe`. The remote command compares the
//! host's copy in `~/.local/bin` with ours (POSIX `cksum`: CRC and size) and,
//! when it differs or is missing, asks for ours on the same connection
//! ([`UPLOAD_MARKER`]); one login does both.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferostream_proto::is_session_id;

/// Usage text, shown for `--help` and for a bad command line.
pub const USAGE: &str = "\
usage: ferostream [options] [user@]host [--] [program [args...]]
       ferostream [options] --attach[=ID] [user@]host

Runs a program on a Linux host and shows its windows here. The program
defaults to xterm. The host needs Xvfb; ferostream-agent is copied to
~/.local/bin on the host when it is missing or out of date.

When the connection drops, the program keeps running on the host and the
windows come back when it reconnects. --attach brings back the windows of
a program whose ferostream.exe was closed or lost (the most recent one, or
session ID: ferostream-agent --list on the host shows them).

options:
  -p PORT        SSH port
  -l USER        user name on the host
  -i FILE        identity (private key) file
  -J HOST        jump host(s), as ssh -J
  -o OPTION      any ssh option (repeatable), as ssh -o
  --ssh PATH     the ssh program (default: Windows OpenSSH)
  --agent PATH   the agent's path on the host (it is not copied then)
  --no-install   never copy the agent to the host
  --install-to PATH  where the agent is copied to on the host
                 (default ~/.local/bin/ferostream-agent)
  --remove-agent delete the copied agent from the host once its program
                 ends and no other FeroStream session runs there
  --attach[=ID]  take over a session left running on the host
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
    /// A new session, or one to take over.
    pub attach: AttachTo,
    /// Copy the agent to the host when it is missing or different.
    pub install: bool,
    /// Where the agent is installed on the host, and whether it goes.
    pub place: Place,
}

/// Which session on the host the client wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachTo {
    /// Start a new one.
    New,
    /// The one whose client left most recently.
    Newest,
    /// This one (an id from [`ferostream_proto::Message::Session`]).
    Id(String),
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

/// What the remote command prints, before the agent's preamble, when it
/// wants the agent: then `<arch>\n`, and it reads exactly the agent's size
/// in bytes from stdin.
pub const UPLOAD_MARKER: &[u8] = b"\0FEROSTREAM-UPLOAD ";

/// Biggest agent file the client offers, in bytes.
pub const MAX_AGENT_FILE: u64 = 64 * 1024 * 1024;

/// An agent binary next to `ferostream.exe`, for hosts of one architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFile {
    /// As `uname -m` says it: `x86_64`, `aarch64`.
    pub arch: String,
    pub path: PathBuf,
    /// POSIX `cksum` of the file, and its size.
    pub crc: u32,
    pub size: u64,
}

/// The agent binaries in `dir`: `ferostream-agent` (x86-64, what CI builds)
/// and `ferostream-agent-<arch>` for other hosts.
pub fn find_agent_files(dir: &Path) -> Vec<AgentFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<AgentFile> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let arch = match name.as_str() {
            "ferostream-agent" => "x86_64",
            n => match n.strip_prefix("ferostream-agent-") {
                Some(a) if is_arch(a) => a,
                _ => continue,
            },
        };
        if out.iter().any(|f| f.arch == arch) {
            continue;
        }
        let path = e.path();
        let ok_size = std::fs::metadata(&path)
            .is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_AGENT_FILE);
        if !ok_size {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        out.push(AgentFile {
            arch: arch.into(),
            path,
            crc: cksum(&data),
            size: data.len() as u64,
        });
    }
    out.sort_by(|a, b| a.arch.cmp(&b.arch));
    out
}

/// An architecture name safe on a command line.
pub fn is_arch(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 32
        && a.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The POSIX `cksum` CRC of `data` (CRC-32, polynomial 0x04C11DB7, the
/// length appended, inverted), as every Linux has it.
pub fn cksum(data: &[u8]) -> u32 {
    fn step(crc: u32, byte: u8) -> u32 {
        let mut c = crc ^ (u32::from(byte) << 24);
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
        }
        c
    }
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        *slot = step(0, i as u8);
    }
    let byte = |crc: u32, b: u8| {
        let i = usize::from(((crc >> 24) as u8) ^ b);
        (crc << 8) ^ table.get(i).copied().unwrap_or(0)
    };
    let mut crc = data.iter().fold(0u32, |c, &b| byte(c, b));
    let mut len = data.len() as u64;
    while len != 0 {
        crc = byte(crc, len as u8);
        len >>= 8;
    }
    !crc
}

/// Where the agent lives on the host and whether it goes when done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The agent's path for `sh`: `$HOME/...` or absolute, checked to need
    /// no quoting ([`Place::new`]).
    sh_path: String,
    /// Delete the agent's file once its session ends and no other session
    /// runs (`--remove-when-done`). Only for the agent at `sh_path`, never
    /// one found on the host's `PATH`.
    pub remove: bool,
}

/// The agent's place on the host unless told otherwise.
pub const DEFAULT_AGENT_PATH: &str = "~/.local/bin/ferostream-agent";

impl Default for Place {
    fn default() -> Place {
        Place {
            sh_path: "$HOME/.local/bin/ferostream-agent".into(),
            remove: false,
        }
    }
}

impl Place {
    /// `path` is `~/...` or absolute, made of letters, digits and
    /// `. _ - + /` (it goes into a shell command unquoted), without `..`.
    pub fn new(path: &str, remove: bool) -> Result<Place, String> {
        let (prefix, rest) = match path.strip_prefix("~/") {
            Some(r) => ("$HOME/", r),
            None => match path.strip_prefix('/') {
                Some(r) => ("/", r),
                None => return Err(format!("the agent's path must start with ~/ or /: {path}")),
            },
        };
        let ok = !rest.is_empty()
            && rest.len() <= 255
            && !rest.ends_with('/')
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-+/".contains(&b))
            && rest
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        if !ok {
            return Err(format!(
                "the agent's path may hold only letters, digits, . _ - + and /: {path}"
            ));
        }
        Ok(Place {
            sh_path: format!("{prefix}{rest}"),
            remove,
        })
    }
}

/// The command ssh runs on the host. Wrapped in `sh -c '...'` so it works
/// whatever the user's login shell is; the script has no single quote, and
/// what goes into it (CRCs, sizes, architectures, session ids) is checked.
///
/// With agent files: the agent at `place` (`~/.local/bin/ferostream-agent`
/// unless told otherwise) is compared with ours for the host's
/// architecture and replaced (atomically, checked again) when it differs.
/// Without: `place` first, then PATH, because a non-interactive SSH shell
/// often lacks `~/.local/bin` on PATH.
///
/// `place.remove` asks the agent at `place` to delete itself when done.
pub fn remote_command(files: &[AgentFile], attach: &AttachTo, place: &Place) -> String {
    let start = format!("exec \"$p\" {} $r", agent_args(attach).join(" "));
    let head = format!(
        "p={}; r={}",
        place.sh_path,
        if place.remove {
            "--remove-when-done"
        } else {
            ""
        }
    );
    let files: Vec<String> = files
        .iter()
        .filter(|f| is_arch(&f.arch))
        .map(|f| format!("{}:{}:{}", f.arch, f.crc, f.size))
        .collect();
    if files.is_empty() {
        return format!("sh -c '{head}; test -x \"$p\" || {{ p=ferostream-agent; r=; }}; {start}'");
    }
    format!(
        "sh -c '{head}; a=$(uname -m); w=; \
         for e in {list}; do case $e in \"$a\":*) w=${{e#*:}};; esac; done; \
         if [ -n \"$w\" ]; then \
         c=\"${{w%:*}} ${{w#*:}}\"; \
         if [ ! -x \"$p\" ] || [ \"$(cksum < \"$p\" 2>/dev/null)\" != \"$c\" ]; then \
         mkdir -p \"${{p%/*}}\" && printf \"\\000FEROSTREAM-UPLOAD %s\\n\" \"$a\" && \
         head -c \"${{w#*:}}\" > \"$p.new\" && [ \"$(cksum < \"$p.new\")\" = \"$c\" ] && \
         chmod 700 \"$p.new\" && mv -f \"$p.new\" \"$p\" || \
         {{ echo \"ferostream: could not install the agent in ${{p%/*}}\" >&2; rm -f \"$p.new\"; exit 1; }}; \
         fi; \
         else test -x \"$p\" || {{ p=ferostream-agent; r=; }}; fi; \
         {start}'",
        list = files.join(" ")
    )
}

fn agent_args(attach: &AttachTo) -> Vec<&str> {
    match attach {
        AttachTo::New => vec!["--stdio"],
        AttachTo::Newest => vec!["--stdio", "--attach"],
        AttachTo::Id(id) if is_session_id(id) => vec!["--stdio", "--attach", id],
        // Never reached (ids are checked where they come in); refuse rather
        // than pass something odd to a shell.
        AttachTo::Id(_) => vec!["--stdio", "--attach", "invalid"],
    }
}

/// `Ok(None)` means help was asked for.
pub fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut ssh = None;
    let mut ssh_args = Vec::new();
    let mut agent = None;
    let mut attach = AttachTo::New;
    let mut install = true;
    let mut install_to: Option<String> = None;
    let mut remove = false;
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
            "--no-install" => install = false,
            "--install-to" => install_to = Some(value(a)?),
            "--remove-agent" => remove = true,
            "--attach" => attach = AttachTo::Newest,
            s if s.starts_with("--attach=") => {
                let id = s.trim_start_matches("--attach=");
                if !is_session_id(id) {
                    return Err(format!(
                        "--attach= takes a session id (12 digits and letters a-f), not {id}"
                    ));
                }
                attach = AttachTo::Id(id.into());
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => break a.clone(),
        }
    };
    let mut program: Vec<String> = it.cloned().collect();
    if program.first().is_some_and(|a| a == "--") {
        program.remove(0);
    }
    if attach != AttachTo::New && !program.is_empty() {
        return Err("--attach takes no program: the session has its own".into());
    }
    if program.is_empty() {
        program.push("xterm".into());
    }
    let place = match install_to {
        Some(p) => Place::new(&p, remove)?,
        None => Place {
            remove,
            ..Place::default()
        },
    };
    Ok(Some(Options {
        place,
        ssh: ssh.unwrap_or_else(default_ssh),
        ssh_args,
        destination,
        agent,
        program,
        clipboard,
        attach,
        install,
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

/// The ssh command line (without stdio set up) for session `attach`;
/// `files` are the agents the client can copy to the host.
pub fn ssh_command(o: &Options, files: &[AgentFile], attach: &AttachTo) -> Command {
    let mut c = Command::new(&o.ssh);
    // -T: no terminal (binary-clean stream); -e none: no escape character.
    c.args(["-T", "-e", "none"]);
    c.args(&o.ssh_args);
    // Notice a dead connection within a minute, to reconnect. ssh keeps
    // the first value it is given, so the user's own -o wins.
    c.args([
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
    ]);
    c.arg("--");
    c.arg(&o.destination);
    match &o.agent {
        Some(path) => {
            c.arg(path.as_str());
            c.args(agent_args(attach));
        }
        None if o.install => {
            c.arg(remote_command(files, attach, &o.place));
        }
        None => {
            c.arg(remote_command(&[], attach, &o.place));
        }
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
        assert!(parse_args(&args("--attach=../../x box")).is_err());
        assert!(parse_args(&args("--attach box gimp")).is_err());
    }

    #[test]
    fn attach_and_install_options() {
        let o = parse_args(&args("box")).unwrap().unwrap();
        assert_eq!(o.attach, AttachTo::New);
        assert!(o.install);
        let o = parse_args(&args("--attach --no-install box"))
            .unwrap()
            .unwrap();
        assert_eq!(o.attach, AttachTo::Newest);
        assert!(!o.install);
        let o = parse_args(&args("--attach=0123456789ab box"))
            .unwrap()
            .unwrap();
        assert_eq!(o.attach, AttachTo::Id("0123456789ab".into()));
    }

    #[test]
    fn cksum_matches_posix() {
        // Values from `printf ... | cksum`.
        assert_eq!(cksum(b""), 4294967295);
        assert_eq!(cksum(b"hello"), 3287646509);
        let big: Vec<u8> = (0..100_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        let mut sh = std::process::Command::new("cksum");
        if let Ok(mut child) = sh
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&big).unwrap();
            let out = child.wait_with_output().unwrap();
            let text = String::from_utf8(out.stdout).unwrap();
            assert_eq!(text.trim(), format!("{} 100000", cksum(&big)));
        }
    }

    #[test]
    fn finds_agent_files() {
        let dir = std::env::temp_dir().join(format!("ferostream-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("ferostream-agent"), b"hello").unwrap();
        std::fs::write(dir.join("ferostream-agent-aarch64"), b"arm").unwrap();
        std::fs::write(dir.join("ferostream-agent-Bad Name"), b"x").unwrap();
        std::fs::write(dir.join("ferostream-agent-riscv64"), b"").unwrap();
        std::fs::write(dir.join("ferostream.exe"), b"x").unwrap();
        let f = find_agent_files(&dir);
        let arches: Vec<&str> = f.iter().map(|f| f.arch.as_str()).collect();
        assert_eq!(arches, ["aarch64", "x86_64"]);
        assert_eq!((f[1].crc, f[1].size), (3287646509, 5));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn agent_places() {
        assert_eq!(
            Place::new("~/bin/fa", false).unwrap().sh_path,
            "$HOME/bin/fa"
        );
        assert_eq!(Place::new("/opt/x/fa", true).unwrap().sh_path, "/opt/x/fa");
        for bad in [
            "bin/fa", "~/", "/", "~/a b", "~/a;rm", "~/x/../y", "~/x//y", "~/x/", "~/$(id)",
            "~/'q", "~/a\"b",
        ] {
            assert!(Place::new(bad, false).is_err(), "{bad}");
        }
        let o = parse_args(&args("--install-to ~/t/fa --remove-agent box"))
            .unwrap()
            .unwrap();
        assert_eq!(o.place, Place::new("~/t/fa", true).unwrap());
        assert!(parse_args(&args("--install-to rel/fa box")).is_err());
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

    fn command_args(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn ssh_command_line() {
        let o = parse_args(&args("--ssh ssh -p 22 box")).unwrap().unwrap();
        let a = command_args(&ssh_command(&o, &[], &AttachTo::New));
        assert_eq!(&a[..5], &["-T", "-e", "none", "-p", "22"]);
        assert_eq!(
            &a[5..10],
            &[
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
                "--"
            ]
        );
        assert_eq!(a[10], "box");
        assert_eq!(
            a[11],
            remote_command(&[], &AttachTo::New, &Place::default())
        );
        assert!(!a[11].contains("UPLOAD"));

        // An agent path: used as given, never installed.
        let o = parse_args(&args("--agent /opt/a box")).unwrap().unwrap();
        let file = AgentFile {
            arch: "x86_64".into(),
            path: "x".into(),
            crc: 1,
            size: 2,
        };
        let a = command_args(&ssh_command(
            &o,
            std::slice::from_ref(&file),
            &AttachTo::Id("0123456789ab".into()),
        ));
        assert_eq!(
            &a[a.len() - 4..],
            &["/opt/a", "--stdio", "--attach", "0123456789ab"]
        );
        // --no-install: no upload in the remote command.
        let o = parse_args(&args("--no-install box")).unwrap().unwrap();
        let a = command_args(&ssh_command(&o, &[file], &AttachTo::Newest));
        assert!(!a.last().unwrap().contains("UPLOAD"));
        assert!(a.last().unwrap().ends_with("--stdio --attach $r'"));
    }

    /// Runs the remote command in a local `sh` with `home` as `$HOME`,
    /// feeding `upload` when it asks for the agent. Returns its output.
    #[cfg(unix)]
    fn run_remote(cmd: &str, home: &Path, upload: &[u8]) -> (bool, Vec<u8>) {
        use std::io::{Read, Write};
        // ssh hands the command to the login shell: so do we.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = Some(child.stdin.take().unwrap());
        let mut stdout = child.stdout.take().unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        let mut sent = false;
        loop {
            let n = stdout.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if !sent
                && seen
                    .windows(UPLOAD_MARKER.len())
                    .any(|w| w == UPLOAD_MARKER)
            {
                // All of it, then the end (a short upload must not hang).
                let mut w = stdin.take().unwrap();
                w.write_all(upload).unwrap();
                w.flush().unwrap();
                sent = true;
            }
        }
        drop(stdin);
        (child.wait().unwrap().success(), seen)
    }

    #[cfg(unix)]
    #[test]
    fn remote_command_installs_and_updates_the_agent() {
        let home = std::env::temp_dir().join(format!("ferostream-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir(&home).unwrap();
        let arch = String::from_utf8(
            std::process::Command::new("uname")
                .arg("-m")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // A stand-in agent: prints its version and its arguments.
        let agent = |v: &str| format!("#!/bin/sh\necho agent-{v} \"$@\"\n").into_bytes();
        let file = |data: &[u8]| AgentFile {
            arch: arch.clone(),
            path: "unused".into(),
            crc: cksum(data),
            size: data.len() as u64,
        };
        let installed = home.join(".local/bin/ferostream-agent");

        // Missing: asked for, installed, started.
        let v1 = agent("one");
        let cmd = remote_command(
            &[
                AgentFile {
                    arch: "other_arch".into(),
                    ..file(b"zz")
                },
                file(&v1),
            ],
            &AttachTo::New,
            &Place::default(),
        );
        let (ok, out) = run_remote(&cmd, &home, &v1);
        assert!(ok);
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.starts_with(&format!("\0FEROSTREAM-UPLOAD {arch}\n")),
            "{text:?}"
        );
        assert!(text.ends_with("agent-one --stdio\n"), "{text:?}");
        assert_eq!(std::fs::read(&installed).unwrap(), v1);

        // The same: not asked for again.
        let (ok, out) = run_remote(&cmd, &home, b"");
        assert!(ok);
        assert_eq!(out, b"agent-one --stdio\n");

        // Different: replaced.
        let v2 = agent("two");
        let cmd = remote_command(
            &[file(&v2)],
            &AttachTo::Id("0123456789ab".into()),
            &Place::default(),
        );
        let (ok, out) = run_remote(&cmd, &home, &v2);
        assert!(ok);
        assert!(
            String::from_utf8_lossy(&out).ends_with("agent-two --stdio --attach 0123456789ab\n")
        );

        // A transfer that breaks off: the old agent stays, nothing starts.
        let v3 = agent("three");
        let cmd = remote_command(&[file(&v3)], &AttachTo::New, &Place::default());
        let (ok, out) = run_remote(&cmd, &home, &v3[..5]);
        assert!(!ok);
        assert!(!String::from_utf8_lossy(&out).contains("agent-"));
        assert_eq!(std::fs::read(&installed).unwrap(), v2);
        assert!(!home.join(".local/bin/ferostream-agent.new").exists());

        // No agent for this host's architecture: whatever is there runs.
        let cmd = remote_command(
            &[AgentFile {
                arch: "other_arch".into(),
                ..file(&v3)
            }],
            &AttachTo::Newest,
            &Place::default(),
        );
        let (ok, out) = run_remote(&cmd, &home, b"");
        assert!(ok);
        assert_eq!(out, b"agent-two --stdio --attach\n");

        // Another place, removed when done: the flag goes to that agent.
        let place = Place::new("~/apps/fs/ferostream-agent", true).unwrap();
        let cmd = remote_command(&[file(&v1)], &AttachTo::New, &place);
        let (ok, out) = run_remote(&cmd, &home, &v1);
        assert!(ok);
        assert!(
            String::from_utf8_lossy(&out).ends_with("agent-one --stdio --remove-when-done\n"),
            "{out:?}"
        );
        let other = home.join("apps/fs/ferostream-agent");
        assert_eq!(std::fs::read(&other).unwrap(), v1);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&other).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // Not there and no copy: the one on PATH runs, never with the flag.
        let place = Place::new("~/nowhere/ferostream-agent", true).unwrap();
        let cmd = remote_command(&[], &AttachTo::New, &place);
        assert!(cmd.contains("p=ferostream-agent; r=;"));
        std::fs::remove_dir_all(&home).unwrap();
    }
}
