# mmux

A persistent, tmux-like terminal multiplexer served to your browser. mmux keeps
the PTYs and terminal state in a long-lived user process, so closing a browser
tab (or losing the network) never interrupts the agents, shells, or editors
running inside it. Several browsers can attach to the same session and share
the same panes.

- Four-slot pane grid: **top-left**, **top-right**, **bottom-left**, **bottom-right**
- Vertical project list; projects can be created, renamed (double-click), and
  closed
- A new project starts the configured pane template in a working directory
  chosen through an interactive directory picker
- No projects are open on a fresh install
- Projects, their working directories, and their pane definitions persist
  across mmux restarts
- Copy with `Ctrl+Insert`, `Ctrl+Shift+C`, or `Ctrl+C` when text is selected
  (`Ctrl+C` without a selection is still sent as SIGINT); OSC 52 copies from
  programs are forwarded to the browser clipboard
- `Ctrl+Shift+Arrow` moves focus between panes (up, down, left, right)
- Browser-reserved Ctrl shortcuts (Ctrl+T, Ctrl+W, Ctrl+N, ...) cannot be
  captured by a web page. Use the mmux Ctrl prefix: `Ctrl+Shift+Space`, then
  the key without Ctrl. For example, `Ctrl+Shift+Space`, `T` sends Ctrl+T to
  pi. The prefix lasts two seconds.
- Paste with `Ctrl+Shift+V` or `Shift+Insert`; right-click also works
- Some browsers reserve `Ctrl+Shift+C` for DevTools. mmux intercepts it in the
  capture phase; if your browser still handles it first, use `Ctrl+Insert` or
  `Ctrl+C` with a selection.
- Linux username/password login through PAM; no mmux-specific accounts
- Server-side terminal emulation with reconnect snapshots and incremental
  screen diffs
- Small, dependency-light frontend; xterm.js is vendored, no CDN required
- Fixed Solarized appearance (`dark` or `light`) for the UI and terminal
- Relative URLs, so it can be mounted behind a path-prefix reverse proxy

The default new-project layout is two `pi` sessions on the left,
`emacs -nw` in the top-right, and a shell in the bottom-right. Every part of
that is configurable.

## Requirements

- Linux
- Rust 1.88 or newer (edition 2024)
- `libpam0g-dev` (Linux-PAM headers) to build
- A PAM service that permits password authentication for the account mmux runs
  as. The default is the standard `login` stack.

Build dependencies on Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libpam0g-dev
```

## Build and run

```sh
cargo build --release
install -Dm755 target/release/mmux ~/.local/bin/mmux

# Optional: write the documented defaults, then edit them.
mkdir -p ~/.config
mmux --print-default-config > ~/.config/mmux.toml

mmux
```

Open <http://127.0.0.1:8787> and sign in with the password of the Unix account
that runs mmux. mmux always spawns panes as that account; it refuses to
authenticate any other user.

Configuration is read from `~/.config/mmux.toml` by default.
`--config PATH` and `MMUX_CONFIG` select another file. `--bind ADDR` and
`MMUX_BIND` override the listen address.

## Projects and persistence

The vertical list contains **projects**. Each project owns up to four panes
and a working directory. Projects are durable: after creation, rename, close,
and shutdown, mmux writes their names, working directories, and pane
definitions to:

```text
$XDG_STATE_HOME/mmux/projects.json
```

On the next start, mmux restores those projects and starts fresh panes in their
saved working directories. Running processes cannot be serialized, so agents
and editors are relaunched; the project list itself is preserved across
restarts and reboots. Pi panes using the default `{session_id}` placeholder
resume their conversation history because the resolved
`--session-id` is persisted with the project. Panes created before the
placeholder existed keep their original commands; if they are pi sessions,
resume them manually with `pi --resume` or recreate the project. If a saved
directory or command no longer exists, the project is still restored and the
affected pane reports the startup error and keeps retrying.

Delete the state file to start with no projects.

## How sessions behave

- PTYs, terminal state, projects, and pane processes live in the mmux server. A
  browser WebSocket is only a viewer.
- Disconnecting all browsers pauses nothing. Reconnecting (or opening a second
  browser) replays the current screen.
- When a pane's command exits, mmux starts a new instance of the same command
  after a short delay. Closing the project or stopping mmux does not restart it.
- Restarting mmux ends the running processes, but restores the persisted
  projects and starts their panes again.
- Multiple browsers share input and output for a pane, like attaching to the
  same tmux window.
- Shared pane size is the smallest viewport among attached browsers, also like
  tmux. With no browsers attached the last size is retained.
- Restarting the mmux service ends its panes. If you need work to survive a
  server restart, do not stop the service.

## Configuration

See [`config.example.toml`](config.example.toml) for every option and its
default. The parts worth calling out:

```toml
[server]
bind = "127.0.0.1:8787"

[auth]
pam_service = "login"    # or a dedicated /etc/pam.d/mmux
session_hours = 24

[workspace]
root = "~"               # directory picker confinement

[terminal]
rows = 30
cols = 100
appearance = "dark"    # "dark" or "light"

