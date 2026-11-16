# rpanel

A small Rust reverse proxy with a server-rendered status panel.

- Configure endpoint groups in YAML.
- Proxy each group to an HTTP or HTTPS upstream.
- Serve a clean panel at `/` with links and optional system status.
- No JavaScript required: the panel is plain HTML/CSS and refreshes with a
  `<meta http-equiv="refresh">` tag.

## Build and run

```sh
cargo build --release
./target/release/rpanel --config ~/.config/rpanel.yaml
```

The default config path is `~/.config/rpanel.yaml` (or the platform config
directory). Validate a config without starting the server:

```sh
rpanel --check --config ./rpanel.example.yaml
```

To serve on a privileged port as a user, the binary needs the
`cap_net_bind_service` file capability. Use the install script, which installs
the binary, applies the capability, and restarts the user unit:

```sh
./install.sh
```

`cargo install` on its own replaces the binary and drops the capability, so a
unit that binds port 443 will then fail with "Permission denied".

## Configuration

See [`rpanel.example.yaml`](rpanel.example.yaml) for a complete example.

### `server`

| Key | Default | Description |
| --- | --- | --- |
| `log` | stdout | File to append logs to. |
| `host` | `127.0.0.1` | Listen address. |
| `port` | `80` | Listen port. |
| `allow_insecure` | `false` | Serve plain HTTP instead of HTTPS. |
| `allow_insecure_upstream` | `false` | Accept invalid upstream TLS certificates (self-signed, wrong hostname). |
| `cert` | — | PEM certificate chain, required unless `allow_insecure`. |
| `key` | — | PEM private key, required unless `allow_insecure`. |

### `panel`

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `true` | Serve the panel at `/`. |
| `refresh_seconds` | `10` | Auto-refresh interval. `0` disables it. |
| `status.storage` | `true` | Mounted drive usage. |
| `status.ram` | `true` | RAM and swap usage. |
| `status.uptime` | `true` | System uptime. |
| `status.network` | `true` | Current speed and throughput windows. |
| `status.status` | `[]` | systemd units to report. |
| `network_state` | state dir | File that keeps the network history across restarts. |
| `links` | `{}` | Display name -> URL, rendered in order. |

`status.status` entries are either a unit name (`tary`) or a mapping for a user
unit:

```yaml
status:
  - tary
  - name: my-user-service
    user: true
```

Network counters are sampled every 10 seconds. The panel shows the current
upload/download speed for "Now", and the total bytes transferred for the last
5 minutes, hour, day, month, and all time. Loopback interfaces are excluded.

The history is written to `panel.network_state` (by default
`~/.local/state/rpanel/network.yaml`, or the platform state directory) every
minute and on shutdown, so the windows and the all-time total survive restarts
and reboots. Set `network_state` to override the location.

### `groups`

Each group has an upstream `host`, an `https` flag, a `forwarded_prefix` flag,
and a map of incoming path patterns to upstream path patterns.

| Key | Default | Description |
| --- | --- | --- |
| `host` | — | Upstream authority, e.g. `127.0.0.1:8080` or `cow.local:3001`. |
| `https` | `false` | Connect to the upstream over TLS. |
| `paths` | `{}` | Incoming path pattern -> upstream path pattern. |
| `forwarded_prefix` | `false` | Send `X-Forwarded-Prefix` with the matched incoming path prefix (no trailing slash). |
| `rewrite` | `false` | Rewrite root-absolute URLs in responses to the group's path prefix. |
| `preserve_origin` | `false` | Forward `Origin`/`Referer` unchanged instead of rewriting them to the upstream origin. |
| `cookie_namespace` | `false` | Keep the group's cookies under `rp_<prefix>_<name>` so apps on the same origin cannot shadow each other's sessions. |

```yaml
groups:
  "Name":
    host: "127.0.0.1:8080"
    https: false
    paths:
      "/name": "/"
      "/name/*": "/*"
  "Tary":
    host: "cow.local:3001"
    https: false
    forwarded_prefix: true
    paths:
      "/tary": "/"
      "/tary/*": "/*"
  "API":
    host: "127.0.0.1:8081"
    https: false
    paths:
      "/api/*": "/v1/*"
```

