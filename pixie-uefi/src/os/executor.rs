use super::UefiOS;
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, task::Wake};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Waker},
};
use spin::Mutex;

pub(super) type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static + Send>>;

struct TaskInner {
    pub in_queue: bool,
    pub future: Option<BoxFuture>,
    pub micros: i64,
}

pub(super) struct Task {
    pub name: &'static str,
    inner: Mutex<TaskInner>,
}

impl Task {
    pub(super) fn new<Fut>(name: &'static str, future: Fut) -> Arc<Task>
    where
        Fut: Future<Output = ()> + 'static + Send,
    {
        Arc::new(Task {
            name,
            inner: Mutex::new(TaskInner {
                in_queue: false,
                future: Some(Box::pin(future)),
                micros: 0,
            }),
        })
    }

    pub(super) fn micros(&self) -> i64 {
        self.inner.lock().micros
    }
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        let mut inner = self.inner.lock();
        if !inner.in_queue {
            inner.in_queue = true;
            drop(inner);
            EXECUTOR.lock().ready_tasks.push_back(self);
        }
    }
}

static EXECUTOR: Mutex<Executor> = Mutex::new(Executor {
    ready_tasks: VecDeque::new(),
});

pub struct Executor {
    // TODO(veluca): scheduling.
    ready_tasks: VecDeque<Arc<Task>>,
}

impl Executor {
    pub fn run(os: UefiOS) -> ! {
        loop {
            let task = EXECUTOR
                .lock()
                .ready_tasks
                .pop_front()
                .expect("Executor should never run out of ready tasks");

            task.inner.lock().in_queue = false;
            let waker = Waker::from(task.clone());
            let mut context = Context::from_waker(&waker);
            let mut fut = task.inner.lock().future.take().unwrap();
            let begin = os.timer().micros();
            let status = fut.as_mut().poll(&mut context);
            let end = os.timer().micros();
            task.inner.lock().micros += end - begin;
            if status.is_pending() {
                task.inner.lock().future = Some(fut);
            }
        }
    }

    pub(super) fn spawn(task: Arc<Task>) {
        EXECUTOR.lock().ready_tasks.push_back(task);
    }
}
