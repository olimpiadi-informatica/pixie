use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;
use std::ops::Deref;

use futures::StreamExt;
use gloo_timers::future::TimeoutFuture;
use pixie_shared::{Config, ImageStat, Unit, WsUpdate};
use reqwasm::websocket::futures::WebSocket;
use reqwasm::websocket::Message;
use sycamore::futures::{spawn_local, spawn_local_scoped};
use sycamore::prelude::*;
use wasm_bindgen::prelude::*;

use reqwasm::http::Request;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = Date, js_name = now)]
    fn date_now() -> f64;

    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

fn send_req(url: String) {
    spawn_local(async move {
        Request::get(&url)
            .send()
            .await
            .unwrap_or_else(|_| panic!("Request to {} failed", url));
    });
}

#[component(inline_props)]
fn UnitInfo<G: Html>(cx: Scope<'_>, unit: Unit, hostname: Option<String>) -> View<G> {
    let time = create_signal(cx, date_now());

    spawn_local_scoped(cx, async move {
        loop {
            TimeoutFuture::new(100).await;
            time.set(date_now());
        }
    });

    let ping_time = unit.last_ping_timestamp as i64;
    let ping_ago = move || {
        let now = (*time.get() * 0.001) as i64;
        now - ping_time
    };

    let fmt_ca = move |unit: &Unit| {
        if let Some(a) = unit.curr_action {
            if let Some((x, y)) = unit.curr_progress {
                format!("{} ({}/{})", a, x, y)
            } else {
                a.to_string()
            }
        } else {
            format!(
                "ping: {} seconds ago, {}",
                ping_ago(),
                String::from_utf8_lossy(&unit.last_ping_msg)
            )
        }
    };

    let led_class = move || match ping_ago() {
        ..=-1 => "led-blue",
        0..=119 => "led-green",
        120..=299 => "led-yellow",
        300.. => "led-red",
    };

    let mac = unit.mac.to_string();
    let mac_nocolon = mac.replace(':', "");

    let id_pull = format!("machine-{mac_nocolon}-pull");
    let url_pull = format!("/admin/action/{mac}/pull");
    let id_push = format!("machine-{mac_nocolon}-push");
    let url_push = format!("/admin/action/{mac}/push");
    let id_boot = format!("machine-{mac_nocolon}-boot");
    let url_boot = format!("/admin/action/{mac}/reboot");
    let id_cancel = format!("machine-{mac_nocolon}-cancel");
    let url_cancel = format!("/admin/action/{mac}/wait");
    let id_register = format!("machine-{mac_nocolon}-register");
    let url_register = format!("/admin/action/{mac}/register");

    let image = unit.image.clone();
    view! { cx,
        tr {
            td {
                div(class=format!("{} tooltip", led_class())) {
                    span(class="tooltiptext") { (ping_ago()) " seconds ago" }
                }
            }
            td { (hostname.clone().unwrap_or_default()) }
            td { (unit.mac) }
            td { (image) }
            td { "row " (unit.row) " col " (unit.col) }
            td { (unit.next_action) }
            td {
                button(id=id_pull, on:click=move |_| send_req(url_pull.clone()) ) {
                    "flash"
                }
            }
            td {
                button(id=id_push, on:click=move |_| send_req(url_push.clone()) ) {
                    "store"
                }
            }
            td {
                button(id=id_boot, on:click=move |_| send_req(url_boot.clone()) ) {
                    "reboot"
                }
            }
            td {
                button(id=id_cancel, on:click=move |_| send_req(url_cancel.clone()) ) {
                    "wait"
                }
            }
            td {
                button(id=id_register, on:click=move |_| send_req(url_register.clone()) ) {
                    "re-register"
                }
            }
            td { (fmt_ca(&unit)) }
        }
    }
}

#[component(inline_props)]
fn GroupInfo<'a, G: Html>(
    cx: Scope<'a>,
    units: Vec<Unit>,
    group_id: u8,
    group_name: String,
    images: Vec<String>,
    hostmap: HashMap<Ipv4Addr, String>,
) -> View<G> {
    let group_units = create_memo(cx, move || {
        let mut units = units
            .iter()
            .filter(|x| x.group == group_id)
            .cloned()
            .collect::<Vec<_>>();
        units.sort_by_key(|x| (x.row, x.col, x.mac));
        units
    });

    let id_pull = format!("group-{group_name}-pull");
    let url_pull = format!("/admin/action/{group_name}/pull");
    let id_boot = format!("group-{group_name}-boot");
    let url_boot = format!("/admin/action/{group_name}/reboot");
    let id_cancel = format!("group-{group_name}-cancel");
    let url_cancel = format!("/admin/action/{group_name}/wait");

    let set_images = View::new_fragment(
        images
            .iter()
            .map(|image| {
                let url = format!("/admin/image/{group_name}/{image}");
                let text = format!("Set image to {:?}", image);
                view! { cx,
                    button(on:click=move |_| send_req(url.clone())) {
                        (text)
                    }
                }
            })
            .collect(),
    );

    view! { cx,
        h2 { (group_name) }
        button(id=id_pull, on:click=move |_| send_req(url_pull.clone()) ) {
            "Pull image on all machines"
        }
        button(id=id_boot, on:click=move |_| send_req(url_boot.clone()) ) {
            "Set all machines to boot into the OS"
        }
        button(id=id_cancel, on:click=move |_| send_req(url_cancel.clone()) ) {
            "Set all machines to wait for next command"
        }
        (set_images)
        table {
            tr {
                th { }
                th { "hostname" }
                th { "mac" }
                th { "image" }
                th { "position" }
                th { "next action" }
                th(colspan=5) { "change action" }
                th { "current action" }
            }
            Indexed(
                iterable=group_units,
                view=move |cx, x| {
                    let hostname = hostmap.get(&x.static_ip()).cloned();
                    view! { cx,
                        UnitInfo(unit=x, hostname=hostname)
                    }
                },
            )
        }
    }
}

