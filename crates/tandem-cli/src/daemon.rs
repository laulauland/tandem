use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tandem_core::sync::ForgeDoc;
use crate::presence::PresenceManager;
use crate::content::ContentManager;
use crate::offline::{self, OperationQueue, QueuedOperation};
use crate::repo::JjRepo;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("Not connected to forge")]
    NotConnected,
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Repo error: {0}")]
    Repo(#[from] crate::repo::RepoError),
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("Sync error: {0}")]
    Sync(String),
    #[error("Offline error: {0}")]
    Offline(#[from] crate::offline::OfflineError),
}

#[derive(Debug)]
pub enum DaemonCommand {
    SyncNow,
    UpdatePresence { change_id: String },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Connected,
    Disconnected,
    SyncCompleted,
    PresenceWarning { change_id: String, user: String },
    Error(String),
}

pub struct Daemon {
    repo_path: PathBuf,
    _forge_url: String,
    doc: Arc<RwLock<ForgeDoc>>,
    presence_manager: PresenceManager,
    content_manager: Option<ContentManager>,
    command_rx: mpsc::Receiver<DaemonCommand>,
    event_tx: mpsc::Sender<DaemonEvent>,
    is_connected: bool,
    operation_queue: OperationQueue,
}

impl Daemon {
    pub fn new(
        repo_path: PathBuf,
        forge_url: String,
        command_rx: mpsc::Receiver<DaemonCommand>,
        event_tx: mpsc::Sender<DaemonEvent>,
    ) -> Self {
        let doc = Arc::new(RwLock::new(ForgeDoc::new()));
        let user_id = whoami::username().unwrap_or_else(|_| "unknown".to_string());
        let device = whoami::devicename().unwrap_or_else(|_| "unknown".to_string());
        let presence_manager = PresenceManager::new(doc.clone(), user_id, device);
        let operation_queue = OperationQueue::load(&repo_path).unwrap_or_default();

        Self {
            repo_path,
            _forge_url: forge_url,
            doc,
            presence_manager,
            content_manager: None,
            command_rx,
            event_tx,
            is_connected: false,
            operation_queue,
        }
    }

