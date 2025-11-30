use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::net::SocketAddrV4;

use futures::future::{select, Either};
use pixie_shared::{HintPacket, RegistrationInfo, TcpRequest, HINT_PORT};
use uefi::proto::console::text::{Color, Key, ScanCode};

use crate::os::error::{Error, Result};
use crate::os::{UefiOS, PACKET_SIZE};

#[derive(Debug, Default)]
struct Data {
    station: RegistrationInfo,
    selected: usize,
}

pub async fn register(os: UefiOS, server_addr: SocketAddrV4) -> Result<()> {
    let data = Rc::new(RefCell::new(Data::default()));
    let data2 = data.clone();

    os.set_ui_drawer(move |os| {
        let data2 = data2.borrow();
        os.write_with_color(
            &format!("Group:  {}\n", data2.station.group),
            if data2.selected == 0 {
                Color::Yellow
            } else {
                Color::White
            },
            Color::Black,
        );
        os.write_with_color(
            &format!("Row:    {}\n", data2.station.row),
            if data2.selected == 1 {
                Color::Yellow
            } else {
                Color::White
            },
            Color::Black,
        );
        os.write_with_color(
            &format!("Column: {}\n", data2.station.col),
            if data2.selected == 2 {
                Color::Yellow
            } else {
                Color::White
            },
            Color::Black,
        );
        os.write_with_color(
            &format!("Image:  {}\n", data2.station.image),
            if data2.selected == 3 {
                Color::Yellow
            } else {
                Color::White
            },
            Color::Black,
        );
    });

    let udp = os.udp_bind(Some(HINT_PORT)).await?;
    let mut buf = [0; PACKET_SIZE];

    let mut hint = true;
    let mut images = Vec::new();
    let mut groups = Vec::new();

    loop {
        let key = if hint {
            loop {
                let recv = Box::pin(udp.recv(&mut buf));
                let key = Box::pin(os.read_key());
                match select(recv, key).await {
                    Either::Left(((buf, _), _)) => {
                        let hint: HintPacket = postcard::from_bytes(buf)?;
                        data.borrow_mut().station = hint.station;
                        images = hint.images;
                        groups = hint.groups.into_iter().map(|(k, _)| k).collect();
                        os.force_ui_redraw();
                    }
                    Either::Right((key, _)) => {
                        hint = false;
                        break key?;
                    }
                }
            }
        } else {
            os.read_key().await?
        };

        if key == Key::Special(ScanCode::DOWN) {
            let mut data = data.borrow_mut();
            data.selected = (data.selected + 1).min(3);
        }
        if key == Key::Special(ScanCode::UP) {
            let mut data = data.borrow_mut();
            data.selected = data.selected.saturating_sub(1);
        }
        if key == Key::Special(ScanCode::LEFT) {
            let mut data = data.borrow_mut();
            match data.selected {
                0 => {
                    data.station.group = groups
                        .iter()
                        .rev()
                        .cycle()
                        .skip_while(|g| **g != data.station.group)
                        .nth(1)
                        .unwrap()
                        .clone()
                }
                1 => data.station.row = data.station.row.wrapping_sub(1),
                2 => data.station.col = data.station.col.wrapping_sub(1),
                3 => {
                    data.station.image = images[(images
                        .iter()
                        .position(|x| x == &data.station.image)
                        .ok_or(Error::msg("Invalid image name"))?
                        + images.len()
                        - 1)
                        % images.len()]
                    .clone();
                }
                _ => unreachable!(),
            }
        }
        if key == Key::Special(ScanCode::RIGHT) {
            let mut data = data.borrow_mut();
            match data.selected {
                0 => {
                    data.station.group = groups
                        .iter()
                        .cycle()
                        .skip_while(|g| **g != data.station.group)
                        .nth(1)
                        .unwrap()
                        .clone()
                }
                1 => data.station.row = data.station.row.wrapping_add(1),
                2 => data.station.col = data.station.col.wrapping_add(1),
                3 => {
                    data.station.image = images[(images
                        .iter()
                        .position(|x| x == &data.station.image)
                        .ok_or(Error::msg("Invalid image name"))?
                        + 1)
                        % images.len()]
                    .clone();
                }
                _ => unreachable!(),
            }
        }
        if key == Key::Printable('\r'.try_into().unwrap()) {
            break;
        }
        os.force_ui_redraw();
    }

    let msg = TcpRequest::Register(data.borrow().station.clone());
    let buf = postcard::to_allocvec(&msg)?;
    let stream = os.connect(server_addr).await?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    assert_eq!(len, 0);
    stream.close_send().await;
    // TODO(virv): this could be better
    stream.force_close().await;

    log::info!("Registration successful! {:?}", data.borrow().station);

    Ok(())
}
