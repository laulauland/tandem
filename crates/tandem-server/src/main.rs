//! Tandem Server
//!
//! Forge server built with Axum, Yrs, and SQLite

mod auth;
mod authz;
mod db;
mod docs;
mod events;
mod handlers;
mod sync;

use axum::{
    Json, Router,
    middleware,
    routing::{get, post},
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use auth::{auth_middleware, get_me, login};
use db::Database;
use docs::DocManager;
use events::EventManager;
use sync::SyncManager;

#[derive(Clone)]
pub struct AppState {
    db: Database,
    docs: Arc<DocManager>,
    events: Arc<EventManager>,
    sync: Arc<SyncManager>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tandem_server=debug,tower_http=debug".into()),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:tandem.db".to_string());

    let db = Database::new(&database_url)
        .await
        .expect("Failed to connect to database");

    db.run_migrations().await.expect("Failed to run migrations");

    tracing::info!("Database initialized and migrations applied");

    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string());
    let docs = Arc::new(DocManager::new(&data_dir));
    tracing::info!("DocManager initialized with data directory: {}", data_dir);

    let events = Arc::new(EventManager::new());
    tracing::info!("EventManager initialized");

    let sync = Arc::new(SyncManager::new());
    tracing::info!("SyncManager initialized");

    let state = AppState {
        db,
        docs: Arc::clone(&docs),
        events,
        sync,
    };

    let app = Router::new()
        .nest("/api", api_routes(state.clone()))
        .nest("/sync", sync_routes())
        .nest("/events", event_routes())
        .route("/health", get(health_check))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Server running on http://localhost:3000");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(docs))
        .await
        .unwrap();
}

async fn shutdown_signal(docs: Arc<DocManager>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, shutting down gracefully");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM, shutting down gracefully");
        },
    }

    tracing::info!("Saving all Y.Doc states to disk");
    if let Err(e) = docs.save_all().await {
        tracing::error!("Failed to save docs: {}", e);
    } else {
        tracing::info!("All Y.Doc states saved successfully");
    }
}

fn api_routes(state: AppState) -> Router<AppState> {
    // Public routes (no authentication required)
    let public_routes = Router::new().route("/auth/login", post(login));

    // Protected routes (authentication required)
    let protected_routes = Router::new()
        .route("/auth/me", get(get_me))
        .route("/repos", get(handlers::repos::list_repos).post(handlers::repos::create_repo))
        .route("/repos/:id", get(handlers::repos::get_repo))
        .route("/repos/:id/changes", get(handlers::changes::list_changes))
        .route("/repos/:id/changes/:cid", get(handlers::changes::get_change))
        .route(
            "/repos/:id/bookmarks",
            get(handlers::bookmarks::list_bookmarks).post(handlers::bookmarks::move_bookmark),
        )
        .route("/repos/:id/presence", get(handlers::presence::get_presence))
        .route("/repos/:id/content/:hash", get(handlers::content::get_content))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    public_routes.merge(protected_routes)
}

fn sync_routes() -> Router<AppState> {
    Router::new().route("/:repo_id", get(sync::sync_handler))
}

fn event_routes() -> Router<AppState> {
    Router::new().route("/:repo_id", get(events::events_handler))
}

async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}
