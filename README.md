# FeroStream

Run a Linux program on a remote host and use its windows on Windows as if
they were local: each remote window becomes a real Win32 window with its own
taskbar button, moved and resized like any other.

```
ferostream.exe me@build-box gimp
```

FeroStream is a pair of small Rust programs:

| Part | Runs on | What it is |
| --- | --- | --- |
| `ferostream.exe` | Windows | Native Win32 client, no toolkit, no installer |
| `ferostream-agent` | Linux | One static binary (about 650 KB), no root, no daemon |

The client starts the agent through your normal OpenSSH (`ssh.exe`), so
logins, keys, `~/.ssh/config`, jump hosts and host-key checks all work as they
already do. The agent runs the program on a private Xvfb display and sends
only pixels; the client sends only input. Nothing listens on the network.

The protocol lives in its own crate, [`ferostream-proto`](crates/proto), so
other clients and SSH tools can integrate it. [FeroSSH](https://github.com/jkobierczynski/FeroSSH)
will.

## Status

Steps 1 to 3 of the [plan](https://claude.ai/artifact/256vVGrd767CWXCAcpPEd1):
windows appear, pixels stream (packed tiles, see below), mouse and keyboard work, titles
and icons show on the taskbar, text is sized for your screen's scaling, the
pointer takes the program's shapes, dialogs stay above their window, menus
and tooltips have no frame, size limits hold, and minimise, maximise and
full screen work from either side. Copied text travels both ways, programs
know your screen layout, and open menus close when you click into another
Windows program. Closing the window or exiting the program ends the
session. The agent copies itself to the host when needed, and a program
keeps running when the connection drops: its windows come back when the
client reconnects. Not yet: screens with different scaling, QUIC.

Pictures stay small: the agent keeps a copy of what the client shows and
sends only pixels that really changed. A scroll (a terminal, a list, a web
page) goes as a copy of pixels the client already has. Each tile is packed
the way that suits it: one colour in 3 bytes, a palette of up to 256
colours (text, menus, toolbars), or colour rows with small differences
(photos, gradients). Then it is compressed with LZ4 (fast, little CPU) or
Deflate (fewest bytes). `--packing auto` (the default) picks from the link
speed it measures, and `fast` or `strong` fix the choice. Scrolling 60
lines in xterm took 459 KB with plain LZ4 tiles. Now it takes 26 KB with
`fast` and 23 KB with `strong`.

Copied text: what a program copies lands on the Windows clipboard; what you
copy on Windows reaches the host when you switch to one of its windows
(Ctrl+V in programs, and middle click or Shift+Insert in xterm). Text is up
to 1 MiB either way, large copies included. Only text is shared.
`--clipboard to-host`, `from-host` or `off` limits that, for example for a
host you trust less; with `to-host` or `off` the agent sends nothing it
sees copied. Text you only select with the mouse (xterm's way of copying,
the X "PRIMARY" selection) stays on the host unless you add
`--share-selection`: otherwise every selection would replace the Windows
clipboard.

Typing goes through your Windows keyboard layout: the client sends the
characters Windows produces (dead keys and AltGr included), so the host
needs no matching layout. Keys without text (arrows, F-keys) and shortcuts
(Ctrl+C, Alt+F) go as key names.

## Trying it

On the Linux host, install Xvfb:

```
sudo apt install xvfb                      # Fedora: xorg-x11-server-Xvfb
```

On Windows, keep `ferostream-agent` next to `ferostream.exe` (the
`ferostream-windows-x64` CI artifact has both) and run:

```
ferostream.exe [-p PORT] [-i KEY] [-J JUMP] [user@]host [program [args...]]
```

The client compares the host's `~/.local/bin/ferostream-agent` with its own
copy (`cksum`: CRC and size) and, when it is missing or different, sends its
copy over the same SSH connection, so there is one login. A file named
`ferostream-agent-aarch64` (or another `uname -m`) next to the exe serves
hosts of that kind. `--no-install` turns the copying off; `--agent PATH`
uses an agent you installed yourself. `--install-to PATH` copies it
somewhere else (`~/...` or an absolute path), and `--remove-agent` has the
copied agent delete itself once its program ends and no other FeroStream
session of yours runs on that host. The copy is executable by you only.

KDE and Qt programs show their icons when the host has an icon theme
(`sudo apt install breeze-icon-theme`); the agent tells them to use it.

The program defaults to `xterm`. Passwords and host-key questions appear in a
small dialog. `ferostream.exe --help` lists every option.

## When the connection drops

The program keeps running on the host. Its windows stay on screen, marked
"(reconnecting…)", and the client tries again after 1, 2, 4, 8, 15 and then
every 30 seconds; once it is back, the windows pick up where they were.
Closing the windows while it waits leaves the program running and tells you
how to come back to it:

```
ferostream.exe --attach me@host              # the session left most recently
ferostream.exe --attach=3f9c0a12b7de me@host # that one
```

On the host, `ferostream-agent --list` shows the sessions and
`ferostream-agent --stop ID` ends one. A session ends by itself when its
program does. (On hosts where logind kills a user's processes at logout,
`KillUserProcesses=yes`, nothing survives a dropped connection; tmux has the
same limit.)

## Security

- The agent never trusts the client and the client never trusts the agent:
  every frame is length-checked before anything is allocated, and the decoder
  is fuzzed (`fuzz/`).
- Unlike X11 forwarding, a remote program cannot read your Windows keystrokes
  or screen: it only ever sees the input you send to its own windows.
- The Xvfb display is reachable only through a Unix socket and a random
  cookie in a file only you can read (`-nolisten tcp`).
- A running session listens on a Unix socket in `/tmp/ferostream-<uid>/`,
  a directory only you can enter (the agent refuses one that is not). A
  new client that attaches sends the previous one away, as `tmux attach -d`
  does.
- The agent the client copies is checked on the host (size and CRC) before
  it replaces the old one; a transfer that breaks off leaves the old agent.
- The agent locks itself down on Linux once it has started Xvfb and your
  program (which keeps your normal rights, as with `ssh -X`). The session
  process and the stdio relay run with no new privileges, are not
  dumpable (no ptrace from your other processes, no core files), and carry
  a seccomp filter: no `execve`, no sockets but Unix ones, no ptrace,
  namespaces, BPF, io_uring, keyrings, module loading or `TIOCSTI`.
  Landlock (Linux 5.13 and later) confines their files to
  `/tmp/ferostream-<uid>/` (plus deleting the agent's own file with
  `--remove-when-done`; the relay gets no files at all), and refuses TCP
  and other processes' abstract Unix sockets. `ferostream-agent
  --check-sandbox` shows what a host enforces and tries the forbidden
  things.
- Fuzzed (`fuzz/`): frame decoding, the decoder a sandboxed client runs
  (upload request, frames, every tile encoding), packing and unpacking
  tiles, and the window properties the agent reads
  from programs (size hints, icons).

## Embedding the client

`ferostream-client` is also a library. `win32::start` runs a session inside
another program's message loop (several at once), with a `win32::Host`
that supplies the transport (any byte stream to `ferostream-agent
--stdio`) and hears when the session ends. `decode::Decoder` takes the
host's bytes apart (the agent upload request, then frames); with
`unpack` on it hands out compressed tiles as raw pixels, so it can run in
a separate sandboxed process while the window side accepts only raw tiles
(`Settings::raw_only`). FeroSSH embeds the client this way.

## Building

```
cargo test --workspace                        # Linux: also runs xterm under Xvfb
cargo build --release -p ferostream-client    # Windows
cargo build --release -p ferostream-agent --target x86_64-unknown-linux-musl
```

## Licence

MIT or Apache-2.0, at your option.
