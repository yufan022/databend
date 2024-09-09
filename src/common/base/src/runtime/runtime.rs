// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::backtrace::Backtrace;
use std::future::Future;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use futures::future;
use log::warn;
use tokio::runtime::Builder;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::runtime::catch_unwind::CatchUnwindFuture;
use crate::runtime::MemStat;

/// Methods to spawn tasks.
pub trait TrySpawn {
    /// Tries to spawn a new asynchronous task, returning a tokio::JoinHandle for it.
    ///
    /// It allows to return an error before spawning the task.
    #[track_caller]
    fn try_spawn<T>(&self, id: impl Into<String>, task: T) -> Result<JoinHandle<T::Output>>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static;

    /// Spawns a new asynchronous task, returning a tokio::JoinHandle for it.
    ///
    /// A default impl of this method just calls `try_spawn` and just panics if there is an error.
    #[track_caller]
    fn spawn<T>(&self, id: impl Into<String>, task: T) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        self.try_spawn(id, task).unwrap()
    }
}

impl<S: TrySpawn> TrySpawn for Arc<S> {
    #[track_caller]
    fn try_spawn<T>(&self, id: impl Into<String>, task: T) -> Result<JoinHandle<T::Output>>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        self.as_ref().try_spawn(id, task)
    }

    #[track_caller]
    fn spawn<T>(&self, id: impl Into<String>, task: T) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        self.as_ref().spawn(id, task)
    }
}

/// Tokio Runtime wrapper.
/// If a runtime is in an asynchronous context, shutdown it first.
pub struct Runtime {
    /// Runtime handle.
    handle: Handle,

    /// Memory tracker for this runtime
    tracker: Arc<MemStat>,

    /// Use to receive a drop signal when dropper is dropped.
    _dropper: Dropper,
}

impl Runtime {
    fn create(name: Option<String>, tracker: Arc<MemStat>, builder: &mut Builder) -> Result<Self> {
        let runtime = builder
            .build()
            .map_err(|tokio_error| ErrorCode::TokioError(tokio_error.to_string()))?;

        let (send_stop, recv_stop) = oneshot::channel();

        let handle = runtime.handle().clone();

        // Block the runtime to shutdown.
        let join_handler = thread::spawn(move || {
            // We ignore channel is closed.
            let _ = runtime.block_on(recv_stop);

            match !cfg!(debug_assertions) {
                true => false,
                false => {
                    let instant = Instant::now();
                    // We wait up to 3 seconds to complete the runtime shutdown.
                    runtime.shutdown_timeout(Duration::from_secs(3));

                    instant.elapsed() >= Duration::from_secs(3)
                }
            }
        });

        Ok(Runtime {
            handle,
            tracker,
            _dropper: Dropper {
                name,
                close: Some(send_stop),
                join_handler: Some(join_handler),
            },
        })
    }

    fn tracker_builder(mem_stat: Arc<MemStat>) -> tokio::runtime::Builder {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .enable_all()
            .on_thread_start(mem_stat.on_start_thread());

        builder
    }

    pub fn get_tracker(&self) -> Arc<MemStat> {
        self.tracker.clone()
    }

    /// Spawns a new tokio runtime with a default thread count on a background
    /// thread and returns a `Handle` which can be used to spawn tasks via
    /// its executor.
    pub fn with_default_worker_threads() -> Result<Self> {
        let mem_stat = MemStat::create(String::from("UnnamedRuntime"));
        let mut runtime_builder = Self::tracker_builder(mem_stat.clone());

        #[cfg(debug_assertions)]
        {
            // We need to pass the thread name in the unit test, because the thread name is the test name
            if matches!(std::env::var("UNIT_TEST"), Ok(var_value) if var_value == "TRUE") {
                if let Some(thread_name) = std::thread::current().name() {
                    runtime_builder.thread_name(thread_name);
                }
            }

            runtime_builder.thread_stack_size(20 * 1024 * 1024);
        }
        runtime_builder.thread_stack_size(100 * 1024 * 1024);

        Self::create(None, mem_stat, &mut runtime_builder)
    }

    #[allow(unused_mut)]
    pub fn with_worker_threads(workers: usize, mut thread_name: Option<String>) -> Result<Self> {
        let mut mem_stat_name = String::from("UnnamedRuntime");

        if let Some(thread_name) = thread_name.as_ref() {
            mem_stat_name = format!("{}Runtime", thread_name);
        }

        let mem_stat = MemStat::create(mem_stat_name);
        let mut runtime_builder = Self::tracker_builder(mem_stat.clone());

        #[cfg(debug_assertions)]
        {
            // We need to pass the thread name in the unit test, because the thread name is the test name
            if matches!(std::env::var("UNIT_TEST"), Ok(var_value) if var_value == "TRUE") {
                if let Some(cur_thread_name) = std::thread::current().name() {
                    thread_name = Some(cur_thread_name.to_string());
                }
            }

            runtime_builder.thread_stack_size(20 * 1024 * 1024);
        }
        runtime_builder.thread_stack_size(100 * 1024 * 1024);

        if let Some(thread_name) = &thread_name {
            runtime_builder.thread_name(thread_name);
        }

        Self::create(
            thread_name,
            mem_stat,
            runtime_builder.worker_threads(workers),
        )
    }

    pub fn inner(&self) -> tokio::runtime::Handle {
        self.handle.clone()
    }

