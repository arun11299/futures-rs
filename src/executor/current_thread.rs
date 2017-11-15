//! Execute tasks on the current thread
//!
//! [`CurrentThread`] provides an executor that keeps spawned futures keeps
//! spawned futures on the same thread that they are spawned on. This allows it
//! to execute futures that are `!Send`. For more details on general executor
//! concepts, like spawning tasks. See [here].
//!
//! Before being able to spawn futures onto [`CurrentThread`], an executor
//! context must be setup. This is done by calling either [`block_with_init`] or
//! [`block_on_all`]. From within that context, [`CurrentThread::spawn`] may be
//! called with the future to run in the background.
//!
//! ```
//! # use futures::executor::current_thread::*;
//! use futures::future::lazy;
//!
//! // Calling spawn here results in a panic
//! // CurrentThread::spawn(my_future);
//!
//! CurrentThread::block_on_all(lazy(|| {
//!     // The execution context is setup, futures may be spawned.
//!     CurrentThread::spawn(lazy(|| {
//!         println!("called from the current thread executor");
//!         Ok(())
//!     }));
//!
//!     Ok::<_, ()>(())
//! }));
//! ```
//!
//! # Execution model
//!
//! When a [`CurrentThread`] execution context is setup with one of the
//! `block_*` functions, the current thread will block and all the tasks managed
//! by the executor are driven to completion. Whenever a task receives a
//! notification, it is pushed to the end of a scheduled list. The
//! [`CurrentThread`] executor will drain this list, advancing the state of each
//! future.
//!
//! All futures managed by [`CurrentThread`] will remain on the current thread,
//! as such, [`CurrentThread`] is able to safely spawn futures that are `!Send`.
//!
//! Once a task is complete, it is dropped. Once all [non daemon](#daemon-tasks) tasks are
//! completed, [`CurrentThread`] unblocks.
//!
//! [`CurrentThread`] makes a best effort to fairly schedule tasks that it
//! manages.
//!
//! # Daemon tasks
//!
//! A daemon task is a task that does not require to be complete in order for
//! [`CurrentThread`] to complete running. These are useful for background
//! "maintenance" tasks that are not critical to the completion of the primary
//! computation.
//!
//! When [`CurrentThread`] completes running and unblocks, any daemon tasks that
//! have not yet completed are immediately dropped.
//!
//! A daemon task can be spawned with [`CurrentThread::spawn_daemon`].
//!
//! [here]: https://tokio.rs/docs/going-deeper-futures/tasks/
//! [`CurrentThread`]: struct.CurrentThread.html
//! [`block_with_init`]: struct.CurrentThread.html#method.block_with_init
//! [`block_on_all`]: struct.CurrentThread.html#method.block_on_all
//! [`CurrentThread::spawn`]: struct.CurrentThread.html#method.spawn
//! [`CurrentThread::spawn_daemon`]: struct.CurrentThread.html#method.spawn_daemon

use Async;
use executor::{self, Spawn};
use future::{self, Future, Executor, ExecuteError, ExecuteErrorKind};
use scheduler;
use task_impl::ThreadNotify;

use std::prelude::v1::*;

use std::{fmt, ptr, thread};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// Executes tasks on the current thread.
///
/// All tasks spawned using this executor will be executed on the current thread
/// as non-daemon tasks. As such, [`CurrentThread`] will wait for these tasks to
/// complete before returning from the various `block_*` functions.
///
/// For more details, see the [module level](index.html) documentation.
#[derive(Debug, Clone)]
pub struct CurrentThread {
    // Prevent the handle from moving across threads.
    _p: ::std::marker::PhantomData<Rc<()>>,
}

/// Executes dameonized tasks on the current thread.
///
/// All tasks spawned using this executor will be executed on the current thread
/// as daemon tasks. As such, [`CurrentThread`] will **not** wait for these tasks to
/// complete before returning from the various `block_*` functions.
///
/// For more details, see the [module level](index.html) documentation.
#[derive(Debug, Clone)]
pub struct DaemonExecutor {
    // Prevent the handle from moving across threads.
    _p: ::std::marker::PhantomData<Rc<()>>,
}

// An object for cooperatively executing multiple tasks on a single thread.
// Useful for working with non-`Send` futures.
//
// NB: this is not `Send`
#[derive(Debug)]
struct TaskRunner {
    /// Executes futures.
    scheduler: Scheduler,
}

#[derive(Debug)]
struct CurrentRunner {
    /// When set to true, the executor should return immediately, even if there
    /// still are non-daemon tasks to run.
    cancel: Cell<bool>,

    /// Number of non-daemon tasks currently being executed by the runner.
    non_daemons: Cell<usize>,

    /// Raw pointer to the current scheduler.
    ///
    /// The raw pointer is required in order to store it in a thread-local slot.
    scheduler: Cell<*mut Scheduler>,
}

type Scheduler = scheduler::Scheduler<SpawnedFuture, Arc<ThreadNotify>>;

