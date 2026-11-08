use std::future::Future;

use tokio::task::{AbortHandle, JoinError, JoinHandle};

use super::JobClaim;

/// Why a worker task ended without returning its normal handler result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkerStopReason {
    Cancelled,
    Panicked,
}

/// Proof that the task which owned a job claim has terminated and been joined.
///
/// The private claim and constructor prevent callers from recovering a running
/// claim based on lease expiry alone. Obtain this value only from
/// [`SupervisedJobTask::join`] or [`SupervisedJobTask::cancel_and_join`].
pub struct StoppedClaim {
    claim: JobClaim,
    reason: WorkerStopReason,
}

impl StoppedClaim {
    pub(super) fn claim(&self) -> &JobClaim {
        &self.claim
    }

    pub(super) fn reason(&self) -> WorkerStopReason {
        self.reason
    }
}

/// A worker task and the durable claim whose effects it owns.
#[must_use = "join the worker or cancel_and_join it before recovering its claim"]
pub struct SupervisedJobTask<T> {
    claim: Option<JobClaim>,
    task: Option<JoinHandle<T>>,
}

/// The result of joining a supervised worker task.
#[must_use = "persist the handler result or recover the joined claim"]
pub enum JobTaskExit<T> {
    Completed { claim: JobClaim, output: T },
    Stopped(StoppedClaim),
}

struct AbortOnDrop(Option<AbortHandle>);

impl AbortOnDrop {
    fn new(task: &JoinHandle<impl Sized>) -> Self {
        Self(Some(task.abort_handle()))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

impl<T> SupervisedJobTask<T>
where
    T: Send + 'static,
{
    /// Starts an owned worker future after its durable claim has been acquired.
    pub fn spawn<F>(claim: JobClaim, worker: F) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        Self {
            claim: Some(claim),
            task: Some(tokio::spawn(worker)),
        }
    }

    /// Waits for natural completion, cancellation, or panic, retaining ownership
    /// of the claim until Tokio confirms that the task has terminated.
    pub async fn join(mut self) -> JobTaskExit<T> {
        let claim = self
            .claim
            .take()
            .expect("supervised job task claim is available until join");
        let task = self
            .task
            .take()
            .expect("supervised job task handle is available until join");
        joined_task(claim, task).await
    }

    /// Aborts the worker future and waits for its termination before exposing
    /// the claim for recovery. This stops local polling; it cannot prove that an
    /// already-issued external request was canceled by its remote service.
    pub async fn cancel_and_join(mut self) -> JobTaskExit<T> {
        let claim = self
            .claim
            .take()
            .expect("supervised job task claim is available until join");
        let task = self
            .task
            .take()
            .expect("supervised job task handle is available until join");
        task.abort();
        joined_task(claim, task).await
    }
}

impl<T> Drop for SupervisedJobTask<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn joined<T>(claim: JobClaim, result: Result<T, JoinError>) -> JobTaskExit<T> {
    match result {
        Ok(output) => JobTaskExit::Completed { claim, output },
        Err(error) => {
            let reason = if error.is_panic() {
                WorkerStopReason::Panicked
            } else {
                WorkerStopReason::Cancelled
            };
            JobTaskExit::Stopped(StoppedClaim { claim, reason })
        }
    }
}

async fn joined_task<T>(claim: JobClaim, task: JoinHandle<T>) -> JobTaskExit<T> {
    let mut abort_on_drop = AbortOnDrop::new(&task);
    let result = task.await;
    abort_on_drop.disarm();
    joined(claim, result)
}

#[cfg(test)]
mod tests {
    use super::AbortOnDrop;

    #[tokio::test]
    async fn abandoned_join_guard_aborts_the_owned_task() {
        let task = tokio::spawn(std::future::pending::<()>());
        let abort_on_drop = AbortOnDrop::new(&task);

        drop(abort_on_drop);

        assert!(task.await.unwrap_err().is_cancelled());
    }
}
