use super::{sync::SyncRefCell, UefiOS};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use core::{cell::RefCell, pin::Pin, task::Context};
use futures::{
    task::{waker_ref, ArcWake},
    Future,
};

pub(super) type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static>>;

static EXECUTOR: SyncRefCell<Executor> = SyncRefCell::new(Executor {
    tasks: VecDeque::new(),
});

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

impl ArcWake for Task {
    fn wake_by_ref(task: &Arc<Self>) {
        if !task.inner.borrow().in_queue {
            task.inner.borrow_mut().in_queue = true;
            EXECUTOR.borrow_mut().tasks.push_back(task.clone());
        }
    }
}

pub struct Executor {
    // TODO(veluca): scheduling.
    tasks: VecDeque<Arc<Task>>,
}

impl Executor {
    pub fn run(os: UefiOS) -> ! {
        loop {
            let task = EXECUTOR
                .borrow_mut()
                .tasks
                .pop_front()
                .expect("Executor should never run out of tasks");

            task.inner.borrow_mut().in_queue = false;
            let waker = waker_ref(&task);
            let context = &mut Context::from_waker(&waker);
            let mut fut = task.inner.borrow_mut().future.take().unwrap();
            let begin = os.timer().micros();
            let status = fut.as_mut().poll(context);
            let end = os.timer().micros();
            task.inner.borrow_mut().micros += end - begin;
            if status.is_pending() {
                task.inner.borrow_mut().future = Some(fut);
            }
        }
    }

    pub(super) fn spawn(task: Arc<Task>) {
        EXECUTOR.borrow_mut().tasks.push_back(task);
    }
}
