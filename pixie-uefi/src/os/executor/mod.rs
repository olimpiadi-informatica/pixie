use alloc::boxed::Box;
use alloc::collections::binary_heap::BinaryHeap;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::fmt::Write;
use core::future::{poll_fn, Future};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use futures::channel::oneshot;
use spin::Mutex;
use uefi::proto::console::text::Color;

use crate::os::executor::event::{Event, EventTrigger};
use crate::os::send_wrapper::SendWrapper;
use crate::os::timer::Timer;
use crate::os::ui::DrawArea;

pub mod event;

type BoxFuture = SendWrapper<Pin<Box<dyn Future<Output = ()> + 'static>>>;

struct Task {
    name: &'static str,
    in_queue: AtomicBool,
    future: Mutex<BoxFuture>,
    micros: AtomicU64,
    last_micros: AtomicU64,
    done: AtomicBool,
}

impl Task {
    pub(super) fn new<Fut>(name: &'static str, future: Fut) -> Arc<Task>
    where
        Fut: Future<Output = ()> + 'static,
    {
        Arc::new(Task {
            name,
            future: Mutex::new(SendWrapper(Box::pin(future))),
            micros: AtomicU64::new(0),
            last_micros: AtomicU64::new(0),
            in_queue: AtomicBool::new(false),
            done: AtomicBool::new(false),
        })
    }
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        if !self.in_queue.swap(true, Ordering::Relaxed) && !self.done.load(Ordering::Relaxed) {
            EXECUTOR.lock().ready_tasks.push_back(self);
        }
    }
}

struct TimedWait {
    wake_at: i64,
    event: EventTrigger,
}

impl PartialEq for TimedWait {
    fn eq(&self, other: &Self) -> bool {
        self.wake_at == other.wake_at
    }
}

impl Eq for TimedWait {}

impl PartialOrd for TimedWait {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimedWait {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Reversed order: min-heap.
        other.wake_at.cmp(&self.wake_at)
    }
}

pub struct JoinHandle<T>(oneshot::Receiver<T>);

impl<T> JoinHandle<T> {
    pub async fn join(self) -> T {
        self.0.await.expect("tasks should never be cancelled")
    }
}

static EXECUTOR: Mutex<Executor> = Mutex::new(Executor {
    wake_on_interrupt: vec![],
    timed_wait: BinaryHeap::new(),
    ready_tasks: VecDeque::new(),
    tasks: vec![],
});

pub struct Executor {
    wake_on_interrupt: Vec<EventTrigger>,
    timed_wait: BinaryHeap<TimedWait>,
    ready_tasks: VecDeque<Arc<Task>>,
    tasks: Vec<Arc<Task>>,
}

pub(super) const TASK_LEN: usize = 34;

impl Executor {
    async fn draw_tasks() {
        let mut draw_area = DrawArea::tasks();
        let (w, h) = draw_area.size();
        assert!((w - 1).is_multiple_of(TASK_LEN + 1));
        let num_w = (w - 1) / (TASK_LEN + 1);
        let mut last = Timer::micros() as u64;
        Self::sleep(Duration::from_millis(100)).await;
        loop {
            draw_area.clear();
            let cur = Timer::micros() as u64;
            let elapsed = cur.saturating_sub(last).max(1) as f64;
            {
                let mut executor = EXECUTOR.lock();
                let tasks = &mut executor.tasks;
                // Sort by *descending* time used since last draw.
                tasks.sort_unstable_by_key(|f| {
                    f.last_micros.load(Ordering::Relaxed) as i64
                        - f.micros.load(Ordering::Relaxed) as i64
                });

                write!(draw_area, "\u{250C}").unwrap();
                for x in 0..num_w {
                    for _ in 0..TASK_LEN {
                        write!(draw_area, "\u{2500}").unwrap();
                    }
                    if x + 1 == num_w {
                        write!(draw_area, "\u{2510}").unwrap();
                    } else {
                        write!(draw_area, "\u{252C}").unwrap();
                    }
                }
                draw_area.newline();
                for y in 0..(h - 2) {
                    write!(draw_area, "\u{2502}").unwrap();
                    for x in 0..num_w {
                        let idx = x * h + y;
                        if idx >= tasks.len() {
                            draw_area.advance(TASK_LEN);
                        } else {
                            let task = &tasks[idx];
                            let total_cpu = task.micros.load(Ordering::Relaxed);
                            let last_cpu = task.last_micros.load(Ordering::Relaxed);
                            let frac = ((total_cpu - last_cpu) as f64 / elapsed).min(1.0);
                            draw_area.write_with_color(
                                &format!(
                                    " {:15}{:5.1}%{:10.3}s ",
                                    &task.name[..task.name.len().min(15)],
                                    frac * 100.0,
                                    total_cpu as f64 * 0.000_001,
                                ),
                                if frac >= 0.5 {
                                    Color::Red
                                } else if frac >= 0.1 {
                                    Color::Yellow
                                } else {
                                    Color::White
                                },
                                Color::Black,
                            );
                        }
                        write!(draw_area, "\u{2502}").unwrap();
                    }
                    draw_area.newline();
                }
                write!(draw_area, "\u{2514}").unwrap();
                for x in 0..num_w {
                    for _ in 0..TASK_LEN {
                        write!(draw_area, "\u{2500}").unwrap();
                    }
                    if x + 1 == num_w {
                        write!(draw_area, "\u{2518}").unwrap();
                    } else {
                        write!(draw_area, "\u{2534}").unwrap();
                    }
                }
                draw_area.newline();

                for t in tasks.iter() {
                    t.last_micros
                        .store(t.micros.load(Ordering::Relaxed), Ordering::Relaxed);
                }

                // Clear completed tasks.
                tasks.retain(|t| !t.done.load(Ordering::Relaxed));
            }
            last = Timer::micros() as u64;
            Self::sleep(Duration::from_secs(1)).await
        }
    }