    pub async fn run(&mut self) -> Result<(), DaemonError> {
        // Get repo ID from config
        let repo = JjRepo::open(&self.repo_path)?;
        let config = repo.forge_config()?
            .ok_or_else(|| DaemonError::ConnectionFailed("No forge config".to_string()))?;

        // Extract repo ID from URL (last path segment)
        let repo_id = config.forge.url
            .rsplit('/')
            .next()
            .unwrap_or("unknown")
            .to_string();

        // Initialize content manager with repo_id
        self.content_manager = Some(ContentManager::new(
            self.doc.clone(),
            config.forge.url.clone(),
            repo_id.clone()
        ));

        // Build WebSocket URL
        let ws_url = format!("{}/sync/{}",
            config.forge.url.replace("https://", "wss://").replace("http://", "ws://"),
            repo_id
        );

        tracing::info!("Connecting to forge: {}", ws_url);

        self.preload_content_on_startup().await;

        // Connect with retry loop
        loop {
            match self.connect_and_sync(&ws_url).await {
                Ok(()) => {
                    tracing::info!("Disconnected from forge, reconnecting...");
                }
                Err(e) => {
                    tracing::error!("Connection error: {}, retrying in 5s...", e);
                    let _ = self.on_disconnected().await;
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }

            // Check for shutdown
            if let Ok(cmd) = self.command_rx.try_recv() {
                if matches!(cmd, DaemonCommand::Shutdown) {
                    tracing::info!("Daemon shutting down");
                    self.presence_manager.clear_presence().await;
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected
    }

    async fn connect_and_sync(&mut self, ws_url: &str) -> Result<(), DaemonError> {
        let (ws_stream, _) = connect_async(ws_url).await
            .map_err(|e| DaemonError::ConnectionFailed(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // Mark as connected
        self.on_connected().await?;

        // Send our state vector to request initial sync
        {
            let doc = self.doc.read().await;
            let sv = doc.encode_state_vector();
            write.send(Message::Binary(sv.into())).await
                .map_err(|e| DaemonError::WebSocket(e.to_string()))?;
        }

        loop {
            tokio::select! {
                // Handle commands from the application
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        DaemonCommand::Shutdown => {
                            tracing::info!("Daemon shutting down");
                            return Ok(());
                        }
                        DaemonCommand::SyncNow => {
                            // Send current state
                            let doc = self.doc.read().await;
                            let sv = doc.encode_state_vector();
                            drop(doc);
                            write.send(Message::Binary(sv.into())).await
                                .map_err(|e| DaemonError::WebSocket(e.to_string()))?;
                        }
                        DaemonCommand::UpdatePresence { change_id } => {
                            // Update presence in local doc
                            if let Ok(cid) = change_id.parse() {
                                self.presence_manager.update_presence(&cid).await;

                                let conflicts = self.presence_manager.check_conflict(&cid).await;
                                if !conflicts.is_empty() {
                                    for conflict in conflicts {
                                        let _ = self.event_tx.send(DaemonEvent::PresenceWarning {
                                            change_id: cid.to_string(),
                                            user: format!("{}@{}", conflict.user_id, conflict.device),
                                        }).await;
                                    }
                                }
                            }
                        }
                    }
                }

                // Handle messages from the server
                Some(msg) = read.next() => {
                    match msg {
                        Ok(Message::Binary(data)) => {
                            // Apply update from server
                            let doc = self.doc.read().await;
                            if let Err(e) = doc.apply_update(&data) {
                                tracing::warn!("Failed to apply update: {:?}", e);
                            }
                        }
                        Ok(Message::Close(_)) => {
                            tracing::info!("Server closed connection");
                            return Ok(());
                        }
                        Ok(Message::Ping(data)) => {
                            write.send(Message::Pong(data)).await
                                .map_err(|e| DaemonError::WebSocket(e.to_string()))?;
                        }
                        Err(e) => {
                            tracing::error!("WebSocket error: {}", e);
                            return Err(DaemonError::WebSocket(e.to_string()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Handle connection established
    async fn on_connected(&mut self) -> Result<(), DaemonError> {
        tracing::info!("Connected to forge");
        self.is_connected = true;
        offline::set_offline(&self.repo_path, false)?;

        let _ = self.event_tx.send(DaemonEvent::Connected).await;

        // Replay any queued operations
        self.replay_queued_operations().await?;

        Ok(())
    }

    /// Handle connection lost
    async fn on_disconnected(&mut self) -> Result<(), DaemonError> {
        tracing::warn!("Disconnected from forge");
        self.is_connected = false;
        offline::set_offline(&self.repo_path, true)?;

        let _ = self.event_tx.send(DaemonEvent::Disconnected).await;

        Ok(())
    }

    /// Queue an operation for offline replay
    fn queue_operation(&mut self, op: QueuedOperation) -> Result<(), DaemonError> {
        self.operation_queue.enqueue(op);
        self.operation_queue.save(&self.repo_path)?;
        Ok(())
    }

    /// Replay all queued operations to the forge
    async fn replay_queued_operations(&mut self) -> Result<(), DaemonError> {
        if self.operation_queue.is_empty() {
            return Ok(());
        }

        tracing::info!("Replaying {} queued operations", self.operation_queue.len());

        let doc = self.doc.read().await;
        let count = offline::replay_queue(&self.repo_path, &doc).await?;
        drop(doc);

        // Reload queue (should be empty now)
        self.operation_queue = OperationQueue::load(&self.repo_path)?;

        tracing::info!("Replayed {} operations", count);
        Ok(())
    }

    async fn preload_content_on_startup(&self) {
        if let Some(content_manager) = &self.content_manager {
            if let Ok(count_loaded) = content_manager.preload_bookmarks().await {
                tracing::info!("Preloaded {} bookmarked changes", count_loaded);
            }

            if let Ok(count_loaded) = content_manager.preload_recent(10).await {
                tracing::info!("Preloaded {} recent changes", count_loaded);
            }
        }
    }
}

#[derive(Clone)]
pub struct DaemonHandle {
    command_tx: mpsc::Sender<DaemonCommand>,
    _event_rx: Arc<RwLock<mpsc::Receiver<DaemonEvent>>>,
}

impl DaemonHandle {
    pub async fn sync_now(&self) -> Result<(), DaemonError> {
        self.command_tx.send(DaemonCommand::SyncNow).await
            .map_err(|_| DaemonError::NotConnected)
    }

    pub async fn update_presence(&self, change_id: String) -> Result<(), DaemonError> {
        self.command_tx.send(DaemonCommand::UpdatePresence { change_id }).await
            .map_err(|_| DaemonError::NotConnected)
    }

    pub async fn shutdown(&self) -> Result<(), DaemonError> {
        self.command_tx.send(DaemonCommand::Shutdown).await
            .map_err(|_| DaemonError::NotConnected)
    }
}

pub fn spawn_daemon(repo_path: PathBuf, forge_url: String) -> DaemonHandle {
    let (command_tx, command_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::channel(32);

    let mut daemon = Daemon::new(repo_path, forge_url, command_rx, event_tx);

    tokio::spawn(async move {
        if let Err(e) = daemon.run().await {
            tracing::error!("Daemon error: {}", e);
        }
    });

    DaemonHandle {
        command_tx,
        _event_rx: Arc::new(RwLock::new(event_rx)),
    }
}
