use pixie_shared::{Address, Station};
use uefi::proto::console::text::Color;

use crate::os::{error::Result, HttpMethod, MessageKind, UefiOS, PACKET_SIZE};

pub async fn register(os: UefiOS, hint_port: u16, server_address: Address) -> Result<()> {
    let udp = os.udp_bind(Some(hint_port)).await?;
    let mut buf = [0; PACKET_SIZE];
    let (buf, _) = udp.recv(&mut buf).await;
    let hint: Station = serde_json::from_slice(buf)?;

    // TODO(veluca): actual registration.

    let hint2 = hint.clone();
    os.set_ui_drawer(move |os| {
        os.write_with_color(
            &format!("Press a key to accept the hint: {:?}", hint2),
            Color::White,
            Color::Black,
        );
    });

    os.append_message(format!("{:?}", os.read_key().await), MessageKind::Debug);

    os.http(
        server_address.ip,
        server_address.port,
        HttpMethod::Post(&serde_json::to_vec(&hint)?),
        b"/register",
    )
    .await?;

    os.append_message(
        format!("Registration successful! {:?}", hint),
        MessageKind::Info,
    );

    Ok(())
}
