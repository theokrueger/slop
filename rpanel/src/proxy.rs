use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use tracing::warn;

use crate::{
    route::Route,
    state::{AppState, ProxyClient},
};

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(60);

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");
const X_FORWARDED_PREFIX: HeaderName = HeaderName::from_static("x-forwarded-prefix");

pub async fn handler(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let path = req.uri().path();
    if let Some(location) = trailing_slash_redirect(&state.routes, path, req.uri().query()) {
        return (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response();
    }
    let Some(route) = state
        .routes
        .iter()
        .find(|route| route.capture(path).is_some())
    else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };

    let Some(uri) = route.upstream_uri(path, req.uri().query()) else {
        warn!(group = %route.group, path = %path, "could not build upstream URI");
        return (StatusCode::BAD_GATEWAY, "bad gateway\n").into_response();
    };

    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let proto = if state.config.server.allow_insecure {
        "http"
    } else {
        "https"
    };

    let result = if is_upgrade_request(&req) {
        forward_upgrade(&state.client, route, uri, req, client_ip, proto).await
    } else {
        forward(&state.client, route, uri, req, client_ip, proto).await
    };
    match result {
        Ok(response) => response,
        Err(error) => {
            warn!(group = %route.group, %error, "upstream request failed");
            (StatusCode::BAD_GATEWAY, "bad gateway\n").into_response()
        }
    }
}

/// Whether the request asks to switch protocols (a WebSocket handshake, or any
/// other `Connection: Upgrade` request).
fn is_upgrade_request(req: &Request) -> bool {
    req.headers().contains_key(header::UPGRADE)
        && req
            .headers()
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// Proxies a protocol-upgrade request (WebSocket) by tunnelling the two upgraded
/// connections after the upstream answers `101 Switching Protocols`.
async fn forward_upgrade(
    client: &ProxyClient,
    route: &Route,
    uri: Uri,
    mut req: Request,
    client_ip: Option<IpAddr>,
    proto: &'static str,
) -> Result<Response> {
    // Capture the downstream upgrade before splitting the request: `Parts` is
    // not upgradeable, but `&mut Request` is.
    let downstream = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();

    let mut request = Request::new(body);
    *request.version_mut() = hyper::Version::HTTP_11;
    copy_upstream_headers(request.headers_mut(), &parts, route, client_ip, proto, true)?;
    *request.method_mut() = parts.method;
    *request.uri_mut() = uri;

    let mut response = tokio::time::timeout(UPSTREAM_TIMEOUT, client.request(request))
        .await
        .context("upstream timed out")?
        .context("upstream request failed")?;

    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        // The upstream declined the upgrade; relay its response as a normal one.
        return Ok(relay_response(response));
    }

    let upstream = hyper::upgrade::on(&mut response);

    tokio::spawn(async move {
        let (down, up) = match (downstream.await, upstream.await) {
            (Ok(down), Ok(up)) => (down, up),
            (down, up) => {
                if let Err(error) = down {
                    warn!(%error, "downstream upgrade failed");
                }
                if let Err(error) = up {
                    warn!(%error, "upstream upgrade failed");
                }
                return;
            }
        };
        let mut down = TokioIo::new(down);
        let mut up = TokioIo::new(up);
        // Copy both directions until each side closes its write half, so a
        // client half-close does not truncate the upstream response.
        if let Err(error) = tokio::io::copy_bidirectional(&mut down, &mut up).await {
            warn!(%error, "upgraded connection copy failed");
        }
    });

    // Relay the 101 handshake response; hyper flushes it and upgrades the
    // downstream connection, which resolves the `downstream` future above.
    Ok(relay_upgrade_response(response))
}

async fn forward(
    client: &ProxyClient,
    route: &Route,
    uri: Uri,
    req: Request,
    client_ip: Option<IpAddr>,
    proto: &'static str,
) -> Result<Response> {
    let (parts, body) = req.into_parts();

    let mut request = Request::new(body);
    copy_upstream_headers(
        request.headers_mut(),
        &parts,
        route,
        client_ip,
        proto,
        false,
    )?;
    *request.method_mut() = parts.method;
    *request.uri_mut() = uri;

    let response = tokio::time::timeout(UPSTREAM_TIMEOUT, client.request(request))
        .await
        .context("upstream timed out")?
        .context("upstream request failed")?;

    Ok(relay_response(response))
}