# Used for every newly created project. Projects are created from the UI in
# the directory chosen there, and persisted for the next mmux start.
[[new_project.panes]]
slot = "top-left"
command = ["pi", "--session-id", "{session_id}"]

[[new_project.panes]]
slot = "bottom-left"
command = ["pi", "--session-id", "{session_id}"]

[[new_project.panes]]
slot = "top-right"
command = ["emacs", "-nw"]

[[new_project.panes]]
slot = "bottom-right"
command = []
```

- `slot` is one of `top-left`, `top-right`, `bottom-left`, `bottom-right`.
  Unused slots are omitted and the grid collapses accordingly.
- An omitted or empty `command` starts `$SHELL`.
- `env` adds per-pane environment variables.
- `terminal.appearance` is fixed to `dark` or `light` rather than following
  each browser. Panes can start before a browser connects, and TUI programs
  query terminal colors at startup, so a single reported palette keeps their
  colors and backgrounds consistent.
- `{session_id}` is replaced once per pane when a project is created with a
  fresh UUID. The resolved command is stored with the project, so programs that
  accept a stable session ID can resume after a restart. The default `pi`
  commands pass it via `--session-id`. Commands without the placeholder are
  left untouched.
- Pane commands inherit mmux's environment. Under systemd, set a suitable
  `PATH` (or use absolute program paths). The supplied unit starts mmux
  through a login shell and sets a PATH that covers the default `pi` panes;
  adjust it if your tools live elsewhere.
- The directory picker is confined to `workspace.root`. Paths are
  canonicalized; symlinks that escape the root are rejected. Set
  `workspace.root = "/"` if you want unrestricted browsing.

## Persistent user service

A systemd user unit is provided in [`contrib/mmux.service`](contrib/mmux.service).

```sh
install -Dm644 contrib/mmux.service ~/.config/systemd/user/mmux.service
systemctl --user daemon-reload
systemctl --user enable --now mmux
journalctl --user -u mmux -f
```

To keep mmux running when you are not logged in, enable lingering once:

```sh
loginctl enable-linger "$USER"
```

Stopping the unit terminates all panes (`KillMode=control-group`). That is the
server lifetime, just like killing a tmux server.

## Security notes

mmux is a browser front end to a login shell. Treat access to it as equivalent
to shell access as the service user. It refuses to run as root; run it as the
user whose sessions it should host.

- Keep the default loopback bind and use a TLS reverse proxy for remote access,
  or forward the port over SSH:
  `ssh -L 8787:127.0.0.1:8787 host`.
- mmux does not terminate TLS itself. If you bind a non-loopback address it
  logs a warning. When serving through HTTPS, also set
  `server.secure_cookies = true`.
- Authentication is delegated to Linux-PAM. Failed login attempts are rate
  limited, and the standard `login` PAM stack includes its own failure delay.
- Session cookies are `HttpOnly`, `SameSite=Strict`, and expire after
  `auth.session_hours` of inactivity.
- State-changing API requests require the `X-MMUX: 1` header and JSON bodies,
  which cross-site forms cannot send.
- The CSP only allows same-origin scripts/styles and same-origin WebSockets.

A minimal Caddy reverse proxy:

```caddyfile
mmux.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

### Behind a path prefix

mmux uses relative asset, API, redirect, and WebSocket URLs, so it can be
mounted under a prefix such as `/mmux/`. The proxy must preserve the prefix in
the browser and strip it from the upstream request. For example, with
rpanel:

```yaml
panel:
  links:
    mmux: /mmux/
  status:
    status:
      - name: mmux
        user: true

groups:
  "mmux":
    host: "127.0.0.1:8787"
    https: false
    paths:
      "/mmux": "/"
      "/mmux/*": "/*"
```

rpanel tunnels the WebSocket upgrade automatically. If your proxy needs a
trailing slash redirect for `/mmux` -> `/mmux/`, include both the exact and
wildcard routes as above.

## Development

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

The test suite includes real PTY tests (spawning `sh`), HTTP API tests with a
mock authenticator, and a WebSocket test that verifies rendering, resizing, and
shared input.

To exercise PAM against a real account manually:

```sh
MMUX_TEST_PAM_USER="$USER" MMUX_TEST_PAM_PASSWORD='…' \
  cargo test pam_accepts_a_real_password -- --ignored
```

Frontend assets live in `web/`. In debug builds `rust-embed` reads them from
disk, so a browser reload picks up edits; release builds embed them in the
binary.

## Layout

```text
src/
  auth.rs          PAM authentication + Unix account detection
  config.rs        TOML config, defaults, validation
  persistence.rs   durable project registry
  session/         projects, panes, PTY ownership, terminal snapshots
  web/             axum routes, login/session handling, WebSocket protocol
  workspace.rs     confined directory picker
web/               embedded frontend (vendored xterm.js)
tests/web.rs       end-to-end HTTP + WebSocket tests
```

## License

MIT. The vendored xterm.js and fit addon are MIT licensed; their license files
are in `web/vendor/`.
