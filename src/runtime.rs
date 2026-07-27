use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Mutex, oneshot};

#[derive(Clone, Default)]
pub struct RuntimeHandles {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
}

impl RuntimeHandles {
    pub async fn register(&self, task_id: String, cancel: oneshot::Sender<()>) {
        self.inner.lock().await.insert(task_id, cancel);
    }

    pub async fn stop(&self, task_id: &str) -> bool {
        self.inner
            .lock()
            .await
            .remove(task_id)
            .is_some_and(|cancel| cancel.send(()).is_ok())
    }

    pub async fn remove(&self, task_id: &str) {
        self.inner.lock().await.remove(task_id);
    }
}
