use core::future::{poll_fn, Future};
use core::task::Poll;

use spin::lazy::Lazy;
use spin::Mutex;
use uefi::boot::ScopedProtocol;
use uefi::proto::console::text::{Input, Key};

use crate::os::error::Result;
use crate::os::send_wrapper::SendWrapper;

static INPUT: Lazy<Mutex<SendWrapper<ScopedProtocol<Input>>>> = Lazy::new(|| {
    let input_handles = uefi::boot::find_handles::<Input>().unwrap();
    let input = uefi::boot::open_protocol_exclusive::<Input>(input_handles[0]).unwrap();
    Mutex::new(SendWrapper(input))
});

pub fn read_key() -> impl Future<Output = Result<Key>> {
    poll_fn(move |cx| {
        let key = INPUT.lock().read_key();
        match key {
            Err(e) => Poll::Ready(Err(e.into())),
            Ok(Some(key)) => Poll::Ready(Ok(key)),
            Ok(None) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    })
}
