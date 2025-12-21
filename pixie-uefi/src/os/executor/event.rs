use alloc::sync::{Arc, Weak};
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};

use futures::task::AtomicWaker;

#[derive(Debug, Default)]
struct Inner {
    triggered: AtomicBool,
    waker: AtomicWaker,
}

pub struct Event {
    inner: Arc<Inner>,
}

impl Event {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    pub fn trigger(&self) -> EventTrigger {
        EventTrigger {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

impl Future for Event {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.inner.triggered.load(Ordering::Relaxed) {
            Poll::Ready(())
        } else {
            self.inner.waker.register(cx.waker());
            Poll::Pending
        }
    }
}

pub struct EventTrigger {
    inner: Weak<Inner>,
}

impl EventTrigger {
    pub fn trigger(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.triggered.store(true, Ordering::Relaxed);
            inner.waker.wake();
        }
    }
}