Path matching:

- Exact patterns (`/`, `/health`) match only that path.
- A trailing `/*` matches that prefix and captures the remainder.
  `/api/*` matches `/api/request` with capture `request`.
- The wildcard is substituted into the upstream pattern:
  `/api/request` -> `/v1/request`, and `/name/request` -> `/request`.
- Exact routes win over wildcards; longer wildcard prefixes win over shorter
  ones.
- If both `/x` and `/x/*` are configured, a request for `/x` is redirected
  (308) to `/x/` so relative URLs resolve under the wildcard route.
- Query strings are preserved.
- Unmatched paths return `404`.

The proxy forwards the method, body, and end-to-end headers, drops hop-by-hop
headers, rewrites `Host`, and adds `X-Forwarded-For` (appended to any
client-supplied value), `X-Forwarded-Host`, `X-Forwarded-Proto`, and
`X-Real-IP`. Groups with `forwarded_prefix` also get `X-Forwarded-Prefix`.

## Notes

- HTTPS upstreams use the platform's native root certificates.
- For same-origin requests, `Origin` and `Referer` are rewritten to the
  upstream origin so upstream CSRF checks keep working. Cross-origin values
  are forwarded unchanged, so the upstream still rejects them.
- `X-Forwarded-For` appends the client address to any value the client sent. If
  rpanel is directly exposed to untrusted clients and an upstream trusts that
  header for authorization, strip or ignore it upstream.
- The panel is served with `Content-Security-Policy`, `X-Content-Type-Options`,
  and `Referrer-Policy` headers and is never cached.
- Storage reporting omits virtual filesystems such as `proc`, `sysfs`, and
  `tmpfs`.
- The panel is only registered when `panel.enable` is true. If it is disabled,
  a group with a `/*` or `/` route can serve the root path.
- `systemctl` is invoked with a 2 second timeout per unit. If it is unavailable,
  the panel shows an error for that unit instead of failing the page.
- Logging defaults to `info`. Set `RUST_LOG` (for example `RUST_LOG=debug`) to
  change the filter.
- `preserve_origin: true` forwards `Origin` and `Referer` as the client sent
  them. rpanel normally rewrites same-origin values to the upstream origin so
  apps that compare them against `Host` accept the request; apps that instead
  compare them against `X-Forwarded-Host` (reverse-proxy aware, e.g.
  qBittorrent) reject that as cross-site and need this option. Default ports
  are canonicalized out of both that header and `X-Forwarded-Host`, because
  browsers omit `:443` while a `Host: example:443` would otherwise not match.
- `cookie_namespace: true` renames the group's cookies to
  `rp_<prefix>_<name>` for the browser and restores them for the upstream.
  Every app served under one origin shares the browser's cookie jar, so two
  apps using `SID` (or a stale cookie left over from an earlier setup) shadow
  each other and can cause endless login redirects. Stale un-namespaced
  cookies of the same name are dropped on the way upstream.
- `rewrite: true` on a group makes rpanel do what nginx `sub_filter` and
  `proxy_redirect` do for upstreams that do not support a base path. It
  prefixes root-absolute URLs in HTML attribute values (`href="/x"`,
  `src="/x"`, `action`, `poster`) and in CSS `url(/x)`, and rewrites
  `Location` redirects that point at the upstream root. Requests to that group
  drop `Accept-Encoding` so bodies arrive uncompressed, and bodies over 4 MiB
  are streamed through unmodified. Script strings are deliberately not
  rewritten: touching every quoted `"/` corrupts regex literals in vendored
  libraries (qBittorrent's MooTools broke this way), and paths built in
  JavaScript are better handled by the app's own base-path setting.
- The server shuts down gracefully on Ctrl-C or SIGTERM.

## Tests

```sh
cargo test
```

The test suite covers config parsing and validation, path matching and
rewriting, network rate calculations, panel rendering, and an end-to-end proxy
test against a local upstream.