#[derive(Debug)]
struct SpawnedFuture {
    /// True if the spawned future should not prevent the executor from
    /// terminating.
    daemon: bool,

    /// The task to execute.
    inner: Task,
}

struct Task(Spawn<Box<Future<Item = (), Error = ()>>>);

/// Current thread's task runner. This is set in `TaskRunner::with`
thread_local!(static CURRENT: CurrentRunner = CurrentRunner {
    cancel: Cell::new(false),
    non_daemons: Cell::new(0),
    scheduler: Cell::new(ptr::null_mut()),
});

impl CurrentThread {
    /// Returns an executor that executes tasks on the current thread.
    ///
    /// This executor can be moved across threads. Spawned tasks will be executed
    /// on the same thread that spawn was called on.
    ///
    /// The user of `CurrentThread` must ensure that when a future is submitted
    /// to the executor, that it is done from the context of a `block_*` call.
    ///
    /// For more details, see the [module level](index.html) documentation.
    pub fn current() -> CurrentThread {
        CurrentThread {
            _p: ::std::marker::PhantomData,
        }
    }

    /// Returns an executor that spawns daemon tasks on the current thread.
    ///
    /// This executor can be moved across threads. Spawned tasks will be
    /// executed on the same thread that spawn was called on.
    ///
    /// The user of `CurrentThread` must ensure that when a future is submitted
    /// to the executor, that it is done from the context of a `block_*` call.
    ///
    /// For more details, see the [module level](index.html) documentation.
    pub fn daemon_executor(&self) -> DaemonExecutor {
        DaemonExecutor {
            _p: ::std::marker::PhantomData,
        }
    }

    /// Execute the given future *synchronously* on the current thread, blocking
    /// until it (and all spawned tasks) completes and returning its result.
    ///
    /// In more detail, this function blocks until:
    ///
    /// - the given future completes, *and*
    /// - all spawned tasks complete, or `cancel_all_spawned` is invoked
    ///
    /// Note that there is no `'static` or `Send` requirement on the future.
    pub fn block_on_all<F: Future>(future: F) -> Result<F::Item, F::Error> {
        TaskRunner::enter(|| future)
    }

    /// Execute the given closure, then block until all spawned tasks complete.
    ///
    /// In more detail, this function will block until:
    /// - All spawned tasks are complete, or
    /// - `cancel_all_spawned` is invoked.
    pub fn block_with_init<F>(f: F) where F: FnOnce() {
        drop(TaskRunner::enter(|| {
            f();
            future::ok::<(), ()>(())
        }))
    }

    /// Spawns a task, i.e. one that must be explicitly either
    /// blocked on or killed off before `block_*` will return.
    ///
    /// # Panics
    ///
    /// This function can only be invoked within a future given to a `block_*`
    /// invocation; any other use will result in a panic.
    pub fn spawn<F>(future: F)
    where F: Future<Item = (), Error = ()> + 'static
    {
        spawn(future, false).unwrap_or_else(|_| {
            panic!("cannot call `spawn` unless your thread is already \
                    in the context of a call to `block_on_all` or \
                    `block_with_init`")
        })
    }

    /// Spawns a daemon, which does *not* block the pending `block_on_all` call.
    ///
    /// # Panics
    ///
    /// This function can only be invoked within a future given to a `block_*`
    /// invocation; any other use will result in a panic.
    pub fn spawn_daemon<F>(future: F)
    where F: Future<Item = (), Error = ()> + 'static
    {
        spawn(future, true).unwrap_or_else(|_| {
            panic!("cannot call `spawn_daemon` unless your thread is already \
                    in the context of a call to `block_on_all` or \
                    `block_with_init`")
        })
    }

    /// Cancels *all* spawned tasks and daemons.
    ///
    /// # Panics
    ///
    /// This function can only be invoked within a future given to a `block_*`
    /// invocation; any other use will result in a panic.
    pub fn cancel_all_spawned() {
        CurrentRunner::with(|runner| runner.cancel_all_spawned())
            .unwrap_or_else(|()| {
                panic!("cannot call `cancel_all_spawned` unless your thread is \
                        already in the context of a call to `block_on_all` or \
                        `block_with_init`")
            })
    }
}

impl<F> Executor<F> for CurrentThread
where F: Future<Item = (), Error = ()> + 'static
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        spawn(future, false)
    }
}


impl<F> Executor<F> for DaemonExecutor
where F: Future<Item = (), Error = ()> + 'static
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        spawn(future, true)
    }
}

fn spawn<F>(future: F, daemon: bool) -> Result<(), ExecuteError<F>>
where F: Future<Item = (), Error = ()> + 'static,
{
    CURRENT.with(|current| {
        if current.scheduler.get().is_null() {
            Err(ExecuteError::new(ExecuteErrorKind::Shutdown, future))
        } else {
            let spawned = SpawnedFuture {
                daemon: daemon,
                inner: Task::new(future),
            };

            if !daemon {
                let non_daemons = current.non_daemons.get();
                current.non_daemons.set(non_daemons + 1);
            }

            unsafe {
                (*current.scheduler.get()).push(spawned);
            }

            Ok(())
        }
    })
}