    pub fn run() -> ! {
        Self::spawn("[show_tasks]", Self::draw_tasks());

        // Maximum amount of microseconds between wakeups of interrupt-based wakers.
        const INTERRUPT_MICROS: i64 = 500;

        let mut last_interrupt_wakeup = Timer::micros();

        let mut do_wake = |force_interrupt_wake| {
            // Wake timed-waiting tasks.
            loop {
                let event = {
                    let mut ex = EXECUTOR.lock();
                    let Some(w) = ex.timed_wait.peek() else {
                        break;
                    };
                    if w.wake_at > Timer::micros() {
                        break;
                    }
                    let w = ex.timed_wait.pop().unwrap();
                    w.event
                };
                event.trigger();
            }
            // Since we don't notice interrupts that happened while we are not hlt-ing,
            // make sure that we wake up all the interrupt-based waiting tasks every at
            // most INTERRUPT_MICROS micros to make it unlikely to miss interrupts.
            if last_interrupt_wakeup + INTERRUPT_MICROS <= Timer::micros() || force_interrupt_wake {
                last_interrupt_wakeup = Timer::micros();
                let to_wake = core::mem::take(&mut EXECUTOR.lock().wake_on_interrupt);
                for e in to_wake {
                    e.trigger();
                }
            }
        };

        loop {
            do_wake(false);
            let task = EXECUTOR.lock().ready_tasks.pop_front();
            let Some(task) = task else {
                // If we don't have anything ready, sleep until the next interrupt.
                // SAFETY: hlt is available on all reasonable x86 processors and has no safety
                // requirements.
                unsafe {
                    core::arch::asm!("hlt");
                }
                do_wake(true);
                continue;
            };

            // It is possible for a done task to end up in the queue (if it wakes
            // itself during execution). If that happens, we just remove it from
            // the queue here.
            if task.done.load(Ordering::Relaxed) {
                continue;
            }
            task.in_queue.store(false, Ordering::Relaxed);
            let waker = Waker::from(task.clone());
            let mut context = Context::from_waker(&waker);
            let mut fut = task.future.try_lock().unwrap();
            let begin = Timer::micros();
            let done = fut.0.as_mut().poll(&mut context);
            let end = Timer::micros();
            task.micros
                .fetch_add((end - begin) as u64, Ordering::Relaxed);
            if done.is_ready() {
                task.done.swap(true, Ordering::Relaxed);
            }
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

    // Wakes a task as soon as *any* interrupt is received.
    pub fn wait_for_interrupt() -> impl Future<Output = ()> {
        let event = Event::new();
        EXECUTOR.lock().wake_on_interrupt.push(event.trigger());
        event
    }

    // Note: there are no guarantees on whether the amount of time we will sleep for
    // will be exceeded.
    pub fn sleep(time: Duration) -> impl Future<Output = ()> {
        let tgt = Timer::micros() + time.as_micros() as i64;
        let event = Event::new();
        EXECUTOR.lock().timed_wait.push(TimedWait {
            wake_at: tgt,
            event: event.trigger(),
        });
        event
    }

    /// Spawn a new task.
    pub fn spawn<Fut, T: 'static>(name: &'static str, f: Fut) -> JoinHandle<T>
    where
        Fut: Future<Output = T> + 'static,
    {
        let (send, recv) = oneshot::channel();
        let task = Task::new(name, async move {
            let t = f.await;
            let _ = send.send(t);
        });
        let mut executor = EXECUTOR.lock();
        executor.tasks.push(task.clone());
        executor.ready_tasks.push_back(task);
        JoinHandle(recv)
    }
}
