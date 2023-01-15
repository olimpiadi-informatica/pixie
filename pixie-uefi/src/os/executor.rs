use core::{cell::RefCell, pin::Pin, sync::atomic::AtomicBool, task::Context};

use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use futures::{
    task::{waker_ref, ArcWake},
    Future,
};

pub(super) type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static>>;

pub struct Executor {
    // TODO(veluca): scheduling.
    tasks: VecDeque<Arc<Task>>,
}

static mut EXECUTOR: Option<RefCell<Executor>> = None;
static EXECUTOR_CONSTRUCTED: AtomicBool = AtomicBool::new(false);

fn executor() -> &'static RefCell<Executor> {
    assert!(EXECUTOR_CONSTRUCTED.load(core::sync::atomic::Ordering::Relaxed));
    // SAFETY: guarded by EXECUTOR_CONSTRUCTED. There are no threads, so no problems with
    // concurrent access.
    unsafe { EXECUTOR.as_ref().unwrap() }
}

pub(super) struct Task {
    pub task: RefCell<Option<BoxFuture>>,
}

// SAFETY: we never create threads anyway.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl ArcWake for Task {
    fn wake_by_ref(task: &Arc<Self>) {
        executor().borrow_mut().tasks.push_back(task.clone());
    }
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
    pub fn run() -> ! {
        loop {
            let task = executor()
                .borrow_mut()
                .tasks
                .pop_front()
                .expect("Executor should never run out of tasks");

            let waker = waker_ref(&task);
            let context = &mut Context::from_waker(&waker);
            let mut task_inner = task.task.borrow_mut();
            if let Some(mut fut) = task_inner.take() {
                if fut.as_mut().poll(context).is_pending() {
                    *task_inner = Some(fut);
                }
            }
        }
    }

    pub fn spawn<Fut>(f: Fut)
    where
        Fut: Future<Output = ()> + 'static,
    {
        executor().borrow_mut().tasks.push_back(Arc::new(Task {
            task: RefCell::new(Some(Box::pin(f))),
        }));
    }
}
