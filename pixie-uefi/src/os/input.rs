use spin::lazy::Lazy;
use spin::Mutex;
use uefi::boot::ScopedProtocol;
use uefi::proto::console::text::{Input, Key};

use crate::os::error::Result;
use crate::os::executor::Executor;
use crate::os::send_wrapper::SendWrapper;

static INPUT: Lazy<Mutex<SendWrapper<ScopedProtocol<Input>>>> = Lazy::new(|| {
    let input_handles = uefi::boot::find_handles::<Input>().unwrap();
    let input = uefi::boot::open_protocol_exclusive::<Input>(input_handles[0]).unwrap();
    Mutex::new(SendWrapper(input))
});

pub async fn read_key() -> Result<Key> {
    loop {
        if let Some(key) = INPUT.lock().read_key()? {
            break Ok(key);
        }
        Executor::wait_for_interrupt().await;
    }
}
