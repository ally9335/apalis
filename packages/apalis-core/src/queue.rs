use std::fmt::Debug;

use actix::{Actor, ActorContext, AsyncContext, Context, Handler, StreamHandler};
use anyhow::Context as AnyHowContext;
use futures::Future;
use serde::{de::DeserializeOwned, Serialize};
use tower::{Service, ServiceExt};

use crate::{
    error::JobError, request::JobRequest, response::JobResult, storage::Storage, streams::FetchJob,
    worker::WorkerManagement,
};
/// A queue represents a consumer of a [Storage].
/// A [tower::Service] must be provided to be called when a new job is detected.
#[must_use]
pub struct Queue<T: Serialize, S: Storage<Output = T>, H> {
    storage: S,
    status: QueueStatus,
    handler: Box<H>,
}

/// Each [Queue] sends heartbeat messages
#[derive(Debug, Clone, Hash, PartialEq, Eq)]

pub enum Heartbeat {
    EnqueueScheduled(i32),
    RenqueueActive,
    Register,
    RenqueueOrpharned,
    Other(&'static str),
}

/// Represents the status of a queue.
/// Mainly consumed by [crate::worker::Worker]
#[derive(Default, Clone)]
pub struct QueueStatus {
    load: i64,
    //since: chrono::DateTime<chrono::Local>,
    id: uuid::Uuid,
}

/// Represents a queue error.
#[derive(Debug)]
pub enum QueueError {}

impl<T, S, H> Queue<T, S, H>
where
    T: 'static + Serialize + Debug + DeserializeOwned,
    S: 'static + Storage<Output = T> + Unpin,
{
    pub fn new(storage: S, handler: H) -> Self {
        Queue {
            storage,
            handler: Box::from(handler),
            status: Default::default(),
        }
    }
}

impl<T: 'static, S: 'static, H: 'static, F: 'static> Actor for Queue<T, S, H>
where
    S: Storage + Unpin + Storage<Output = T>,
    T: Serialize + Debug + DeserializeOwned + Send,
    H: Service<JobRequest<T>, Response = JobResult, Error = JobError, Future = F>
        + Unpin
        + Send
        + 'static,
    F: Future,
{
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.notify(WorkerManagement::Setup);
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {}
}

impl<T: 'static, S: 'static, H: 'static, F: 'static> Handler<WorkerManagement> for Queue<T, S, H>
where
    S: Storage + Unpin + Storage<Output = T>,
    T: Serialize + Debug + DeserializeOwned + Send,
    H: Service<JobRequest<T>, Response = JobResult, Error = JobError, Future = F>
        + Unpin
        + Send
        + 'static,
    F: Future,
{
    type Result = Result<QueueStatus, QueueError>;

    fn handle(&mut self, msg: WorkerManagement, ctx: &mut Self::Context) -> Self::Result {
        let Queue { status, .. } = self;
        let res = match msg {
            WorkerManagement::Status => {}
            WorkerManagement::Stop => ctx.stop(),
            WorkerManagement::Restart => {}
            WorkerManagement::Setup => {
                let res = self.storage.heartbeat(Heartbeat::Register);
                let fut = async move {
                    res.await.unwrap();
                };
                let fut = actix::fut::wrap_future::<_, Self>(fut);
                ctx.spawn(fut);
            }
            WorkerManagement::Ack(_) => todo!(),
            WorkerManagement::Kill(_) => todo!(),
        };
        Ok(status.clone())
    }
}

impl<T: 'static, S: 'static, H: 'static, F: 'static> StreamHandler<Heartbeat> for Queue<T, S, H>
where
    S: Storage<Output = T> + Unpin,
    T: Serialize + Debug + DeserializeOwned + Send,
    H: Service<JobRequest<T>, Response = JobResult, Error = JobError, Future = F>
        + Unpin
        + Send
        + 'static,
    F: Future,
{
    fn handle(&mut self, beat: Heartbeat, ctx: &mut Self::Context) {
        let queue = &mut self.storage;
        let heartbeat = queue.heartbeat(beat);
        let fut = async {
            heartbeat.await.unwrap();
        };
        let fut = actix::fut::wrap_future::<_, Self>(fut);
        ctx.spawn(fut);
    }
}

impl<T: 'static, S: 'static, H: 'static, F: 'static> StreamHandler<FetchJob> for Queue<T, S, H>
where
    S: Storage<Output = T> + Unpin,
    T: Serialize + Debug + DeserializeOwned + Send,
    H: Service<JobRequest<T>, Response = JobResult, Error = JobError, Future = F>
        + Unpin
        + Send
        + 'static,
    F: Future<Output = Result<JobResult, JobError>>,
{
    fn handle(&mut self, _msg: FetchJob, ctx: &mut Self::Context) {
        let mut storage = self.storage.clone();
        let fut = {
            let job = storage.consume();
            let service: *mut Box<H> = &mut self.handler;
            async move {
                let handle = unsafe {
                    let handle = (*service).ready().await.unwrap();
                    handle
                };
                let job = match job.await {
                    Ok(job) => job,
                    Err(e) => {
                        return log::debug!("FetchJob failed with error {:?}", e);
                    }
                };

                if job.is_none() {
                    return;
                }
                let job = job.unwrap();
                let id = job.id();

                let res = handle.call(job).await.with_context(|| {
                    format!(
                        "Job [{}] Failed to complete job in queue {}",
                        id,
                        std::any::type_name::<T>()
                    )
                });
                let job_id = id.clone();
                let finalize = match res {
                    Ok(r) => match r {
                        JobResult::Success => storage.ack(id).await,
                        JobResult::Retry => storage.retry(id).await,
                        JobResult::Kill => storage.kill(id).await,
                        JobResult::Reschedule(wait) => storage.reschedule(id, wait).await,
                    },
                    Err(e) => storage.reschedule(id, chrono::Duration::seconds(1)).await,
                };
                log::info!(
                    target: std::any::type_name::<S>(),
                    "Job [{}] finalized with res : {:?}",
                    job_id,
                    finalize
                );
            }
        };

        let fut = actix::fut::wrap_future::<_, Self>(fut);
        ctx.spawn(fut);
    }
}
