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

pub async fn handler(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let path = req.uri().path();
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

    match forward(&state.client, route, uri, req, client_ip, proto).await {
        Ok(response) => response,
        Err(error) => {
            warn!(group = %route.group, %error, "upstream request failed");
            (StatusCode::BAD_GATEWAY, "bad gateway\n").into_response()
        }
    }
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

    let mut builder = Request::builder().method(parts.method).uri(uri);
    let headers = builder
        .headers_mut()
        .context("request builder has no headers")?;
    copy_headers(&parts.headers, headers);
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&route.host).context("invalid upstream host")?,
    );
    if let Some(ip) = client_ip {
        append_forwarded(headers, X_FORWARDED_FOR, &ip.to_string());
        headers.insert(
            X_REAL_IP,
            HeaderValue::from_str(&ip.to_string()).context("invalid client IP")?,
        );
    }
    if let Some(host) = parts.headers.get(header::HOST) {
        headers.insert(X_FORWARDED_HOST, host.clone());
    }
    headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static(proto));

    let request = builder.body(body).context("building upstream request")?;
    let response = tokio::time::timeout(UPSTREAM_TIMEOUT, client.request(request))
        .await
        .context("upstream timed out")?
        .context("upstream request failed")?;

    let (parts, body) = response.into_parts();
    let mut builder = Response::builder()
        .status(parts.status)
        .version(parts.version);
    let headers = builder
        .headers_mut()
        .context("response builder has no headers")?;
    copy_headers(&parts.headers, headers);
    builder
        .body(Body::new(body))
        .context("building downstream response")
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
