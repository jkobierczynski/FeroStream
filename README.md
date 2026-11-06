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
windows appear, pixels stream (LZ4 tiles), mouse and keyboard work, titles
and icons show on the taskbar, text is sized for your screen's scaling, the
pointer takes the program's shapes, dialogs stay above their window, menus
and tooltips have no frame, size limits hold, and minimise, maximise and
full screen work from either side. Copied text travels both ways, programs
know your screen layout, and open menus close when you click into another
Windows program. Closing the window or exiting the program ends the
session. Not yet: screens with different scaling, smarter compression,
QUIC, reattaching to a running session.

Copied text: what a program copies lands on the Windows clipboard; what you
copy on Windows reaches the host when you switch to one of its windows
(Ctrl+V in programs, and middle click or Shift+Insert in xterm).
`--clipboard to-host`, `from-host` or `off` limits that, for example for a
host you trust less.

Typing goes through your Windows keyboard layout: the client sends the
characters Windows produces (dead keys and AltGr included), so the host
needs no matching layout. Keys without text (arrows, F-keys) and shortcuts
(Ctrl+C, Alt+F) go as key names.

## Trying it

On the Linux host, install Xvfb and put the agent where SSH finds it:

```
sudo apt install xvfb                      # Fedora: xorg-x11-server-Xvfb
mkdir -p ~/.local/bin
cp ferostream-agent ~/.local/bin/          # from the CI artifact
```

On Windows:

```
ferostream.exe [-p PORT] [-i KEY] [-J JUMP] [user@]host [program [args...]]
```

KDE and Qt programs show their icons when the host has an icon theme
(`sudo apt install breeze-icon-theme`); the agent tells them to use it.

The program defaults to `xterm`. Passwords and host-key questions appear in a
small dialog. `ferostream.exe --help` lists every option.

## Security

- The agent never trusts the client and the client never trusts the agent:
  every frame is length-checked before anything is allocated, and the decoder
  is fuzzed (`fuzz/`).
- Unlike X11 forwarding, a remote program cannot read your Windows keystrokes
  or screen: it only ever sees the input you send to its own windows.
- The Xvfb display is reachable only through a Unix socket and a random
  cookie in a file only you can read (`-nolisten tcp`).

## Building

```
cargo test --workspace                        # Linux: also runs xterm under Xvfb
cargo build --release -p ferostream-client    # Windows
cargo build --release -p ferostream-agent --target x86_64-unknown-linux-musl
```

## Licence

MIT or Apache-2.0, at your option.
