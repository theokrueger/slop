use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use tokio::time::MissedTickBehavior;

use super::{AppState, AuthUser};
use crate::session::PaneId;
use crate::session::pane::{Pane, Snapshot};

/// Client -> server frame tags.
const CLIENT_RESIZE: u8 = 0;
const CLIENT_INPUT: u8 = 1;

/// Server -> client frame tags.
const SERVER_FULL: u8 = 0;
const SERVER_DIFF: u8 = 1;
const SERVER_CLIPBOARD: u8 = 2;

const TICK: Duration = Duration::from_millis(33);

pub async fn pane_socket(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    AuthUser(_user): AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(pane) = state.sessions.pane(PaneId::new(id)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let client_id = state.next_client_id();
    ws.max_message_size(1024 * 1024)
        .on_upgrade(move |socket| run(socket, pane, client_id))
}

async fn run(socket: WebSocket, pane: std::sync::Arc<Pane>, client_id: u64) {
    let (mut sink, mut stream) = socket.split();
    let mut clipboard = pane.subscribe_clipboard();
    let snapshot = pane.snapshot();
    if send_full(&mut sink, &snapshot).await.is_err() {
        return;
    }
    let mut previous = snapshot.screen;
    let _ = pane.attach_client(client_id, snapshot.cols, snapshot.rows);
    tracing::debug!(
        pane = pane.id().get(),
        client = client_id,
        "terminal client attached"
    );

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if pane.is_closed() {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                let (rows, cols) = pane.size();
                let (prev_rows, prev_cols) = {
                    let (rows, cols) = previous.size();
                    (rows.get(), cols.get())
                };
                if (rows, cols) != (prev_rows, prev_cols) {
                    let snapshot = pane.snapshot();
                    if send_full(&mut sink, &snapshot).await.is_err() {
                        break;
                    }
                    previous = snapshot.screen;
                } else if let Some(update) = pane.update(&previous) {
                    let mut payload = Vec::with_capacity(1 + update.data.len());
                    payload.push(SERVER_DIFF);
                    payload.extend_from_slice(update.data.as_bytes());
                    if sink.send(Message::Binary(Bytes::from(payload))).await.is_err() {
                        break;
                    }
                    previous = update.screen;
                }
            }
            clip = clipboard.recv() => {
                match clip {
                    Ok(text) => {
                        if send_clipboard(&mut sink, &text).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(pane = pane.id().get(), skipped, "clipboard events lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            message = stream.next() => {
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        handle_client_frame(&pane, client_id, data);
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(err)) => {
                        tracing::debug!(pane = pane.id().get(), error = %err, "terminal socket error");
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    pane.detach_client(client_id);
    tracing::debug!(
        pane = pane.id().get(),
        client = client_id,
        "terminal client detached"
    );
}

fn handle_client_frame(pane: &Pane, client_id: u64, data: Bytes) {
    let Some((&opcode, rest)) = data.split_first() else {
        return;
    };
    match opcode {
        CLIENT_RESIZE if rest.len() >= 4 => {
            let cols = u16::from_be_bytes([rest[0], rest[1]]);
            let rows = u16::from_be_bytes([rest[2], rest[3]]);
            pane.update_client(client_id, cols, rows);
        }
        CLIENT_INPUT => {
            if let Err(err) = pane.write_input(rest) {
                tracing::debug!(pane = pane.id().get(), error = %err, "could not write terminal input");
            }
        }
        _ => {}
    }
}

async fn send_clipboard(
    sink: &mut SplitSink<WebSocket, Message>,
    text: &str,
) -> Result<(), axum::Error> {
    let mut payload = Vec::with_capacity(1 + text.len());
    payload.push(SERVER_CLIPBOARD);
    payload.extend_from_slice(text.as_bytes());
    sink.send(Message::Binary(Bytes::from(payload))).await
}

async fn send_full(
    sink: &mut SplitSink<WebSocket, Message>,
    snapshot: &Snapshot,
) -> Result<(), axum::Error> {
    let mut payload = Vec::with_capacity(5 + snapshot.data.len());
    payload.push(SERVER_FULL);
    payload.extend_from_slice(&snapshot.rows.to_be_bytes());
    payload.extend_from_slice(&snapshot.cols.to_be_bytes());
    payload.extend_from_slice(snapshot.data.as_bytes());
    sink.send(Message::Binary(Bytes::from(payload))).await
}
