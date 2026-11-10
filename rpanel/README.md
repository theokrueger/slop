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

## Configuration

See [`rpanel.example.yaml`](rpanel.example.yaml) for a complete example.

### `server`

| Key | Default | Description |
| --- | --- | --- |
| `log` | stdout | File to append logs to. |
| `host` | `127.0.0.1` | Listen address. |
| `port` | `80` | Listen port. |
| `allow_insecure` | `false` | Serve plain HTTP instead of HTTPS. |
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
| `status.network` | `true` | Throughput over several windows. |
| `status.status` | `[]` | systemd units to report. |
| `links` | `{}` | Display name -> URL, rendered in order. |

`status.status` entries are either a unit name (`tary`) or a mapping for a user
unit:

```yaml
status:
  - tary
  - name: my-user-service
    user: true
```

Network throughput is sampled every 10 seconds and reported for now, the last
5 minutes, hour, day, month, and since rpanel started. Loopback interfaces are
excluded.

### `groups`

Each group has an upstream `host`, an `https` flag, and a map of incoming path
patterns to upstream path patterns.

```yaml
groups:
  "Name":
    host: "127.0.0.1:8080"
    https: false
    paths:
      "/": "/name"
      "/api/*": "/nameapi/v1/*"
  "Tary":
    host: "cow.local:3001"
    https: false
    paths:
      "/*": "/tary/*"
```

Path matching:

- Exact patterns (`/`, `/health`) match only that path.
- A trailing `/*` matches that prefix and captures the remainder.
  `/api/*` matches `/api/request` with capture `request`.
- The wildcard is substituted into the upstream pattern:
  `/api/request` -> `/nameapi/v1/request`.
- Exact routes win over wildcards; longer wildcard prefixes win over shorter
  ones.
- Query strings are preserved.
- Unmatched paths return `404`.

The proxy forwards the method, body, and end-to-end headers, drops hop-by-hop
headers, rewrites `Host`, and adds `X-Forwarded-For`, `X-Forwarded-Host`,
`X-Forwarded-Proto`, and `X-Real-IP`.

## Notes

- HTTPS upstreams use the platform's native root certificates.
- Storage reporting omits virtual filesystems such as `proc`, `sysfs`, and
  `tmpfs`.
- The panel is only registered when `panel.enable` is true. If it is disabled,
  a group with a `/*` or `/` route can serve the root path.
- `systemctl` is invoked with a 2 second timeout per unit. If it is unavailable,
  the panel shows an error for that unit instead of failing the page.
- Logging defaults to `info`. Set `RUST_LOG` (for example `RUST_LOG=debug`) to
  change the filter.
- The server shuts down gracefully on Ctrl-C or SIGTERM.

## Tests

```sh
cargo test
```

The test suite covers config parsing and validation, path matching and
rewriting, network rate calculations, panel rendering, and an end-to-end proxy
test against a local upstream.
