use gloo_timers::future::TimeoutFuture;
use pixie_shared::{ActionKind, Config, Unit};
use sycamore::prelude::*;
use sycamore::{
    futures::{spawn_local, spawn_local_scoped},
    suspense::Suspense,
};

use reqwasm::http::Request;

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
fn UnitInfo<G: Html>(cx: Scope<'_>, unit: Unit) -> View<G> {
    let fmt_ca = |curr_action: Option<ActionKind>, progress: Option<(usize, usize)>| {
        if let Some(a) = curr_action {
            if let Some((x, y)) = progress {
                format!("{} ({}/{})", a, x, y)
            } else {
                a.to_string()
            }
        } else {
            "".into()
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

    view! { cx,
        tr {
            td { (unit.mac) }
            td { (unit.image) }
            td { "row " (unit.row) " col " (unit.col) }
            td { (unit.next_action) }
            td {
                button(id=id_pull, on:click=move |_| send_req(url_pull.clone()) ) {
                    "pull"
                }
            }
            td {
                button(id=id_push, on:click=move |_| send_req(url_push.clone()) ) {
                    "push"
                }
            }
            td {
                button(id=id_boot, on:click=move |_| send_req(url_boot.clone()) ) {
                    "OS"
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
            td { (fmt_ca(unit.curr_action, unit.curr_progress)) }
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
) -> View<G> {
    let group_units = create_memo(cx, move || {
        units
            .get()
            .iter()
            .filter(|x| x.group == group_id)
            .cloned()
            .collect::<Vec<_>>()
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
                th { "mac" }
                th { "image" }
                th { "position" }
                th { "next action" }
                th(colspan=5) { "change action" }
                th { "current action" }
            }
            Indexed(
                iterable=group_units,
                view=|cx, x| {
                    view! { cx,
                        UnitInfo(unit=x)
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
                view! { cx,
                GroupInfo(units=units, group_id=*id, group_name=name.clone(), images=images) }
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
            style {
                ":root { color-scheme: light dark; }"
            }

            Suspense(fallback=view! { cx, "Loading..." }) {
                UnitView {}
            }
        }
    });
}
