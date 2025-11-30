use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::task::Wake;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Waker};

use super::sync::SyncRefCell;
use crate::os::timer::Timer;

pub(super) type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static>>;

struct TaskInner {
    pub in_queue: bool,
    pub future: Option<BoxFuture>,
    pub micros: i64,
}

pub(super) struct Task {
    pub name: &'static str,
    inner: RefCell<TaskInner>,
}

impl Task {
    pub(super) fn new<Fut>(name: &'static str, future: Fut) -> Arc<Task>
    where
        Fut: Future<Output = ()> + 'static,
    {
        Arc::new(Task {
            name,
            inner: RefCell::new(TaskInner {
                in_queue: false,
                future: Some(Box::pin(future)),
                micros: 0,
            }),
        })
    }

    pub(super) fn micros(&self) -> i64 {
        self.inner.borrow().micros
    }
}

// SAFETY: we never create threads anyway.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        if !self.inner.borrow().in_queue {
            self.inner.borrow_mut().in_queue = true;
            EXECUTOR.borrow_mut().ready_tasks.push_back(self);
        }
    }
}

static EXECUTOR: SyncRefCell<Executor> = SyncRefCell::new(Executor {
    ready_tasks: VecDeque::new(),
});

pub struct Executor {
    // TODO(veluca): scheduling.
    ready_tasks: VecDeque<Arc<Task>>,
}

impl Executor {
    pub fn run() -> ! {
        loop {
            let task = EXECUTOR
                .borrow_mut()
                .ready_tasks
                .pop_front()
                .expect("Executor should never run out of ready tasks");

            task.inner.borrow_mut().in_queue = false;
            let waker = Waker::from(task.clone());
            let mut context = Context::from_waker(&waker);
            let mut fut = task.inner.borrow_mut().future.take().unwrap();
            let begin = Timer::micros();
            let status = fut.as_mut().poll(&mut context);
            let end = Timer::micros();
            task.inner.borrow_mut().micros += end - begin;
            if status.is_pending() {
                task.inner.borrow_mut().future = Some(fut);
            }
        }
    }

    pub(super) fn spawn(task: Arc<Task>) {
        EXECUTOR.borrow_mut().ready_tasks.push_back(task);
    }
}
