use log::info;
use pixie_shared::{Address, Station};

use crate::os::{error::Result, HttpMethod, UefiOS, PACKET_SIZE};

pub async fn register(os: UefiOS, hint_port: u16, server_address: Address) -> Result<()> {
    let udp = os.udp_bind(Some(hint_port)).await?;
    let mut buf = [0; PACKET_SIZE];
    let (buf, _) = udp.recv(&mut buf).await;
    let hint: Station = serde_json::from_slice(buf)?;

    // TODO(veluca): actual registration.

    os.http(
        server_address.ip,
        server_address.port,
        HttpMethod::Post(&serde_json::to_vec(&hint)?),
        b"/register",
    )
    .await?;

    info!("Registration successful! {:?}", hint);

    Ok(())
}
