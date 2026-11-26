//! Asks the user about an SSH host key seen for the first time, through the window.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;
use treasury_ipc::HostKeyQuestion;
use treasury_tunnel::HostKeyPrompt;

/// How long the question waits for an answer before the key is refused.
const ANSWER_WITHIN: Duration = Duration::from_secs(300);

pub struct GuiPrompt {
    app: AppHandle,
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
}

impl GuiPrompt {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            next: AtomicU64::new(1),
            waiting: Mutex::new(HashMap::new()),
        }
    }

    /// Delivers the user's answer to question `id`.
    pub fn answer(&self, id: u64, accept: bool) {
        if let Some(tx) = self
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
        {
            let _ = tx.send(accept);
        }
    }
}

#[async_trait]
impl HostKeyPrompt for GuiPrompt {
    async fn confirm_new(&self, host: &str, port: u16, algorithm: &str, fingerprint: &str) -> bool {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        let question = HostKeyQuestion {
            id,
            host: host.to_owned(),
            port,
            algorithm: algorithm.to_owned(),
            fingerprint: fingerprint.to_owned(),
        };
        if self.app.emit("host-key", question).is_err() {
            return false;
        }
        let accepted = matches!(tokio::time::timeout(ANSWER_WITHIN, rx).await, Ok(Ok(true)));
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        accepted
    }
}
