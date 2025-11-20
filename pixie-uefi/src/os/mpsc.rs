use alloc::{collections::VecDeque, rc::Rc, sync::Arc};
use core::{cell::RefCell, future::poll_fn, task::Poll, task::Waker};
use spin::Mutex;

struct Data<T> {
    size: usize,
    tx_count: usize,
    tx_waker: Option<Waker>,
    rx_waker: Option<Waker>,
    queue: VecDeque<T>,
}

pub struct Sender<T> {
    inner: Arc<Mutex<Data<T>>>,
}

impl<T> Sender<T> {
    pub async fn send(&mut self, value: T) {
        let mut value = Some(value);
        poll_fn(|cx| {
            let mut inner = self.inner.lock();
            if inner.queue.len() < inner.size {
                inner.queue.push_back(value.take().unwrap());
                if let Some(waker) = inner.rx_waker.take() {
                    waker.wake();
                }
                Poll::Ready(())
            } else {
                inner.tx_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        inner.tx_count -= 1;
        if let Some(waker) = inner.rx_waker.take() {
            waker.wake();
        }
    }
}

pub struct Receiver<T> {
    inner: Arc<Mutex<Data<T>>>,
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| {
            let mut inner = self.inner.lock();
            if let Some(value) = inner.queue.pop_front() {
                if let Some(waker) = inner.tx_waker.take() {
                    waker.wake();
                }
                Poll::Ready(Some(value))
            } else if inner.tx_count == 0 {
                Poll::Ready(None)
            } else {
                inner.rx_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

pub fn channel<T>(size: usize) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Mutex::new(Data {
        size,
        tx_count: 1,
        tx_waker: None,
        rx_waker: None,
        queue: VecDeque::new(),
    }));
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}
