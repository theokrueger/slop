use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};

use crate::{
    config::{Config, PanelConfig},
    net::NetworkHistory,
    route::Route,
};

pub type ProxyClient = Client<HttpsConnector<HttpConnector>, Body>;

pub struct AppState {
    pub config: Arc<Config>,
    pub routes: Arc<Vec<Route>>,
    pub client: ProxyClient,
    pub panel: Option<Arc<PanelState>>,
}

pub struct PanelState {
    pub config: PanelConfig,
    pub network: Arc<NetworkHistory>,
}

pub fn build_client() -> Result<ProxyClient> {
    let connector = HttpsConnectorBuilder::new()
        .with_native_roots()
        .context("loading native TLS roots")?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}
