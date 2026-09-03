//! Tandem workspace creation, attachment, observation, and publication.

mod daemon;
mod watch;
mod workspace_init;

pub use daemon::{
    is_running, read_status, resolve_debounce, run_daemon, Daemon, DaemonOptions, DaemonStatus,
    SnapshotOutcome,
};
pub use watch::run_watch;

pub use workspace_init::{
    clone_tandem_workspace, init_tandem_workspace, user_settings_from_environment, WorkspaceOrigin,
};
