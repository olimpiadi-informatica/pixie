
use log::info;
use pixie_shared::{Address, Station};

use crate::os::{error::Result, HttpMethod, UefiOS};

pub async fn register(os: UefiOS, server_address: Address) -> Result<!> {
    let resp = os
        .http(
            server_address.ip,
            server_address.port,
            HttpMethod::Get,
            b"/register_hint",
        )
        .await?;
    let mut hint: Station = serde_json::from_slice(&resp)?;

    // TODO(veluca): actual registration.
    // Whatever...
    hint.row += 1;

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
