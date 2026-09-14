//! The task system: units of build work that the orchestrator schedules.
//!
//! A [`Task`] is a value constructed from its inputs; `run` receives the
//! orchestrator [`Context`], through which dependent tasks are spawned
//! (yielding futures of their outputs) and microVMs are booted. Tasks are
//! scheduled as `Box<dyn Task<Output = ErasedOutput>>`, so the scheduler
//! is fully insulated from what tasks do or return.

use std::any::Any;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};

use crate::orchestrator::Inner;
use crate::vm::{Vm, VmSpec};

pub type TaskFuture<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send>>;

pub trait Task: Send + 'static {
    type Output: Send + 'static;

    fn run(self: Box<Self>, ctx: Context) -> TaskFuture<Self::Output>;
}

/// Type-erased task output.
pub type ErasedOutput = Box<dyn Any + Send>;

/// What the scheduler handles: any task with its output type erased.
pub type DynTask = dyn Task<Output = ErasedOutput>;

struct Erased<T: Task>(T);

impl<T: Task> Task for Erased<T> {
    type Output = ErasedOutput;

    fn run(self: Box<Self>, ctx: Context) -> TaskFuture<ErasedOutput> {
        Box::pin(async move {
            let out = Task::run(Box::new(self.0), ctx).await?;
            Ok(Box::new(out) as ErasedOutput)
        })
    }
}

/// Given to every task: spawns dependent tasks and mediates access to the
/// outside world (the nix cache, scratch dirs, microVMs).
#[derive(Clone)]
pub struct Context {
    pub(crate) inner: Arc<Inner>,
}

impl Context {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        Context { inner }
    }

    /// Schedules `task` and returns a future of its output. The task starts
    /// running immediately; dropping the future does not cancel it.
    pub fn spawn<T: Task>(&self, task: T) -> TaskFuture<T::Output> {
        let erased: Box<DynTask> = Box::new(Erased(task));
        let ctx = self.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = Task::run(erased, ctx).await.and_then(|out| {
                out.downcast::<T::Output>()
                    .map(|b| *b)
                    .map_err(|_| anyhow!("unreachable"))
            });
            let _ = tx.send(result);
        });
        Box::pin(async { rx.await.context("task ended without a result")? })
    }

    /// Boots a microVM. All VM access goes through the orchestrator so
    /// instances can later be pooled or reused.
    pub fn boot_vm(&self, spec: VmSpec) -> anyhow::Result<Vm> {
        Vm::boot(spec)
    }

    pub fn cache_dir(&self) -> &Path {
        &self.inner.cache_dir
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.inner.cache_dir.join("tmp")
    }

    pub fn nix_cache(&self) -> &nix_cache::NixCache {
        &self.inner.nix_cache
    }

    pub fn image_path(&self, store_path: &str) -> PathBuf {
        crate::orchestrator::image_path_for(&self.inner.cache_dir, store_path)
    }

    /// Bounds concurrent network fetches.
    pub async fn fetch_permit(&self) -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
        self.inner
            .fetch_sem
            .clone()
            .acquire_owned()
            .await
            .context("fetch semaphore closed")
    }
}
