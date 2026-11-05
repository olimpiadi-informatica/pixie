use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::net::SocketAddrV4;

use futures::future::{Either, select};
use pixie_shared::{HINT_PORT, HintPacket, RegistrationInfo, RpcRequest, UnitStats};
use uefi::proto::console::text::{Color, Key, ScanCode};

use crate::os::error::Result;
use crate::os::net::{ETH_PACKET_SIZE, UdpSocket};
use crate::os::ui::DrawArea;
use crate::os::{input, ui};

#[derive(Debug, Default)]
struct Data {
    station: RegistrationInfo,
    selected: usize,
}

pub async fn register(server_addr: SocketAddrV4) -> Result<()> {
    let data = Rc::new(RefCell::new(Data::default()));
    let data2 = data.clone();

    let draw_content = move |draw_area: &mut DrawArea| {
        draw_area.clear();
        let data2 = data2.borrow();
        let fg = |i| {
            if data2.selected == i {
                Color::Yellow
            } else {
                Color::White
            }
        };
        draw_area.write_with_color(
            &format!("Group:  {}\n", data2.station.group),
            fg(0),
            Color::Black,
        );
        draw_area.write_with_color(
            &format!("Row:    {}\n", data2.station.row),
            fg(1),
            Color::Black,
        );
        draw_area.write_with_color(
            &format!("Column: {}\n", data2.station.col),
            fg(2),
            Color::Black,
        );
        draw_area.write_with_color(
            &format!("Image:  {}\n", data2.station.image),
            fg(3),
            Color::Black,
        );
    };

    let udp = UdpSocket::bind(Some(HINT_PORT)).await?;
    let mut buf = [0; ETH_PACKET_SIZE];

    let mut has_hint = false;
    let mut hint = true;
    let mut images = Vec::new();
    let mut groups = Vec::new();

    ui::update_content(&draw_content);
    ui::flush();

    loop {
        let key = if hint {
            loop {
                let recv = Box::pin(udp.recv_from(&mut buf));
                let key = Box::pin(input::read_key());
                match select(recv, key).await {
                    Either::Left(((buf, _), _)) => match postcard::from_bytes::<HintPacket>(buf) {
                        Ok(pkt) => {
                            let new_groups: Vec<_> =
                                pkt.groups.into_iter().map(|(k, _)| k).collect();
                            if new_groups.is_empty() || pkt.images.is_empty() {
                                log::warn!("Ignoring HintPacket with empty groups or images");
                                continue;
                            }
                            images = pkt.images;
                            groups = new_groups;
                            let mut d = data.borrow_mut();
                            d.station = pkt.station;
                            if !groups.contains(&d.station.group) {
                                d.station.group = groups[0].clone();
                            }
                            if !images.contains(&d.station.image) {
                                d.station.image = images[0].clone();
                            }
                            drop(d);
                            has_hint = true;
                            ui::update_content(&draw_content);
                            ui::flush();
                        }
                        Err(e) => {
                            log::warn!("Ignoring invalid HintPacket: {e}");
                        }
                    },
                    Either::Right((key, _)) => {
                        let key = key?;
                        if !has_hint {
                            continue;
                        }
                        hint = false;
                        break key;
                    }
                }
            }
        } else {
            input::read_key().await?
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
                    if !groups.is_empty() {
                        let idx = groups
                            .iter()
                            .position(|g| g == &data.station.group)
                            .unwrap_or(0);
                        data.station.group =
                            groups[(idx + groups.len() - 1) % groups.len()].clone();
                    }
                }
                1 => data.station.row = data.station.row.wrapping_sub(1),
                2 => data.station.col = data.station.col.wrapping_sub(1),
                3 => {
                    if !images.is_empty() {
                        let idx = images
                            .iter()
                            .position(|x| x == &data.station.image)
                            .unwrap_or(0);
                        data.station.image =
                            images[(idx + images.len() - 1) % images.len()].clone();
                    }
                }
                _ => unreachable!(),
            }
        }
        if key == Key::Special(ScanCode::RIGHT) {
            let mut data = data.borrow_mut();
            match data.selected {
                0 => {
                    if !groups.is_empty() {
                        let idx = groups
                            .iter()
                            .position(|g| g == &data.station.group)
                            .unwrap_or(0);
                        data.station.group = groups[(idx + 1) % groups.len()].clone();
                    }
                }
                1 => data.station.row = data.station.row.wrapping_add(1),
                2 => data.station.col = data.station.col.wrapping_add(1),
                3 => {
                    if !images.is_empty() {
                        let idx = images
                            .iter()
                            .position(|x| x == &data.station.image)
                            .unwrap_or(0);
                        data.station.image = images[(idx + 1) % images.len()].clone();
                    }
                }
                _ => unreachable!(),
            }
        }
        if key == Key::Printable('\r'.try_into().unwrap()) {
            break;
        }
        ui::update_content(&draw_content);
        ui::flush();
    }

    let mem_stats = crate::os::memory::stats();
    let stat = UnitStats {
        ram: mem_stats.used + mem_stats.free + mem_stats.other,
        disk: crate::os::disk::Disk::largest().await.size(),
        cpu: crate::os::util::get_cpu_model(),
    };

    drop(udp);
    let station = data.borrow().station.clone();
    let udp = UdpSocket::bind(None).await?;
    udp.rpc::<()>(server_addr, RpcRequest::Register(station, stat))
        .await?;

    log::info!("Registration successful! {}", data.borrow().station);

    Ok(())
}
