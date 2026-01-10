use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{broadcast, RwLock};
use std::collections::HashMap;
use crate::AppState;

/// Events sent to web UI clients
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ChangeUpdated {
        change_id: String,
        record_id: String,
    },
    BookmarkMoved {
        name: String,
        target: String,
    },
    PresenceChanged {
        user_id: String,
        change_id: Option<String>,
    },
    Connected {
        repo_id: String,
    },
}

/// Manages event subscriptions for web UI clients
pub struct EventManager {
    channels: RwLock<HashMap<String, broadcast::Sender<Event>>>,
}

impl EventManager {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to events for a repository
    pub async fn subscribe(&self, repo_id: &str) -> broadcast::Receiver<Event> {
        let mut channels = self.channels.write().await;

        if let Some(sender) = channels.get(repo_id) {
            sender.subscribe()
        } else {
            let (tx, rx) = broadcast::channel(100);
            channels.insert(repo_id.to_string(), tx);
            rx
        }
    }

    /// Broadcast an event to all subscribers of a repository
    pub async fn broadcast(&self, repo_id: &str, event: Event) {
        let channels = self.channels.read().await;
        if let Some(sender) = channels.get(repo_id) {
            let _ = sender.send(event);
        }
    }

    /// Emit change updated event
    pub async fn emit_change_updated(&self, repo_id: &str, change_id: &str, record_id: &str) {
        self.broadcast(repo_id, Event::ChangeUpdated {
            change_id: change_id.to_string(),
            record_id: record_id.to_string(),
        }).await;
    }

    /// Emit bookmark moved event
    pub async fn emit_bookmark_moved(&self, repo_id: &str, name: &str, target: &str) {
        self.broadcast(repo_id, Event::BookmarkMoved {
            name: name.to_string(),
            target: target.to_string(),
        }).await;
    }

    /// Emit presence changed event
    pub async fn emit_presence_changed(&self, repo_id: &str, user_id: &str, change_id: Option<&str>) {
        self.broadcast(repo_id, Event::PresenceChanged {
            user_id: user_id.to_string(),
            change_id: change_id.map(|s| s.to_string()),
        }).await;
    }
}

impl Default for EventManager {
    fn default() -> Self {
        Self::new()
    }
}

/// WebSocket handler for web UI events
pub async fn events_handler(
    ws: WebSocketUpgrade,
    Path(repo_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events(socket, repo_id, state))
}

async fn handle_events(socket: WebSocket, repo_id: String, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to events for this repo
    let mut event_rx = state.events.subscribe(&repo_id).await;

    tracing::info!("Web UI client connected to events for repo {}", repo_id);

    // Send connected event
    let connected = Event::Connected { repo_id: repo_id.clone() };
    if let Ok(json) = serde_json::to_string(&connected) {
        let _ = sender.send(Message::Text(json)).await;
    }

    loop {
        tokio::select! {
            // Forward events to client
            Ok(event) = event_rx.recv() => {
                if let Ok(json) = serde_json::to_string(&event) {
                    if let Err(e) = sender.send(Message::Text(json)).await {
                        tracing::error!("Failed to send event: {}", e);
                        break;
                    }
                }
            }
            // Handle incoming messages (ping/pong, close)
            Some(msg) = receiver.next() => {
                match msg {
                    Ok(Message::Close(_)) => {
                        tracing::info!("Web UI client disconnected from repo {}", repo_id);
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
        }
    }
}