impl TaskRunner {
    /// Return a new `TaskRunner`
    fn new(thread_notify: Arc<ThreadNotify>) -> TaskRunner {
        TaskRunner {
            scheduler: scheduler::Scheduler::new(thread_notify),
        }
    }

    /// Enter a new `TaskRunner` context
    fn enter<F, A>(f: F) -> Result<A::Item, A::Error>
        where F: FnOnce() -> A,
              A: Future,
    {
        // Create a new task runner that will be used for the duration of `f`.
        ThreadNotify::with_current(|thread_notify| {
            // The runner has to be created outside of the MY_TASK_RUNNER.with
            // block.
            let mut runner = TaskRunner::new(thread_notify.clone());

            CURRENT.with(|current| {
                // Make sure that another task runner is not set.
                assert!(current.scheduler.get().is_null());

                // Set the scheduler to the TLS and perform setup work,
                // returning a future to execute.
                //
                // This could possibly spawn other tasks.
                let future = current.set_scheduler(&mut runner.scheduler, f);

                // Execute the runner
                runner.finish(thread_notify, current, future)
            })
        })
    }

    fn finish<F: Future>(&mut self,
                         thread_notify: &Arc<ThreadNotify>,
                         current: &CurrentRunner,
                         future: F)
        -> Result<F::Item, F::Error>
    {
        let mut result = None;
        let mut future = Some(executor::spawn(future));

        while future.is_some() || current.non_daemons.get() > 0 {
            if current.cancel.get() {
                // TODO: This probably can be improved
                current.cancel.set(false);

                debug_assert!(current.scheduler.get().is_null());
                self.scheduler = scheduler::Scheduler::new(thread_notify.clone());
            }

            // Set the scheduler to the TLS before polling the future. This
            // enables the future to potentially spawn more tasks onto the
            // current thread executor.
            let res = current.set_scheduler(&mut self.scheduler, || {
                future.as_mut()
                    .map(|f| f.poll_future_notify(thread_notify, 0))
            });

            match res {
                Some(Ok(Async::Ready(e))) => {
                    result = Some(Ok(e));
                    future = None;
                }
                Some(Err(e)) => {
                    result = Some(Err(e));
                    future = None;
                }
                Some(Ok(Async::NotReady)) |
                None => {}
            }

            self.poll_all(current);

            if future.is_some() || current.non_daemons.get() > 0 {
                thread_notify.park();
            }
        }

        result.unwrap()
    }

    fn poll_all(&mut self, current: &CurrentRunner) {
        use scheduler::Tick;

        loop {
            let res = self.scheduler.tick(|scheduler, spawned, notify| {
                current.set_scheduler(scheduler, || {
                    match spawned.inner.0.poll_future_notify(notify, 0) {
                        Ok(Async::Ready(_)) | Err(_) => {
                            Async::Ready(spawned.daemon)
                        }
                        Ok(Async::NotReady) => Async::NotReady,
                    }
                })
            });

            match res {
                Tick::Data(is_daemon) => {
                    if !is_daemon {
                        let non_daemons = current.non_daemons.get();
                        debug_assert!(non_daemons > 0);
                        current.non_daemons.set(non_daemons - 1);
                    }
                },
                Tick::Empty => {
                    return;
                }
                Tick::Inconsistent => {
                    // Yield the thread and loop
                    thread::yield_now();
                }
            }
        }
    }
}

impl CurrentRunner {
    fn with<F, R>(f: F) -> Result<R, ()>
    where F: FnOnce(&Self) -> R,
    {
        CURRENT.with(|current| {
            if current.scheduler.get().is_null() {
                Err(())
            } else {
                Ok(f(current))
            }
        })
    }

    /// Set the provided scheduler to the TLS slot for the duration of the
    /// closure
    fn set_scheduler<F, R>(&self, scheduler: &mut Scheduler, f: F) -> R
    where F: FnOnce() -> R
    {
        // Ensure that the runner is removed from the thread-local context
        // when leaving the scope. This handles cases that involve panicking.
        struct Reset<'a>(&'a CurrentRunner);

        impl<'a> Drop for Reset<'a> {
            fn drop(&mut self) {
                self.0.scheduler.set(ptr::null_mut());
            }
        }

        let _reset = Reset(self);

        self.scheduler.set(scheduler as *mut Scheduler);

        f()
    }

    fn cancel_all_spawned(&self) {
        self.cancel.set(true);
    }
}

impl Task {
    fn new<T: Future<Item = (), Error = ()> + 'static>(f: T) -> Self {
        Task(executor::spawn(Box::new(f)))
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Task")
            .finish()
    }
}