struct Bytes(u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1024 {
            write!(f, "{} B", self.0)
        } else if self.0 < 1024 * 1024 {
            write!(f, "{:.2} KiB", self.0 as f64 / 1024.0)
        } else if self.0 < 1024 * 1024 * 1024 {
            write!(f, "{:.2} MiB", self.0 as f64 / 1024.0 / 1024.0)
        } else {
            write!(f, "{:.2} GiB", self.0 as f64 / 1024.0 / 1024.0 / 1024.0)
        }
    }
}

#[component(inline_props)]
fn Images<'a, G: Html>(cx: Scope<'a>, images_signal: &'a ReadSignal<Option<ImageStat>>) -> View<G> {
    let make_image_row = move |name: String, image: (u64, u64)| {
        let id_pull = format!("image-{name}-pull");
        let url_pull = format!("/admin/action/{name}/pull");
        let id_boot = format!("image-{name}-boot");
        let url_boot = format!("/admin/action/{name}/reboot");
        let id_cancel = format!("image-{name}-cancel");
        let url_cancel = format!("/admin/action/{name}/wait");
        view! { cx, tr {
            td { (name) }
            td { (Bytes(image.0)) }
            td { (Bytes(image.1)) }
            td {
                button(id=id_pull, on:click=move |_| send_req(url_pull.clone()) ) {
                    "Pull image on all machines"
                }
            }
            td {
                button(id=id_boot, on:click=move |_| send_req(url_boot.clone()) ) {
                    "Set all machines to boot into the OS"
                }
            }
            td {
                button(id=id_cancel, on:click=move |_| send_req(url_cancel.clone()) ) {
                    "Set all machines to wait for next command"
                }
            }
          }
        }
    };

    let images_table = move || {
        View::new_fragment(
            images_signal
                .get()
                .deref()
                .as_ref()
                .map(|images| {
                    images
                        .images
                        .iter()
                        .map(|(name, image)| make_image_row(name.clone(), *image))
                        .collect()
                })
                .unwrap_or_default(),
        )
    };

    let total_csize = move || {
        images_signal
            .get()
            .deref()
            .as_ref()
            .map(|images| images.total_csize)
    };

    let reclaimable = move || {
        images_signal
            .get()
            .deref()
            .as_ref()
            .map(|images| images.reclaimable)
    };

    view! { cx,
        h1 { "Images" }
        table {
            tr {
                th { "Image" }
                th { "Size" }
                th { "Compressed" }
            }
            (images_table())
            tr {
                td { "Total" }
                td { (Bytes(total_csize().unwrap_or_default())) }
            }
            tr {
                td { "Reclaimable" }
                td { (Bytes(reclaimable().unwrap_or_default())) }
                td { }
                td {
                    button(id="reclaime", on:click=move |_| send_req("/admin/gc".into()) ) {
                        "Reclaim disk space"
                    }
                }
            }
        }
    }
}

#[component]
async fn MainView<G: Html>(cx: Scope<'_>) -> View<G> {
    let config_signal = create_signal(cx, None::<Config>);
    let hostmap_signal = create_signal(cx, None::<HashMap<Ipv4Addr, String>>);
    let units_signal = create_signal(cx, None::<Vec<Unit>>);
    let images_signal = create_signal(cx, None::<ImageStat>);

    spawn_local_scoped(cx, async move {
        let url = format!(
            "ws://{}/admin/ws",
            web_sys::window().unwrap().location().host().unwrap()
        );

        let mut ws = WebSocket::open(&url).unwrap_or_else(|err| {
            log(&format!("Failed to open websocket: {:?}", err));
            panic!();
        });

        loop {
            let msg = ws
                .next()
                .await
                .unwrap_or_else(|| {
                    log("Websocket closed");
                    panic!();
                })
                .unwrap_or_else(|err| {
                    log(&format!("Websocket error: {:?}", err));
                    panic!();
                });

            match msg {
                Message::Text(text) => {
                    let update: WsUpdate = serde_json::from_str(&text).unwrap_or_else(|err| {
                        log(&format!("Failed to parse websocket message: {:?}", err));
                        panic!();
                    });

                    match update {
                        WsUpdate::Config(config) => {
                            config_signal.set(Some(config));
                        }
                        WsUpdate::HostMap(hostmap) => {
                            hostmap_signal.set(Some(hostmap));
                        }
                        WsUpdate::Units(units) => {
                            units_signal.set(Some(units));
                        }
                        WsUpdate::ImageStats(images) => {
                            images_signal.set(Some(images));
                        }
                    }
                }
                Message::Bytes(_) => {
                    panic!("Unexpected binary message")
                }
            }
        }
    });

    let groups = move || {
        let config_opt = config_signal.get().deref().clone();
        let units_opt = units_signal.get().deref().clone();

        View::new_fragment(
            config_opt
                .zip(units_opt)
                .map(|(config, units)| {
                    config.groups
                        .iter()
                        .map(|(name, id)| {
                            let units = units.clone();
                            let images = config.images.clone();
                            let hostmap = hostmap_signal.get().deref().clone().unwrap_or_default();
                            view! { cx,
                            GroupInfo(units=units, group_id=*id, group_name=name.clone(), images=images, hostmap=hostmap) }
                        })
                        .collect()
                })
                .unwrap_or_default()
        )
    };

    view! { cx,
        Images(images_signal=images_signal)

        h1 { "Groups" }
        (groups())
    }
}

fn main() {
    sycamore::render(|cx| {
        view! { cx,
            MainView {}
        }
    });
}
