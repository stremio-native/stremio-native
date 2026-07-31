use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::profiles::ProfileId;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeHostError {
    #[error("target profile preflight failed: {0}")]
    Preflight(String),
    #[error("current profile storage flush failed: {0}")]
    Flush(String),
    #[error("target profile runtime construction failed: {target}; rollback: {rollback}")]
    Construction { target: String, rollback: String },
}

#[async_trait]
pub trait RuntimeFactory<R>: Send + Sync {
    async fn preflight(&self, profile_id: &ProfileId) -> Result<(), RuntimeHostError>;
    async fn flush(&self, runtime: &Arc<R>) -> Result<(), RuntimeHostError>;
    async fn stop_playback(&self, runtime: &Arc<R>);
    async fn build(
        &self,
        profile_id: &ProfileId,
        generation: u64,
        cancellation: CancellationToken,
    ) -> Result<Arc<R>, RuntimeHostError>;
}

pub struct RuntimeHost<R> {
    active_profile: RwLock<ProfileId>,
    runtime: RwLock<Arc<R>>,
    generation: AtomicU64,
    cancellation: RwLock<CancellationToken>,
    tasks: Mutex<HashMap<u64, JoinHandle<()>>>,
    next_task_id: AtomicU64,
    switch_gate: tokio::sync::Mutex<()>,
}

impl<R: Send + Sync + 'static> RuntimeHost<R> {
    pub fn new(profile_id: ProfileId, runtime: Arc<R>) -> Self {
        Self {
            active_profile: RwLock::new(profile_id),
            runtime: RwLock::new(runtime),
            generation: AtomicU64::new(0),
            cancellation: RwLock::new(CancellationToken::new()),
            tasks: Mutex::new(HashMap::new()),
            next_task_id: AtomicU64::new(1),
            switch_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn active_profile(&self) -> ProfileId {
        self.active_profile
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn runtime(&self) -> Arc<R> {
        self.runtime
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn spawn_registered(&self, future: impl Future<Output = ()> + Send + 'static) -> u64 {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let task = tokio::spawn(future);
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, task);
        id
    }

    pub async fn switch_profile(
        &self,
        target: ProfileId,
        factory: &dyn RuntimeFactory<R>,
    ) -> Result<Arc<R>, RuntimeHostError> {
        let _switch = self.switch_gate.lock().await;
        if target == self.active_profile() {
            return Ok(self.runtime());
        }
        factory.preflight(&target).await?;

        let previous_profile = self.active_profile();
        let previous_runtime = self.runtime();
        factory.stop_playback(&previous_runtime).await;
        factory.flush(&previous_runtime).await?;
        self.cancel_and_join_tasks().await;

        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let cancellation = CancellationToken::new();
        *self
            .cancellation
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = cancellation.clone();

        match factory
            .build(&target, generation, cancellation.clone())
            .await
        {
            Ok(runtime) => {
                *self
                    .active_profile
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = target;
                *self
                    .runtime
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = runtime.clone();
                Ok(runtime)
            }
            Err(target_error) => {
                cancellation.cancel();
                let rollback_generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
                let rollback_token = CancellationToken::new();
                *self
                    .cancellation
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = rollback_token.clone();
                match factory
                    .build(&previous_profile, rollback_generation, rollback_token)
                    .await
                {
                    Ok(runtime) => {
                        *self
                            .runtime
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = runtime;
                        Err(RuntimeHostError::Construction {
                            target: target_error.to_string(),
                            rollback: "restored previous profile".to_owned(),
                        })
                    }
                    Err(rollback_error) => {
                        *self
                            .runtime
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous_runtime;
                        Err(RuntimeHostError::Construction {
                            target: target_error.to_string(),
                            rollback: rollback_error.to_string(),
                        })
                    }
                }
            }
        }
    }

    pub async fn shutdown(&self) {
        let _switch = self.switch_gate.lock().await;
        self.cancel_and_join_tasks().await;
    }

    async fn cancel_and_join_tasks(&self) {
        self.cancellation_token().cancel();
        let tasks = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *tasks)
                .into_values()
                .collect::<Vec<_>>()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for mut task in tasks {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestFactory {
        fail_target: AtomicBool,
    }

    #[async_trait]
    impl RuntimeFactory<String> for TestFactory {
        async fn preflight(&self, _profile_id: &ProfileId) -> Result<(), RuntimeHostError> {
            Ok(())
        }

        async fn flush(&self, _runtime: &Arc<String>) -> Result<(), RuntimeHostError> {
            Ok(())
        }

        async fn stop_playback(&self, _runtime: &Arc<String>) {}

        async fn build(
            &self,
            profile_id: &ProfileId,
            _generation: u64,
            _cancellation: CancellationToken,
        ) -> Result<Arc<String>, RuntimeHostError> {
            if self.fail_target.swap(false, Ordering::AcqRel) {
                Err(RuntimeHostError::Preflight("forced".to_owned()))
            } else {
                Ok(Arc::new(profile_id.as_str().to_owned()))
            }
        }
    }

    #[tokio::test]
    async fn failed_switch_rebuilds_previous_profile() {
        let owner = ProfileId::new();
        let target = ProfileId::new();
        let host = RuntimeHost::new(owner.clone(), Arc::new(owner.as_str().to_owned()));
        let factory = TestFactory {
            fail_target: AtomicBool::new(true),
        };

        assert!(host.switch_profile(target, &factory).await.is_err());
        assert_eq!(host.active_profile(), owner);
        assert_eq!(host.runtime().as_str(), owner.as_str());
    }
}