    pub fn block_on<T, F>(&self, future: F) -> F::Output
    where F: Future<Output = Result<T>> + Send + 'static {
        let future = CatchUnwindFuture::create(future);
        self.handle.block_on(future).flatten()
    }

    // For each future of `futures`, before being executed
    // a permit will be acquired from the semaphore, and released when it is done
    pub async fn try_spawn_batch<Fut>(
        &self,
        semaphore: Semaphore,
        futures: impl IntoIterator<Item = Fut>,
    ) -> Result<Vec<JoinHandle<Fut::Output>>>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let semaphore = Arc::new(semaphore);
        let iter = futures.into_iter().map(|v| {
            |permit| async {
                let r = v.await;
                drop(permit);
                r
            }
        });
        self.try_spawn_batch_with_owned_semaphore(semaphore, iter)
            .await
    }

    // For each future of `futures`, before being executed
    // a permit will be acquired from the semaphore, and released when it is done

    // Please take care using the `semaphore`.
    // If sub task may be spawned in the `futures`, and uses the
    // clone of semaphore to acquire permits, please release the permits on time,
    // or give sufficient(but not abundant, of course) permits, to tolerant the
    // maximum degree of parallelism, otherwise, it may lead to deadlock.
    pub async fn try_spawn_batch_with_owned_semaphore<F, Fut>(
        &self,
        semaphore: Arc<Semaphore>,
        futures: impl IntoIterator<Item = F>,
    ) -> Result<Vec<JoinHandle<Fut::Output>>>
    where
        F: FnOnce(OwnedSemaphorePermit) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let iter = futures.into_iter();
        let mut handlers =
            Vec::with_capacity(iter.size_hint().1.unwrap_or_else(|| iter.size_hint().0));
        for fut in iter {
            let semaphore = semaphore.clone();
            // Although async task is rather lightweight, it do consumes resources,
            // so we acquire a permit BEFORE spawn.
            // Thus, the `futures` passed into this method is NOT suggested to be "materialized"
            // iterator, e.g. Vec<..>
            let permit = semaphore.acquire_owned().await.map_err(|e| {
                ErrorCode::Internal(format!("semaphore closed, acquire permit failure. {}", e))
            })?;
            let handler = self
                .handle
                .spawn(async_backtrace::location!().frame(async move {
                    // take the ownership of the permit, (implicitly) drop it when task is done
                    fut(permit).await
                }));
            handlers.push(handler)
        }

        Ok(handlers)
    }

    // TODO(Winter): remove
    // Please do not use this method(it's temporary)
    #[async_backtrace::framed]
    pub async fn spawn_blocking<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        match_join_handle(self.handle.spawn_blocking(f)).await
    }
}

impl TrySpawn for Runtime {
    #[track_caller]
    fn try_spawn<T>(&self, id: impl Into<String>, task: T) -> Result<JoinHandle<T::Output>>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        let id = id.into();
        let task = match id == GLOBAL_TASK {
            true => async_backtrace::location!(String::from(GLOBAL_TASK_DESC)).frame(task),
            false => {
                async_backtrace::location!(format!("Running query {} spawn task", id)).frame(task)
            }
        };
        Ok(self.handle.spawn(task))
    }
}

/// Dropping the dropper will cause runtime to shutdown.
pub struct Dropper {
    name: Option<String>,
    close: Option<oneshot::Sender<()>>,
    join_handler: Option<thread::JoinHandle<bool>>,
}

impl Drop for Dropper {
    fn drop(&mut self) {
        // Send a signal to say i am dropping.
        if let Some(close_sender) = self.close.take() {
            if close_sender.send(()).is_ok() {
                match self.join_handler.take().unwrap().join() {
                    Err(e) => warn!("Runtime dropper panic, {:?}", e),
                    Ok(true) => {
                        // When the runtime shutdown is blocked for more than 3 seconds,
                        // we will print the backtrace in the warn log, which will help us debug.
                        warn!(
                            "Runtime dropper is blocked 3 seconds, runtime name: {:?}, drop backtrace: {:?}",
                            self.name,
                            Backtrace::capture()
                        );
                    }
                    _ => {}
                };
            }
        }
    }
}

pub async fn match_join_handle<T>(handle: JoinHandle<Result<T>>) -> Result<T> {
    match handle.await {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(cause)) => Err(cause),
        Err(join_error) => match join_error.is_cancelled() {
            true => Err(ErrorCode::TokioError("Tokio error is cancelled.")),
            false => {
                let panic_error = join_error.into_panic();
                match panic_error.downcast_ref::<&'static str>() {
                    None => match panic_error.downcast_ref::<String>() {
                        None => Err(ErrorCode::PanicError("Sorry, unknown panic message")),
                        Some(message) => Err(ErrorCode::PanicError(message.to_string())),
                    },
                    Some(message) => Err(ErrorCode::PanicError(message.to_string())),
                }
            }
        },
    }
}

/// Run multiple futures parallel
/// using a semaphore to limit the parallelism number, and a specified thread pool to run the futures.
/// It waits for all futures to complete and returns their results.
pub async fn execute_futures_in_parallel<Fut>(
    futures: impl IntoIterator<Item = Fut>,
    thread_nums: usize,
    permit_nums: usize,
    thread_name: String,
) -> Result<Vec<Fut::Output>>
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    // 1. build the runtime.
    let semaphore = Semaphore::new(permit_nums);
    let runtime = Arc::new(Runtime::with_worker_threads(
        thread_nums,
        Some(thread_name),
    )?);

    // 2. spawn all the tasks to the runtime with semaphore.
    let join_handlers = runtime.try_spawn_batch(semaphore, futures).await?;

    // 3. get all the result.
    future::try_join_all(join_handlers)
        .await
        .map_err(|e| ErrorCode::Internal(format!("try join all futures failure, {}", e)))
}

pub const GLOBAL_TASK: &str = "Zxv39PlwG1ahbF0APRUf03";
pub const GLOBAL_TASK_DESC: &str = "Global spawn task";
