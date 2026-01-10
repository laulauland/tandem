use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::sync::RwLock;
use uuid::Uuid;
use crate::AppState;

/// WebSocket handler for yrs sync
pub async fn sync_handler(
    ws: WebSocketUpgrade,
    Path(repo_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_sync(socket, repo_id, state))
}

async fn handle_sync(socket: WebSocket, repo_id: String, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    let doc = match state.docs.get_or_load(&repo_id).await {
        Ok(doc) => doc,
        Err(e) => {
            tracing::error!("Failed to load doc for {}: {}", repo_id, e);
            return;
        }
    };

    let client_id = Uuid::new_v4();
    let mut broadcast_rx = state.sync.subscribe(&repo_id).await;

    tracing::info!("Client {} connected to sync for repo {}", client_id, repo_id);

    loop {
        tokio::select! {
            Some(msg) = receiver.next() => {
                match msg {
                    Ok(Message::Binary(data)) => {
                        let doc = doc.read().await;

                        if let Err(_e) = doc.apply_update(&data) {
                            // Might be a state vector - compute diff and send
                            let update = doc.encode_update_from(&data);
                            drop(doc);
                            if let Err(e) = sender.send(Message::Binary(update)).await {
                                tracing::error!("Failed to send update: {}", e);
                                break;
                            }
                        } else {
                            // Successfully applied update
                            drop(doc);

                            // Save to disk
                            if let Err(e) = state.docs.save(&repo_id).await {
                                tracing::warn!("Failed to save doc: {}", e);
                            }

                            // Broadcast to other clients
                            state.sync.broadcast(&repo_id, client_id, data).await;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        tracing::info!("Client {} disconnected from repo {}", client_id, repo_id);
                        break;
                    }
                    Ok(Message::Ping(data)) => {
                        if let Err(e) = sender.send(Message::Pong(data)).await {
                            tracing::error!("Failed to send pong: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            Ok(msg) = broadcast_rx.recv() => {
                // Don't echo back to sender
                if msg.sender_id != client_id {
                    if let Err(e) = sender.send(Message::Binary(msg.data)).await {
                        tracing::error!("Failed to forward broadcast: {}", e);
                        break;
                    }
                }
            }
        }
    }
}

/// Message wrapper that includes sender ID to prevent echo
#[derive(Clone, Debug)]
pub(crate) struct BroadcastMessage {
    sender_id: Uuid,
    data: Vec<u8>,
}

/// Track connected clients for broadcasting
pub struct SyncManager {
    channels: RwLock<HashMap<String, tokio::sync::broadcast::Sender<BroadcastMessage>>>,
}

impl SyncManager {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
        }
    }

    pub async fn subscribe(&self, repo_id: &str) -> tokio::sync::broadcast::Receiver<BroadcastMessage> {
        let mut channels = self.channels.write().await;
        let tx = channels.entry(repo_id.to_string()).or_insert_with(|| {
            let (tx, _rx) = tokio::sync::broadcast::channel(100);
            tx
        });
        tx.subscribe()
    }

    pub async fn broadcast(&self, repo_id: &str, sender_id: Uuid, data: Vec<u8>) {
        let channels = self.channels.read().await;
        if let Some(tx) = channels.get(repo_id) {
            let msg = BroadcastMessage { sender_id, data };
            let _ = tx.send(msg);
        }
    }
}
