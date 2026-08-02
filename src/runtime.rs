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

    /// Atomically reserves a runtime slot for an agent. Agent task ids use
    /// the `a`/`r` prefixes, so shell jobs do not consume delegation slots.
    pub async fn register_agent(
        &self,
        task_id: String,
        cancel: oneshot::Sender<()>,
        max_concurrent: usize,
    ) -> bool {
        let mut handles = self.inner.lock().await;
        let running_agents = handles
            .keys()
            .filter(|id| id.starts_with('a') || id.starts_with('r'))
            .count();
        if running_agents >= max_concurrent.max(1) {
            return false;
        }
        handles.insert(task_id, cancel);
        true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_registration_enforces_the_parallel_limit_atomically() {
        let runtime = RuntimeHandles::default();
        let (first, _first_rx) = oneshot::channel();
        let (second, _second_rx) = oneshot::channel();
        let (third, _third_rx) = oneshot::channel();
        assert!(runtime.register_agent("a1".into(), first, 2).await);
        assert!(runtime.register_agent("r2".into(), second, 2).await);
        assert!(!runtime.register_agent("a3".into(), third, 2).await);
        assert!(runtime.stop("a1").await);
    }
}
