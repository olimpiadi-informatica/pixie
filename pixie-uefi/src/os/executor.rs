use core::{cell::RefCell, pin::Pin, sync::atomic::AtomicBool, task::Context};

use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use futures::{
    task::{waker_ref, ArcWake},
    Future,
};

use super::UefiOS;

pub(super) type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static>>;

static mut EXECUTOR: Option<RefCell<Executor>> = None;
static EXECUTOR_CONSTRUCTED: AtomicBool = AtomicBool::new(false);

fn executor() -> &'static RefCell<Executor> {
    assert!(EXECUTOR_CONSTRUCTED.load(core::sync::atomic::Ordering::Relaxed));
    // SAFETY: guarded by EXECUTOR_CONSTRUCTED. There are no threads, so no problems with
    // concurrent access.
    unsafe { EXECUTOR.as_ref().unwrap() }
}

struct TaskInner {
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
        executor().borrow_mut().tasks.push_back(task.clone());
    }
}

pub struct Executor {
    // TODO(veluca): scheduling.
    tasks: VecDeque<Arc<Task>>,
}

impl Executor {
    pub fn init() {
        assert!(!EXECUTOR_CONSTRUCTED.load(core::sync::atomic::Ordering::Relaxed));
        // SAFETY: guarded by EXECUTOR_CONSTRUCTED. There are no threads, so no problems with
        // concurrent access.
        unsafe {
            EXECUTOR = Some(RefCell::new(Executor {
                tasks: VecDeque::new(),
            }));
            EXECUTOR_CONSTRUCTED.store(true, core::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn run(os: UefiOS) -> ! {
        loop {
            let task = executor()
                .borrow_mut()
                .tasks
                .pop_front()
                .expect("Executor should never run out of tasks");

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
        executor().borrow_mut().tasks.push_back(task);
    }
}