/// Copies end-to-end request headers to `target` and adds the forwarding
/// headers rpanel sets. When `upgrade` is set, the `Connection`/`Upgrade`
/// handshake headers are kept instead of stripped as hop-by-hop headers.
fn copy_upstream_headers(
    target: &mut HeaderMap,
    parts: &http::request::Parts,
    route: &Route,
    client_ip: Option<IpAddr>,
    proto: &'static str,
    upgrade: bool,
) -> Result<()> {
    let source = &parts.headers;
    if upgrade {
        copy_upgrade_headers(source, target);
    } else {
        copy_headers(source, target);
    }

    target.insert(
        header::HOST,
        HeaderValue::from_str(&route.host).context("invalid upstream host")?,
    );

    // The host the client used, for deciding whether `Origin`/`Referer` are
    // same-origin. HTTP/2 carries it as `:authority` rather than `Host`.
    let public_host = parts
        .uri
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| {
            source
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        });
    rewrite_origin_headers(target, route, public_host);

    if let Some(ip) = client_ip {
        let ip = ip.to_string();
        append_forwarded(target, X_FORWARDED_FOR, &ip);
        target.insert(
            X_REAL_IP,
            HeaderValue::from_str(&ip).context("invalid client IP")?,
        );
    }
    if let Some(host) = source.get(header::HOST) {
        target.insert(X_FORWARDED_HOST, host.clone());
    } else if let Some(authority) = parts.uri.authority()
        && let Ok(value) = HeaderValue::from_str(authority.as_str())
    {
        target.insert(X_FORWARDED_HOST, value);
    }
    target.insert(X_FORWARDED_PROTO, HeaderValue::from_static(proto));
    if let Some(prefix) = route.forwarded_prefix()
        && let Ok(value) = HeaderValue::from_str(prefix)
    {
        target.insert(X_FORWARDED_PREFIX, value);
    }
    Ok(())
}

/// Converts an upstream response into a downstream one, dropping hop-by-hop
/// headers.
fn relay_response(response: hyper::Response<hyper::body::Incoming>) -> Response {
    let (parts, body) = response.into_parts();
    let mut response = Response::new(Body::new(body));
    *response.status_mut() = parts.status;
    *response.version_mut() = parts.version;
    copy_headers(&parts.headers, response.headers_mut());
    response
}

/// Relays a `101 Switching Protocols` response, preserving the `Connection`
/// and `Upgrade` headers needed by the tunnel.
fn relay_upgrade_response(response: hyper::Response<hyper::body::Incoming>) -> Response {
    let (parts, _body) = response.into_parts();
    let mut response = Response::new(Body::empty());
    *response.status_mut() = parts.status;
    *response.version_mut() = parts.version;
    let headers = response.headers_mut();
    copy_headers(&parts.headers, headers);
    for name in [header::CONNECTION, header::UPGRADE] {
        if let Some(value) = parts.headers.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    response
}

/// Rewrites `Origin` and `Referer` to the upstream origin for same-origin
/// requests, so CSRF checks that compare them against the rewritten `Host`
/// header keep working. Cross-origin values are left untouched: rewriting
/// those would let any site bypass the upstream's CSRF protection.
fn rewrite_origin_headers(headers: &mut HeaderMap, route: &Route, public_host: Option<&str>) {
    let Some(public_host) = public_host else {
        return;
    };
    let scheme = if route.https { "https" } else { "http" };
    let origin = format!("{scheme}://{}", route.host);

    if let Some(value) = headers.get(header::ORIGIN)
        && value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<Uri>().ok())
            .is_some_and(|uri| same_authority(&uri, public_host))
    {
        match HeaderValue::from_str(&origin) {
            Ok(value) => {
                headers.insert(header::ORIGIN, value);
            }
            Err(_) => {
                headers.remove(header::ORIGIN);
            }
        }
    }

    if let Some(referer) = headers.get(header::REFERER).cloned()
        && let Some(uri) = referer
            .to_str()
            .ok()
            .and_then(|value| value.parse::<Uri>().ok())
        && same_authority(&uri, public_host)
    {
        let path = uri.path_and_query().map_or("/", |pq| pq.as_str());
        match format!("{origin}{path}").parse::<HeaderValue>() {
            Ok(value) => {
                headers.insert(header::REFERER, value);
            }
            Err(_) => {
                headers.remove(header::REFERER);
            }
        }
    }
}

/// Whether `uri` points at the authority the client used to reach rpanel.
fn same_authority(uri: &Uri, public_host: &str) -> bool {
    uri.authority()
        .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(public_host))
}

/// Redirects a bare prefix to its trailing-slash form when both an exact
/// route and a wildcard route exist for that prefix. This keeps relative
/// asset and API URLs resolving under the prefix.
fn trailing_slash_redirect(routes: &[Route], path: &str, query: Option<&str>) -> Option<String> {
    if path.ends_with('/') {
        return None;
    }
    let exact = routes
        .iter()
        .any(|route| !route.is_wildcard() && route.capture(path).is_some());
    if !exact {
        return None;
    }
    let with_slash = format!("{path}/");
    let wildcard = routes
        .iter()
        .any(|route| route.is_wildcard() && route.prefix() == with_slash);
    if !wildcard {
        return None;
    }
    let mut location = with_slash;
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        location.push('?');
        location.push_str(query);
    }
    Some(location)
}

/// Copies request headers for a protocol upgrade, keeping `Connection` and
/// `Upgrade` (and the `Sec-WebSocket-*` handshake headers) that `copy_headers`
/// would strip as hop-by-hop.
fn copy_upgrade_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for (name, value) in source {
        if name == header::CONNECTION || name == header::UPGRADE {
            target.append(name.clone(), value.clone());
            continue;
        }
        if is_hop_by_hop(name) {
            continue;
        }
        target.append(name.clone(), value.clone());
    }
}

