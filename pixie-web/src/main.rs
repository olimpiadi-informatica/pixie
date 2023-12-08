use std::collections::HashMap;
use std::net::Ipv4Addr;
pub use std::time::*;

use gloo_timers::future::TimeoutFuture;
use pixie_shared::{Config, Unit};
use sycamore::prelude::*;
use sycamore::{
    futures::{spawn_local, spawn_local_scoped},
    suspense::Suspense,
};
use wasm_bindgen::prelude::*;

use reqwasm::http::Request;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = Date, js_name = now)]
    fn date_now() -> f64;
}

async fn make_req<T: for<'de> serde::Deserialize<'de>>(url: &str) -> T {
    Request::get(url)
        .send()
        .await
        .expect(&format!("Request to {} failed", url))
        .json()
        .await
        .expect("Invalid response")
}

fn send_req(url: String) {
    spawn_local(async move {
        Request::get(&url)
            .send()
            .await
            .expect(&format!("Request to {} failed", url));
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
    let fmt_ca = |unit: &Unit| {
        if let Some(a) = unit.curr_action {
            if let Some((x, y)) = unit.curr_progress {
                format!("{} ({}/{})", a, x, y)
            } else {
                a.to_string()
            }
        } else {
            let ping_time = unit.last_ping_timestamp as i64;
            let now = (*time.get() * 0.001) as i64;
            format!(
                "ping: {} seconds ago, {}",
                now - ping_time,
                String::from_utf8_lossy(&unit.last_ping_msg)
            )
        }
    };

    let mac = unit.mac.to_string();
    let mac_nocolon = mac.replace(":", "");

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
    units: &'a ReadSignal<Vec<Unit>>,
    group_id: u8,
    group_name: String,
    images: Vec<String>,
    hostmap: HashMap<Ipv4Addr, String>,
) -> View<G> {
    let group_units = create_memo(cx, move || {
        let mut units = units
            .get()
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

#[component(inline_props)]
fn Images<'a, 'b, G: Html>(cx: Scope<'a>, images: &'b Vec<String>) -> View<G> {
    let make_image_row = |x: String| {
        let id_pull = format!("image-{x}-pull");
        let url_pull = format!("/admin/action/{x}/pull");
        let id_boot = format!("image-{x}-boot");
        let url_boot = format!("/admin/action/{x}/reboot");
        let id_cancel = format!("image-{x}-cancel");
        let url_cancel = format!("/admin/action/{x}/wait");
        view! { cx, tr {
            td { (x) }
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

    let images = View::new_fragment(images.iter().cloned().map(make_image_row).collect());

    view! { cx,
        h1 { "Images" }
        table {
            (images)
        }
    }
}

#[component]
async fn UnitView<G: Html>(cx: Scope<'_>) -> View<G> {
    let config: Config = make_req("/admin/config").await;
    let hostmap: HashMap<Ipv4Addr, String> = make_req("/admin/hostmap").await;

    let units = create_signal(cx, make_req::<Vec<Unit>>("/admin/units").await);

    spawn_local_scoped(cx, async move {
        loop {
            TimeoutFuture::new(100).await;
            let new = make_req("/admin/units").await;
            if new != *units.get() {
                units.set(new);
            }
        }
    });

    let groups = View::new_fragment(
        config
            .groups
            .iter()
            .map(|(name, id)| {
                let images = config.images.clone();
                let hostmap = hostmap.clone();
                view! { cx,
                GroupInfo(units=units, group_id=*id, group_name=name.clone(), images=images, hostmap=hostmap) }
            })
            .collect(),
    );

    let images = config.images.clone();
    view! { cx,
        Images(images=&images)

        h1 { "Groups" }
        (groups)
    }
}

fn main() {
    sycamore::render(|cx| {
        view! { cx,
            Suspense(fallback=view! { cx, "Loading..." }) {
                UnitView {}
            }
        }
    });
}
