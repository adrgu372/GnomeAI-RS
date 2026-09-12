//! Blind, bounded two-peer relay. Deploy behind TLS; no chat persistence.
use axum::{
    Router,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc};

const MAX_FRAME: usize = 24 * 1024 * 1024;
struct Packet {
    message: Message,
    _budget: OwnedSemaphorePermit,
}
type Rooms = HashMap<String, HashMap<u64, mpsc::Sender<Packet>>>;
#[derive(Clone)]
struct Relay {
    rooms: Arc<Mutex<Rooms>>,
    connections: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    next: Arc<AtomicU64>,
}

async fn upgrade(
    State(relay): State<Relay>,
    Path(channel): Path<String>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if channel.len() != 43
        || !channel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(permit) = relay.connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_frame_size(MAX_FRAME)
        .max_message_size(MAX_FRAME)
        .on_upgrade(move |socket| serve(relay, channel, socket, permit))
        .into_response()
}
async fn serve(relay: Relay, channel: String, socket: WebSocket, _permit: OwnedSemaphorePermit) {
    let id = relay.next.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel::<Packet>(4);
    {
        let mut rooms = relay.rooms.lock().await;
        let room = rooms.entry(channel.clone()).or_default();
        if room.len() >= 2 {
            return;
        }
        room.insert(id, tx);
    }
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            packet=rx.recv() => {
                let Some(packet)=packet else {break;};
                if !matches!(tokio::time::timeout(Duration::from_secs(15),sink.send(packet.message)).await,Ok(Ok(()))) {break;}
            }
            incoming=tokio::time::timeout(Duration::from_secs(30),stream.next()) => {
                let Ok(Some(Ok(message)))=incoming else {break;};
                match message {
                    Message::Text(text) => {
                        let receiver=relay.rooms.lock().await.get(&channel)
                            .and_then(|room|room.iter().find(|(other,_)|**other!=id).map(|(_,tx)|tx.clone()));
                        if let Some(receiver)=receiver {
                            let Ok(budget)=relay.bytes.clone().try_acquire_many_owned(text.len().max(1) as u32) else {break;};
                            if receiver.try_send(Packet { message:Message::Text(text), _budget:budget }).is_err() {break;}
                        }
                    }
                    Message::Ping(data) => {if sink.send(Message::Pong(data)).await.is_err(){break;}},
                    Message::Pong(_) => {},
                    _ => break,
                }
            }
        }
    }
    let mut rooms = relay.rooms.lock().await;
    if let Some(room) = rooms.get_mut(&channel) {
        room.remove(&id);
        if room.is_empty() {
            rooms.remove(&channel);
        }
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let relay = Relay {
        rooms: Default::default(),
        connections: Arc::new(Semaphore::new(64)),
        bytes: Arc::new(Semaphore::new(64 * 1024 * 1024)),
        next: Arc::new(AtomicU64::new(1)),
    };
    let address = std::env::var("GNOMEAI_RELAY_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let listener = tokio::net::TcpListener::bind(address).await?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws/{channel}", get(upgrade))
        .with_state(relay);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
