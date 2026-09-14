//! Connections shared by the three stores of one loaded repository.
//!
//! Factories own this registry; live stores own the connections. Mutable
//! workspace identity and head versions remain in each op-heads adapter.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};

use blake2::{Blake2b512, Digest as _};
use jj_lib::backend::BackendLoadError;

use crate::{repo_link, TandemClient};

#[derive(Clone, Hash, PartialEq, Eq)]
struct SessionKey {
    repository: PathBuf,
    address: String,
    bearer_fingerprint: [u8; 64],
}

struct SessionSlot {
    state: Mutex<SessionState>,
    changed: Condvar,
}

enum SessionState {
    Loading,
    Ready(Weak<TandemClient>),
    Failed(Arc<str>),
}

#[derive(Default)]
pub(crate) struct RepositorySessions {
    sessions: Mutex<HashMap<SessionKey, Arc<SessionSlot>>>,
}

impl RepositorySessions {
    pub(crate) fn load(&self, store_path: &Path) -> Result<Arc<TandemClient>, BackendLoadError> {
        self.load_with(
            store_path,
            |address, token| {
                TandemClient::connect(address, token)
                    .map_err(|error| BackendLoadError(error.into()))
            },
            || {},
        )
    }

    fn load_with<C, W>(
        &self,
        store_path: &Path,
        connect: C,
        waiting: W,
    ) -> Result<Arc<TandemClient>, BackendLoadError>
    where
        C: Fn(&str, &str) -> Result<Arc<TandemClient>, BackendLoadError>,
        W: Fn(),
    {
        let repository = store_path
            .parent()
            .ok_or_else(|| backend_error("store has no repository directory"))?
            .canonicalize()
            .map_err(|error| BackendLoadError(error.into()))?;
        let address = repo_link::read_server_address(store_path)?;
        let token = repo_link::read_token(store_path)?;
        let key = SessionKey {
            repository,
            address: address.clone(),
            bearer_fingerprint: Blake2b512::digest(token.as_bytes()).into(),
        };
        loop {
            let (slot, leader) = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| backend_error("repository sessions lock poisoned"))?;
                match sessions.get(&key) {
                    Some(slot) => (slot.clone(), false),
                    None => {
                        let slot = Arc::new(SessionSlot {
                            state: Mutex::new(SessionState::Loading),
                            changed: Condvar::new(),
                        });
                        sessions.insert(key.clone(), slot.clone());
                        (slot, true)
                    }
                }
            };
            if leader {
                match connect(&address, &token) {
                    Ok(client) => {
                        *slot.state.lock().map_err(|_| {
                            backend_error("repository session slot lock poisoned")
                        })? = SessionState::Ready(Arc::downgrade(&client));
                        slot.changed.notify_all();
                        return Ok(client);
                    }
                    Err(error) => {
                        let message: Arc<str> = error.to_string().into();
                        *slot.state.lock().map_err(|_| {
                            backend_error("repository session slot lock poisoned")
                        })? = SessionState::Failed(message.clone());
                        slot.changed.notify_all();
                        if let Ok(mut sessions) = self.sessions.lock() {
                            if sessions
                                .get(&key)
                                .is_some_and(|current| Arc::ptr_eq(current, &slot))
                            {
                                sessions.remove(&key);
                            }
                        }
                        return Err(backend_error(&message));
                    }
                }
            }
            let mut state = slot
                .state
                .lock()
                .map_err(|_| backend_error("repository session slot lock poisoned"))?;
            loop {
                match &*state {
                    SessionState::Loading => {
                        waiting();
                        state = slot
                            .changed
                            .wait(state)
                            .map_err(|_| backend_error("repository session slot wait poisoned"))?;
                    }
                    SessionState::Ready(client) => {
                        if let Some(client) = client.upgrade() {
                            return Ok(client);
                        }
                        drop(state);
                        let mut sessions = self
                            .sessions
                            .lock()
                            .map_err(|_| backend_error("repository sessions lock poisoned"))?;
                        if sessions
                            .get(&key)
                            .is_some_and(|current| Arc::ptr_eq(current, &slot))
                        {
                            sessions.remove(&key);
                        }
                        break;
                    }
                    SessionState::Failed(message) => return Err(backend_error(message)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    fn linked_store() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join(".jj/repo/store");
        std::fs::create_dir_all(&store).unwrap();
        repo_link::write_link(&store, "example.test", "test-token").unwrap();
        (temp, store)
    }

    #[test]
    fn concurrent_same_key_loads_share_one_connection_and_wake_waiters() {
        let (_temp, store) = linked_store();
        let sessions = Arc::new(RepositorySessions::default());
        let client = TandemClient::test_instance();
        let calls = Arc::new(AtomicU64::new(0));
        let (leader_entered_tx, leader_entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (waiter_tx, waiter_rx) = mpsc::channel();

        let leader = {
            let sessions = sessions.clone();
            let store = store.clone();
            let client = client.clone();
            let calls = calls.clone();
            let release_rx = release_rx.clone();
            std::thread::spawn(move || {
                sessions.load_with(
                    &store,
                    move |_, _| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        leader_entered_tx.send(()).unwrap();
                        release_rx.lock().unwrap().recv().unwrap();
                        Ok(client.clone())
                    },
                    || {},
                )
            })
        };
        leader_entered_rx.recv().unwrap();
        let waiter = {
            let sessions = sessions.clone();
            let store = store.clone();
            let client = client.clone();
            let calls = calls.clone();
            std::thread::spawn(move || {
                sessions.load_with(
                    &store,
                    move |_, _| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(client.clone())
                    },
                    || waiter_tx.send(()).unwrap(),
                )
            })
        };
        waiter_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        let first = leader.join().unwrap().unwrap();
        let second = waiter.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let third = sessions
            .load_with(&store, |_, _| unreachable!(), || {})
            .unwrap();
        assert!(Arc::ptr_eq(&first, &third));
    }

    #[test]
    fn failed_leader_wakes_waiter_and_a_retry_can_connect() {
        let (_temp, store) = linked_store();
        let sessions = Arc::new(RepositorySessions::default());
        let (leader_entered_tx, leader_entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (waiter_tx, waiter_rx) = mpsc::channel();
        let leader = {
            let sessions = sessions.clone();
            let store = store.clone();
            std::thread::spawn(move || {
                sessions.load_with(
                    &store,
                    move |_, _| {
                        leader_entered_tx.send(()).unwrap();
                        release_rx.lock().unwrap().recv().unwrap();
                        Err(backend_error("controlled connection failure"))
                    },
                    || {},
                )
            })
        };
        leader_entered_rx.recv().unwrap();
        let waiter = {
            let sessions = sessions.clone();
            let store = store.clone();
            std::thread::spawn(move || {
                sessions.load_with(
                    &store,
                    |_, _| unreachable!("waiter must not connect"),
                    || waiter_tx.send(()).unwrap(),
                )
            })
        };
        waiter_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        assert!(leader.join().unwrap().is_err());
        assert!(waiter.join().unwrap().is_err());

        let client = TandemClient::test_instance();
        let retried = sessions
            .load_with(&store, |_, _| Ok(client.clone()), || {})
            .unwrap();
        assert!(Arc::ptr_eq(&client, &retried));
    }
}

fn backend_error(message: &str) -> BackendLoadError {
    BackendLoadError(anyhow::anyhow!(message.to_string()).into())
}
