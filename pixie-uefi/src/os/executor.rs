use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::fmt::Write;
use core::future::{poll_fn, Future};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use spin::Mutex;
use uefi::boot::{EventType, TimerTrigger, Tpl};
use uefi::proto::console::text::Color;

use crate::os::send_wrapper::SendWrapper;
use crate::os::timer::Timer;
use crate::os::ui::DrawArea;

type BoxFuture = SendWrapper<Pin<Box<dyn Future<Output = ()> + 'static>>>;

struct Task {
    name: &'static str,
    in_queue: AtomicBool,
    future: Mutex<BoxFuture>,
    micros: AtomicU64,
    last_micros: AtomicU64,
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

pub(super) const TASK_LEN: usize = 34;

impl Executor {
    async fn draw_tasks() {
        let mut draw_area = DrawArea::tasks();
        let (w, h) = draw_area.size();
        assert!((w - 1).is_multiple_of(TASK_LEN + 1));
        let num_w = (w - 1) / (TASK_LEN + 1);
        let mut last = Timer::micros() as u64;
        Self::sleep_us(100_000).await;
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
            }
            last = Timer::micros() as u64;
            Self::sleep_us(1_000_000).await;
        }
    }

    pub fn run() -> ! {
        Self::spawn("[show_tasks]", Self::draw_tasks());
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
}
