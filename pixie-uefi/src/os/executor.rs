use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::future::{poll_fn, Future};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};
use spin::Mutex;
use uefi::boot::{EventType, TimerTrigger, Tpl};

use crate::os::timer::Timer;

struct BoxFuture(Pin<Box<dyn Future<Output = ()> + 'static>>);

// SAFETY: there are no threads.
unsafe impl Send for BoxFuture {}

struct Task {
    name: &'static str,
    in_queue: AtomicBool,
    future: Mutex<BoxFuture>,
    micros: AtomicU64,
}

impl Task {
    pub(super) fn new<Fut>(name: &'static str, future: Fut) -> Arc<Task>
    where
        Fut: Future<Output = ()> + 'static,
    {
        Arc::new(Task {
            name,
            future: Mutex::new(BoxFuture(Box::pin(future))),
            micros: AtomicU64::new(0),
            in_queue: AtomicBool::new(false),
        })
    }
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        if !self.in_queue.swap(true, Ordering::Relaxed) {
            EXECUTOR.lock().ready_tasks.push_back(self);
        }
    }
}

static EXECUTOR: Mutex<Executor> = Mutex::new(Executor {
    ready_tasks: VecDeque::new(),
    tasks: vec![],
});

pub struct Executor {
    // TODO(veluca): scheduling.
    ready_tasks: VecDeque<Arc<Task>>,
    tasks: Vec<Arc<Task>>,
}

impl Executor {
    pub fn run() -> ! {
        loop {
            let task = EXECUTOR
                .lock()
                .ready_tasks
                .pop_front()
                .expect("Executor should never run out of ready tasks");

            task.in_queue.store(false, Ordering::Relaxed);
            let waker = Waker::from(task.clone());
            let mut context = Context::from_waker(&waker);
            let mut fut = task.future.try_lock().unwrap();
            let begin = Timer::micros();
            let _ = fut.0.as_mut().poll(&mut context);
            let end = Timer::micros();
            task.micros
                .fetch_add((end - begin) as u64, Ordering::Relaxed);
        }
    }

    /// Interrupt task execution.
    /// This is useful to yield the CPU to other tasks.
    pub fn sched_yield() -> impl Future<Output = ()> {
        let mut ready = false;
        poll_fn(move |cx| {
            if ready {
                Poll::Ready(())
            } else {
                ready = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    pub fn sleep_us(us: u64) -> impl Future<Output = ()> {
        let tgt = Timer::micros() as u64 + us;
        poll_fn(move |cx| {
            let now = Timer::micros() as u64;
            if now >= tgt {
                Poll::Ready(())
            } else {
                // TODO(veluca): actually suspend the task.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    /// **WARNING**: this function halts all tasks
    pub fn deep_sleep_us(us: u64) {
        // SAFETY: we are not using a callback
        let e =
            unsafe { uefi::boot::create_event(EventType::TIMER, Tpl::NOTIFY, None, None).unwrap() };
        uefi::boot::set_timer(&e, TimerTrigger::Relative(10 * us)).unwrap();
        uefi::boot::wait_for_event(&mut [e]).unwrap();
    }

    /// Spawn a new task.
    pub fn spawn<Fut>(name: &'static str, f: Fut)
    where
        Fut: Future<Output = ()> + 'static,
    {
        let task = Task::new(name, f);
        let mut executor = EXECUTOR.lock();
        executor.tasks.push(task.clone());
        executor.ready_tasks.push_back(task);
    }

    pub fn top_tasks(n: usize) -> Vec<(u64, &'static str)> {
        let mut tasks: Vec<_> = EXECUTOR
            .lock()
            .tasks
            .iter()
            .map(|x| (x.micros.load(Ordering::Relaxed), x.name))
            .collect();
        tasks.sort_by_key(|t| -(t.0 as i64));
        tasks.truncate(n);
        tasks
    }
}
