use std::{fmt::Write, sync::Arc, time::Instant};

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};

use crate::{
    config::{PanelConfig, StatusConfig},
    net::Rates,
    state::AppState,
    status::{self, RamInfo, ServiceStatus, StorageInfo, SystemStatus},
};

pub async fn handler(State(state): State<Arc<AppState>>) -> Response {
    let Some(panel) = state.panel.as_ref() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "panel unavailable\n").into_response();
    };
    let status = status::collect(&panel.config.status).await;
    let rates = panel.network.rates(Instant::now());
    let mut response = Html(render(&panel.config, &status, &rates)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

pub fn render(config: &PanelConfig, status: &SystemStatus, rates: &Rates) -> String {
    let show_status = has_status(&config.status);
    let refresh = if config.refresh_seconds > 0 && show_status {
        format!(
            r#"<meta http-equiv="refresh" content="{}">"#,
            config.refresh_seconds
        )
    } else {
        String::new()
    };

    let mut html = String::with_capacity(8192);
    let _ = write!(
        html,
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>rpanel</title>
{refresh}
<style>{CSS}</style>
</head>
<body>
<main class="{}">
<section class="links">
<header>
<h1>rpanel</h1>
<p class="muted">Reverse proxy panel</p>
</header>
<nav>
"#,
        if show_status { "" } else { "single" }
    );

    if config.links.is_empty() {
        html.push_str(r#"<p class="muted">No links configured.</p>"#);
    } else {
        for (name, url) in &config.links {
            let _ = write!(
                html,
                r#"<a class="link" href="{}"><span>{}</span><span class="arrow">→</span></a>"#,
                escape(url),
                escape(name)
            );
        }
    }

    html.push_str("</nav>\n</section>\n");

    if show_status {
        html.push_str(r#"<aside class="status">"#);
        if config.status.uptime {
            render_uptime(&mut html, status.uptime);
        }
        if config.status.ram {
            render_ram(&mut html, status.ram.as_ref());
        }
        if config.status.storage {
            render_storage(&mut html, &status.storage);
        }
        if config.status.network {
            render_network(&mut html, rates);
        }
        if !config.status.status.is_empty() {
            render_services(&mut html, &status.services);
        }
        html.push_str("</aside>\n");
    }

    html.push_str("</main>\n<footer>rpanel</footer>\n</body>\n</html>\n");
    html
}

fn has_status(config: &StatusConfig) -> bool {
    config.storage || config.ram || config.uptime || config.network || !config.status.is_empty()
}

fn render_uptime(html: &mut String, uptime: Option<u64>) {
    let value = uptime
        .map(format_duration)
        .unwrap_or_else(|| "unavailable".into());
    let _ = write!(
        html,
        r#"<section class="card"><h2>Uptime</h2><div class="row"><span>System</span><span>{}</span></div></section>"#,
        escape(&value)
    );
}

fn render_ram(html: &mut String, ram: Option<&RamInfo>) {
    html.push_str(r#"<section class="card"><h2>Memory</h2>"#);
    match ram {
        Some(ram) => {
            let _ = write!(
                html,
                r#"<div class="row"><span>RAM</span><span>{} / {} ({}%)</span></div>{}"#,
                format_bytes(ram.used),
                format_bytes(ram.total),
                percent(ram.used, ram.total).round(),
                bar(ram.used, ram.total)
            );

            if ram.swap_total > 0 {
                let _ = write!(
                    html,
                    r#"<div class="row"><span>Swap</span><span>{} / {} ({}%)</span></div>{}"#,
                    format_bytes(ram.swap_used),
                    format_bytes(ram.swap_total),
                    percent(ram.swap_used, ram.swap_total).round(),
                    bar(ram.swap_used, ram.swap_total)
                );
            } else {
                html.push_str(
                    r#"<div class="row"><span>Swap</span><span class="muted">not configured</span></div>"#,
                );
            }
        }
        None => html.push_str(r#"<p class="muted">unavailable</p>"#),
    }
    html.push_str("</section>");
}

fn render_storage(html: &mut String, storage: &[StorageInfo]) {
    html.push_str(r#"<section class="card"><h2>Storage</h2>"#);
    if storage.is_empty() {
        html.push_str(r#"<p class="muted">No mounted drives found.</p>"#);
    } else {
        for disk in storage {
            let _ = write!(
                html,
                r#"<div class="storage-row"><div class="row"><span class="mono">{}</span><span>{} · {} / {} ({}%)</span></div>{}</div>"#,
                escape(&disk.mount),
                escape(&disk.fs),
                format_bytes(disk.used),
                format_bytes(disk.total),
                percent(disk.used, disk.total).round(),
                bar(disk.used, disk.total)
            );
        }
    }
    html.push_str("</section>");
}

fn render_network(html: &mut String, rates: &Rates) {
    let rows = [
        ("Now", rates.now),
        ("5 min", rates.five_min),
        ("1 hour", rates.hour),
        ("1 day", rates.day),
        ("1 month", rates.month),
        ("All time", rates.all_time),
    ];
    html.push_str(
        r#"<section class="card"><h2>Network</h2><table><thead><tr><th>Window</th><th>Down</th><th>Up</th></tr></thead><tbody>"#,
    );
    for (label, rate) in rows {
        let _ = write!(
            html,
            "<tr><th>{}</th><td>{}</td><td>{}</td></tr>",
            label,
            format_rate(rate.rx),
            format_rate(rate.tx)
        );
    }
    html.push_str("</tbody></table></section>");
}

fn render_services(html: &mut String, services: &[ServiceStatus]) {
    html.push_str(r#"<section class="card"><h2>Services</h2><ul class="services">"#);
    for service in services {
        let (class, label) = if service.error.is_some() {
            ("bad", "error")
        } else {
            match service.state.as_str() {
                "active" => ("ok", "active"),
                "failed" => ("bad", "failed"),
                "inactive" => ("idle", "inactive"),
                other => ("idle", other),
            }
        };
        let detail = if let Some(error) = service.error.as_deref().filter(|error| !error.is_empty())
        {
            error.to_owned()
        } else if service.sub_state.is_empty() {
            service.description.clone()
        } else if service.description.is_empty() {
            service.sub_state.clone()
        } else {
            format!("{} · {}", service.sub_state, service.description)
        };
        let user = if service.user { " · user" } else { "" };
        let _ = write!(
            html,
            r#"<li><span class="dot {class}"></span><div class="service-body"><div class="row"><span>{}<span class="muted">{user}</span></span><span class="badge {class}">{}</span></div><div class="muted">{}</div></div></li>"#,
            escape(&service.name),
            escape(label),
            escape(&detail)
        );
    }
    html.push_str("</ul></section>");
}

pub fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_rate(bytes_per_second: f64) -> String {
    format!(
        "{}/s",
        format_bytes(bytes_per_second.max(0.0).round() as u64)
    )
}

pub fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn bar(used: u64, total: u64) -> String {
    format!(
        r#"<div class="bar"><span style="width:{:.1}%"></span></div>"#,
        percent(used, total)
    )
}

const CSS: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f6f7f9;
  --fg: #1b1f24;
  --muted: #667085;
  --card: #ffffff;
  --border: #e4e7ec;
  --accent: #2563eb;
  --ok: #16a34a;
  --bad: #dc2626;
  --idle: #d97706;
  --bar: #e9edf2;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0f1115;
    --fg: #e6e8eb;
    --muted: #98a2b3;
    --card: #171a21;
    --border: #262b35;
    --accent: #60a5fa;
    --ok: #4ade80;
    --bad: #f87171;
    --idle: #fbbf24;
    --bar: #232833;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1100px; margin: 0 auto; padding: 40px 20px; display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1.1fr); gap: 24px; align-items: start; }
main.single { grid-template-columns: 1fr; }
@media (max-width: 800px) { main { grid-template-columns: 1fr; padding: 24px 16px; } }
h1 { font-size: 1.6rem; margin: 0 0 4px; letter-spacing: -0.02em; }
h2 { font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); margin: 0 0 14px; }
p { margin: 0; }
.muted { color: var(--muted); }
.links nav { display: grid; gap: 10px; margin-top: 24px; }
.link { display: flex; justify-content: space-between; align-items: center; padding: 14px 16px; background: var(--card); border: 1px solid var(--border); border-radius: 10px; color: inherit; text-decoration: none; transition: border-color .15s, transform .15s; }
.link:hover { border-color: var(--accent); transform: translateY(-1px); }
.arrow { color: var(--muted); }
.status { display: grid; gap: 16px; }
.card { background: var(--card); border: 1px solid var(--border); border-radius: 12px; padding: 18px; }
.row { display: flex; justify-content: space-between; gap: 12px; align-items: baseline; }
.bar { height: 6px; background: var(--bar); border-radius: 999px; overflow: hidden; margin: 8px 0 14px; }
.bar span { display: block; height: 100%; background: var(--accent); border-radius: inherit; }
.bar:last-child { margin-bottom: 0; }
.storage-row + .storage-row { margin-top: 14px; }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; }
table { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
th, td { text-align: right; padding: 6px 0; }
th:first-child, td:first-child { text-align: left; }
thead th { color: var(--muted); font-weight: 500; font-size: 0.8rem; }
tbody th { font-weight: 500; color: var(--muted); }
.services { list-style: none; margin: 0; padding: 0; display: grid; gap: 12px; }
.services li { display: flex; gap: 10px; align-items: flex-start; }
.service-body { flex: 1; }
.dot { width: 8px; height: 8px; border-radius: 50%; margin-top: 7px; flex: none; background: var(--idle); }
.dot.ok { background: var(--ok); }
.dot.bad { background: var(--bad); }
.badge { font-size: 0.75rem; padding: 2px 8px; border-radius: 999px; background: var(--bar); color: var(--muted); }
.badge.ok { color: var(--ok); }
.badge.bad { color: var(--bad); }
footer { max-width: 1100px; margin: 0 auto; padding: 0 20px 32px; color: var(--muted); font-size: 0.8rem; }
"#;

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::{
        config::Service,
        net::Rate,
        status::{RamInfo, ServiceStatus, StorageInfo},
    };

    fn config() -> PanelConfig {
        PanelConfig {
            enable: true,
            refresh_seconds: 10,
            status: StatusConfig {
                storage: true,
                ram: true,
                uptime: true,
                network: true,
                status: vec![Service::System("tary".into())],
            },
            links: IndexMap::from([
                ("Tary".to_string(), "/tary".to_string()),
                ("Name".to_string(), "/name".to_string()),
            ]),
        }
    }

    fn status() -> SystemStatus {
        SystemStatus {
            storage: vec![StorageInfo {
                mount: "/".into(),
                fs: "ext4".into(),
                total: 100 * 1024 * 1024 * 1024,
                used: 25 * 1024 * 1024 * 1024,
            }],
            ram: Some(RamInfo {
                used: 4 * 1024 * 1024 * 1024,
                total: 16 * 1024 * 1024 * 1024,
                swap_used: 0,
                swap_total: 0,
            }),
            uptime: Some(90_061),
            services: vec![ServiceStatus {
                name: "tary".into(),
                user: false,
                state: "active".into(),
                sub_state: "running".into(),
                description: "Tary service".into(),
                error: None,
            }],
        }
    }

    fn rates() -> Rates {
        Rates {
            now: Rate {
                rx: 1024.0,
                tx: 2048.0,
            },
            five_min: Rate {
                rx: 100.0,
                tx: 200.0,
            },
            hour: Rate::default(),
            day: Rate::default(),
            month: Rate::default(),
            all_time: Rate::default(),
        }
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn formats_bytes_and_durations() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(10 * 1024), "10 KiB");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(3_661), "1h 1m");
        assert_eq!(format_duration(90_061), "1d 1h 1m");
    }

    #[test]
    fn renders_links_and_status() {
        let html = render(&config(), &status(), &rates());
        assert!(html.contains(r#"href="/tary""#));
        assert!(html.contains(">Tary<"));
        assert!(html.contains("Uptime"));
        assert!(html.contains("1d 1h 1m"));
        assert!(html.contains("Memory"));
        assert!(html.contains("Storage"));
        assert!(html.contains("Network"));
        assert!(html.contains("running · Tary service"));
        assert!(html.contains(r#"http-equiv="refresh" content="10""#));
    }

    #[test]
    fn omits_disabled_sections() {
        let mut config = config();
        config.status.storage = false;
        config.status.ram = false;
        config.status.uptime = false;
        config.status.network = false;
        config.status.status.clear();
        let html = render(&config, &status(), &rates());
        assert!(!html.contains("Storage"));
        assert!(!html.contains("Memory"));
        assert!(!html.contains("Uptime"));
        assert!(!html.contains("Network"));
        assert!(!html.contains("http-equiv=\"refresh\""));
        assert!(html.contains("main class=\"single\""));
    }

    #[test]
    fn escapes_link_names_and_urls() {
        let mut config = config();
        config.links = IndexMap::from([("<script>".to_string(), "\"x\"".to_string())]);
        let html = render(&config, &status(), &rates());
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("href=\"&quot;x&quot;\""));
        assert!(!html.contains("<script>"));
    }
}
