use core::{cell::RefCell, pin::Pin};

use alloc::{boxed::Box, sync::Arc};
use futures::{task::ArcWake, Future};

use super::UefiOS;

pub(super) type BoxFuture<T = ()> = Pin<Box<dyn Future<Output = T> + 'static>>;

pub(super) struct Task {
    pub task: RefCell<Option<BoxFuture>>,
    pub os: UefiOS,
}

// SAFETY: we never create threads anyway.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl ArcWake for Task {
    fn wake_by_ref(task: &Arc<Self>) {
        task.os.os().borrow_mut().tasks.push_back(task.clone());
    }
}

pub struct RepeatFn<F: Fn() + 'static + Unpin> {
    f: F,
}

impl<F: Fn() + 'static + Unpin> RepeatFn<F> {
    pub fn new(f: F) -> RepeatFn<F> {
        RepeatFn { f }
    }
}

impl<F: Fn() + 'static + Unpin> Future for RepeatFn<F> {
    type Output = ();
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        (self.get_mut().f)();
        cx.waker().wake_by_ref();
        core::task::Poll::Pending
    }
}
