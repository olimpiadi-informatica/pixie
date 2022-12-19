use log::info;
use pixie_shared::{Address, Station};

use crate::os::{error::Result, HttpMethod, UefiOS};

pub async fn register(os: UefiOS, hint_port: u16, server_address: Address) -> Result<!> {
    let udp = os.udp_bind(Some(hint_port)).await?;
    let mut hint: Station = Default::default();
    udp.recv(|data, _, _| -> Result<()> {
        hint = serde_json::from_slice(data)?;
        Ok(())
    })
    .await?;

    // TODO(veluca): actual registration.

    os.http(
        server_address.ip,
        server_address.port,
        HttpMethod::Post(&serde_json::to_vec(&hint)?),
        b"/register",
    )
    .await?;

    info!("Registration successful! {:?}", hint);
    os.sleep_us(10_000_000).await;
    os.reset();
}