fn copy_headers(source: &HeaderMap, target: &mut HeaderMap) {
    let connection_headers = connection_header_names(source);
    for (name, value) in source {
        if is_hop_by_hop(name) || connection_headers.iter().any(|header| header == name) {
            continue;
        }
        target.append(name, value.clone());
    }
}

fn connection_header_names(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "http2-settings"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn append_forwarded(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    let value = match headers.get(&name).and_then(|value| value.to_str().ok()) {
        Some(existing) if !existing.is_empty() => format!("{existing}, {value}"),
        _ => value.to_owned(),
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_bare_prefix_to_trailing_slash() {
        let group = crate::config::Group {
            host: "nastty.local:8080".into(),
            https: true,
            paths: [
                ("/qbittorrent".to_string(), "/".to_string()),
                ("/qbittorrent/*".to_string(), "/*".to_string()),
            ]
            .into_iter()
            .collect(),
            forwarded_prefix: false,
        };
        let routes =
            crate::route::build_routes(&indexmap::IndexMap::from([("qbit".to_string(), group)]));

        assert_eq!(
            trailing_slash_redirect(&routes, "/qbittorrent", None).as_deref(),
            Some("/qbittorrent/")
        );
        assert_eq!(
            trailing_slash_redirect(&routes, "/qbittorrent", Some("a=1")).as_deref(),
            Some("/qbittorrent/?a=1")
        );
        assert_eq!(
            trailing_slash_redirect(&routes, "/qbittorrent/", None),
            None
        );
        assert_eq!(trailing_slash_redirect(&routes, "/other", None), None);
    }

    #[test]
    fn does_not_redirect_an_exact_route_shadowed_by_a_catch_all() {
        let group = |host: &str, from: &str, to: &str| crate::config::Group {
            host: host.into(),
            https: false,
            paths: [(from.to_string(), to.to_string())].into_iter().collect(),
            forwarded_prefix: false,
        };
        let routes = crate::route::build_routes(&indexmap::IndexMap::from([
            ("root".to_string(), group("root:1", "/*", "/root/*")),
            (
                "health".to_string(),
                group("health:1", "/health", "/health"),
            ),
        ]));

        assert_eq!(trailing_slash_redirect(&routes, "/health", None), None);
    }

    #[test]
    fn rewrites_origin_and_referer_to_upstream() {
        let group = crate::config::Group {
            host: "nastty.local:8080".into(),
            https: true,
            paths: Default::default(),
            forwarded_prefix: false,
        };
        let route = Route::new("qbit", &group, "/qbittorrent/*", "/*");
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://cow"));
        headers.insert(
            header::REFERER,
            HeaderValue::from_static("https://cow/qbittorrent/"),
        );

        rewrite_origin_headers(&mut headers, &route, Some("cow"));

        assert_eq!(headers[header::ORIGIN], "https://nastty.local:8080");
        assert_eq!(
            headers[header::REFERER],
            "https://nastty.local:8080/qbittorrent/"
        );
    }

    #[test]
    fn leaves_cross_origin_headers_untouched() {
        let group = crate::config::Group {
            host: "nastty.local:8080".into(),
            https: true,
            paths: Default::default(),
            forwarded_prefix: false,
        };
        let route = Route::new("qbit", &group, "/qbittorrent/*", "/*");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.test"),
        );
        headers.insert(
            header::REFERER,
            HeaderValue::from_static("https://evil.test/qbittorrent/"),
        );

        rewrite_origin_headers(&mut headers, &route, Some("cow"));

        assert_eq!(headers[header::ORIGIN], "https://evil.test");
        assert_eq!(headers[header::REFERER], "https://evil.test/qbittorrent/");
    }

    #[test]
    fn identifies_hop_by_hop_headers() {
        for name in [
            "connection",
            "http2-settings",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(is_hop_by_hop(&HeaderName::from_static(name)), "{name}");
        }
        assert!(!is_hop_by_hop(&header::CONTENT_TYPE));
    }

    #[test]
    fn copies_headers_except_connection_named_ones() {
        let mut source = HeaderMap::new();
        source.insert(header::CONNECTION, HeaderValue::from_static("x-drop"));
        source.insert("x-drop", HeaderValue::from_static("no"));
        source.insert("x-keep", HeaderValue::from_static("yes"));
        source.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );

        let mut target = HeaderMap::new();
        copy_headers(&source, &mut target);

        assert!(!target.contains_key("x-drop"));
        assert!(!target.contains_key(header::TRANSFER_ENCODING));
        assert_eq!(target["x-keep"], "yes");
    }

    #[test]
    fn appends_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_FOR, HeaderValue::from_static("10.0.0.1"));
        append_forwarded(&mut headers, X_FORWARDED_FOR, "10.0.0.2");
        assert_eq!(headers[X_FORWARDED_FOR], "10.0.0.1, 10.0.0.2");
    }
}
