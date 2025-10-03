use std::{collections::HashMap, net::Ipv4Addr};

use futures::{StreamExt, TryStreamExt};
use gloo_net::http::Request;
use js_sys::Uint8Array;
use leptos::*;
use leptos_use::{use_preferred_dark, use_timestamp};
use pixie_shared::{util::BytesFmt, Config, ImagesStats, StatusUpdate, Unit};
use thaw::{
    Button, ButtonColor, ButtonGroup, ButtonVariant, GlobalStyle, Popover, PopoverPlacement,
    PopoverTrigger, Space, Table, Theme, ThemeProvider,
};
use wasm_bindgen_futures::stream::JsStream;

fn send_req(url: String) {
    spawn_local(async move {
        Request::get(&url)
            .send()
            .await
            .unwrap_or_else(|_| panic!("Request to {url} failed"));
    });
}

#[component]
fn Images(#[prop(into)] images: Signal<Option<ImagesStats>>) -> impl IntoView {
    let image_row = move |(full_name, image): (String, (u64, u64))| {
        let url_flash = format!("admin/action/{full_name}/flash");
        let url_boot = format!("admin/action/{full_name}/reboot");
        let url_cancel = format!("admin/action/{full_name}/wait");
        let url_rollback = format!("admin/rollback/{full_name}");
        let url_delete = format!("admin/delete/{full_name}");

        let mut it = full_name.split('@');
        let name = it.next().unwrap().to_owned();
        let version = it.next().map(ToOwned::to_owned);
        let has_version = version.is_some();

        view! {
            <tr>
                <td>{name}</td>
                <td>{version}</td>
                <td>{BytesFmt(image.0).to_string()}</td>
                <td>{BytesFmt(image.1).to_string()}</td>
                <td>
                    <ButtonGroup>
                        {
                            if !has_version {
                                view! {
                                    <Button
                                        color=ButtonColor::Error
                                        on_click=move |_| send_req(url_flash.clone())
                                    >
                                        "Flash all machines"
                                    </Button>
                                    <Button
                                        color=ButtonColor::Success
                                        on_click=move |_| send_req(url_boot.clone())
                                    >
                                        "Set all machines to boot into the OS"
                                    </Button>
                                    <Button
                                        color=ButtonColor::Primary
                                        on_click=move |_| send_req(url_cancel.clone())
                                    >
                                        "Set all machines to wait for next command"
                                    </Button>
                                }.into_view()
                            } else {
                                view! {
                                    <Button
                                        variant=ButtonVariant::Outlined
                                        on_click=move |_| send_req(url_rollback.clone())
                                        >
                                        "Rollback image"
                                    </Button>
                                    <Button
                                        color=ButtonColor::Error
                                        on_click=move |_| send_req(url_delete.clone())
                                        >
                                        "Delete image"
                                    </Button>
                                }
                                .into_view()
                            }
                        }
                    </ButtonGroup>
                </td>
            </tr>
        }
    };

    let total_csize = move || images.get().as_ref().map(|images| images.total_csize);

    let reclaimable = move || images.get().as_ref().map(|images| images.reclaimable);

    view! {
        <h1>"Images"</h1>
        <Table>
            <tr>
                <th>"Image"</th>
                <th>"Version"</th>
                <th>"Size"</th>
                <th>"Compressed"</th>
                <th></th>
            </tr>
            <For
                each=move || images.get().map(|x| x.images.clone()).unwrap_or_default()
                key=|x| x.clone()
                children=image_row
            />
            <tr>
                <td>"Total"</td>
                <td></td>
                <td></td>
                <td>{move || BytesFmt(total_csize().unwrap_or_default()).to_string()}</td>
                <td></td>
            </tr>
            <tr>
                <td>"Reclaimable"</td>
                <td></td>
                <td></td>
                <td>{move || BytesFmt(reclaimable().unwrap_or_default()).to_string()}</td>
                <td>
                    <Button
                        color=ButtonColor::Primary
                        on_click=move |_| send_req("admin/gc".into())
                    >
                        "Reclaim disk space"
                    </Button>
                </td>
            </tr>
        </Table>
    }
}

#[component]
fn Group(
    #[prop(into)] units: Signal<Vec<Unit>>,
    #[prop(into)] group_name: Signal<String>,
    images: Signal<Vec<String>>,
    hostmap: Signal<HashMap<Ipv4Addr, String>>,
    #[prop(into)] time: Signal<i64>,
) -> impl IntoView {
    let render_unit = move |idx: usize| {
        let unit = create_memo(move |_| units.get()[idx].clone());
        let ping_ago = move || time.get() - unit.get().last_ping_timestamp as i64;

        let mac = move || unit.get().mac.to_string();
        let url_flash = move || format!("admin/action/{}/flash", mac());
        let url_store = move || format!("admin/action/{}/store", mac());
        let url_boot = move || format!("admin/action/{}/reboot", mac());
        let url_cancel = move || format!("admin/action/{}/wait", mac());
        let url_register = move || format!("admin/action/{}/register", mac());
        let url_shutdown = move || format!("admin/action/{}/shutdown", mac());
        let url_forget = move || format!("admin/forget/{}", mac());

        let fmt_ca = move || {
            let unit = unit.get();
            if let Some(a) = unit.curr_action {
                if let Some((x, y)) = unit.curr_progress {
                    format!("{a} ({x}/{y})")
                } else {
                    a.to_string()
                }
            } else {
                format!(
                    "ping: {} seconds ago, {}",
                    ping_ago(),
                    String::from_utf8_lossy(&unit.last_ping_comment)
                )
            }
        };

        let led_class = move || match ping_ago() {
            ..0 => "led-blue",
            0..120 => "led-green",
            120..300 => "led-yellow",
            300.. => "led-red",
        };

        view! {
            <tr>
                <td>
                    <Popover tooltip=true placement=PopoverPlacement::Right>
                        <PopoverTrigger slot>
                            <div class=led_class></div>
                        </PopoverTrigger>
                        {move || format!("{} seconds ago", ping_ago())}
                    </Popover>
                </td>
                <td>
                    {move || {
                        hostmap.get().get(&unit.get().static_ip()).cloned().unwrap_or_default()
                    }}
                </td>
                <td>{move || unit.get().mac.to_string()}</td>
                <td>{move || unit.get().image}</td>
                <td>{move || format!("row {} col {}", unit.get().row, unit.get().col)}</td>
                <td>{move || unit.get().next_action.to_string()}</td>
                <td>
                    <ButtonGroup>
                        <Button color=ButtonColor::Error on_click=move |_| send_req(url_flash())>
                            "flash"
                        </Button>
                        <Button color=ButtonColor::Warning on_click=move |_| send_req(url_store())>
                            "store"
                        </Button>
                        <Button color=ButtonColor::Success on_click=move |_| send_req(url_boot())>
                            "reboot"
                        </Button>
                        <Button color=ButtonColor::Primary on_click=move |_| send_req(url_cancel())>
                            "wait"
                        </Button>
                        <Button
                            variant=ButtonVariant::Outlined
                            on_click=move |_| send_req(url_register())
                        >
                            "re-register"
                        </Button>
                        <Button
                            variant=ButtonVariant::Outlined
                            on_click=move |_| send_req(url_shutdown())
                        >
                            "shutdown"
                        </Button>
                    </ButtonGroup>
                </td>
                <td class="expand">{fmt_ca}</td>
                <td>
                    <Button color=ButtonColor::Error on_click=move |_| send_req(url_forget())>
                    "forget"
                    </Button>
                </td>
            </tr>
        }
        .into_view()
    };

    let url_flash = move || format!("admin/action/{}/flash", group_name.get());
    let url_boot = move || format!("admin/action/{}/reboot", group_name.get());
    let url_cancel = move || format!("admin/action/{}/wait", group_name.get());

    let image_button = move |image: String| {
        let text = format!("Set image to {image:?}");
        let url = move || format!("admin/image/{}/{}", group_name.get(), image);
        view! {
            <Button color=ButtonColor::Error on_click=move |_| send_req(url())>
                {text}
            </Button>
        }
    };

    view! {
        <h1>{group_name}</h1>
        <Space vertical=true>
            <ButtonGroup>
                <Button color=ButtonColor::Error on_click=move |_| send_req(url_flash())>
                    "Flash all machines"
                </Button>
                <Button color=ButtonColor::Success on_click=move |_| send_req(url_boot())>
                    "Set all machines to boot into the OS"
                </Button>
                <Button color=ButtonColor::Primary on_click=move |_| send_req(url_cancel())>
                    "Set all machines to wait for next command"
                </Button>
                <For each=move || images.get() key=|x| x.clone() children=image_button/>
            </ButtonGroup>
            <Table>
                <tr>
                    <th></th>
                    <th>"hostname"</th>
                    <th>"mac"</th>
                    <th>"image"</th>
                    <th>"position"</th>
                    <th>"next action"</th>
                    <th>"change action"</th>
                    <th>"current action"</th>
                    <th></th>
                </tr>
                <For each=move || 0..units.get().len() key=|x| *x children=render_unit/>
            </Table>
        </Space>
    }
}

#[component]
fn Disconnect(connected: ReadSignal<bool>) -> impl IntoView {
    view! {
        <Show when=move || !connected.get()>
            <div style="
                position: fixed;
                bottom: 1em;
                right: 1em;
                background-color: rgba(255, 0, 0, 0.1);
                color: #900;
                padding: 0.5em 1em;
                border-radius: 0.5em;
                z-index: 1000;
                pointer-events: none;
            ">
                "⚠️ Disconnected from server"
            </div>
        </Show>
    }
}

#[component]
fn App() -> impl IntoView {
    let (connected, set_connected) = create_signal(true);

    let (config, set_config) = create_signal(None::<Config>);
    let (hostmap, set_hostname) = create_signal(None::<HashMap<Ipv4Addr, String>>);
    let (units, set_units) = create_signal(None::<Vec<Unit>>);
    let (image_stats, set_image_stats) = create_signal(None::<ImagesStats>);

    let images = Signal::derive(move || {
        config
            .get()
            .map(|x| x.images.clone())
            .unwrap_or_else(Vec::new)
    });

    let handle_message = move |msg| match msg {
        StatusUpdate::Units(mut u) => {
            u.sort_by_key(|x| x.static_ip());
            set_units.set(Some(u));
        }
        StatusUpdate::Config(c) => set_config.set(Some(c)),
        StatusUpdate::HostMap(h) => set_hostname.set(Some(h)),
        StatusUpdate::ImagesStats(i) => set_image_stats.set(Some(i)),
    };

    spawn_local(async move {
        struct Disconnect(WriteSignal<bool>);

        impl Drop for Disconnect {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }

        let _disconnect = Disconnect(set_connected);

        let req = Request::get("admin/status");
        let res = req.send().await.expect("could not connect to server");
        let body = res.body().expect("could not get body");
        let js_stream = JsStream::from(body.values());
        let mut stream = js_stream.map(|item| item.map(|js_val| Uint8Array::new(&js_val).to_vec()));

        let mut buf = vec![];
        while let Some(data) = stream.try_next().await.unwrap() {
            let mut data = &data[..];
            while let Some(newline_pos) = data.iter().position(|x| *x == b'\n') {
                buf.extend_from_slice(&data[..newline_pos]);
                let msg: StatusUpdate =
                    serde_json::from_slice(&buf).expect("invalid message from server");
                buf.clear();
                handle_message(msg);
                data = &data[newline_pos + 1..];
            }
            buf.extend_from_slice(data);
        }
    });

    let time = use_timestamp();
    let time_in_seconds = create_memo(move |_| (time.get() * 0.001) as i64);

    let render_group = move |id: u8| {
        let group_name = create_memo(move |_| {
            config
                .get()
                .unwrap()
                .groups
                .get_by_second(&id)
                .unwrap()
                .clone()
        });
        let units = create_memo(move |_| -> Vec<_> {
            units
                .get()
                .unwrap_or_else(Vec::new)
                .iter()
                .filter(|x| x.group == id)
                .cloned()
                .collect()
        });
        let hostmap = Signal::derive(move || hostmap.get().unwrap_or_else(HashMap::new));
        view! { <Group units group_name images hostmap time=time_in_seconds/> }.into_view()
    };

    let render_group_grid = move |id: u8| {
        let group_name = create_memo(move |_| {
            config
                .get()
                .unwrap()
                .groups
                .get_by_second(&id)
                .unwrap()
                .clone()
        });
        let units = create_memo(move |_| -> Vec<_> {
            units
                .get()
                .unwrap_or_else(Vec::new)
                .iter()
                .filter(|x| x.group == id)
                .cloned()
                .collect()
        });
        let render_unit_grid = move |idx: usize| {
            let unit = create_memo(move |_| units.get()[idx].clone());
            let ping_ago = move || time_in_seconds.get() - unit.get().last_ping_timestamp as i64;

            let class = move || match ping_ago() {
                ..0 => "grid-blue",
                0..120 => "grid-green",
                120..300 => "grid-yellow",
                300.. => "grid-red",
            };

            let size_style = "width: 16px; height: 16px;";

            let style = move || {
                format!(
                    "grid-column: {}; grid-row: {}; {size_style}",
                    unit.get().col,
                    unit.get().row
                )
            };

            let popover_text = move || {
                format!(
                    "row {} col {}: {} seconds ago",
                    unit.get().row,
                    unit.get().col,
                    ping_ago()
                )
            };

            view! {
                <div style=style class=class>
                    <Popover tooltip=true placement=PopoverPlacement::Right>
                        <PopoverTrigger slot>
                            <div class=class style=size_style>
                            </div>
                        </PopoverTrigger>
                        {popover_text}
                    </Popover>
                </div>
            }
        };
        view! {
            <div style="display: inline-block; text-align: center;" >
                <h2>{group_name}</h2>
                <div style="display: inline-grid; margin: 10px; transform: scaleY(-1);">
                    <For each=move || 0..units.get().len() key=|x| *x children=render_unit_grid/>
                </div>
            </div>
        }
    };

    view! {
        <Images images=image_stats/>
        <h1>Ping Summary</h1>
        <Space vertical=false>
            <For
                each=move || {
                    config.get().clone().into_iter().flat_map(|x| x.groups.into_iter().map(|x| x.1))
                }
                key=|x| *x
                children=render_group_grid
            />
        </Space>
        <For
            each=move || {
                config.get().clone().into_iter().flat_map(|x| x.groups.into_iter().map(|x| x.1))
            }
            key=|x| *x
            children=render_group
        />
        <Disconnect connected />
    }
}

fn main() {
    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Debug).unwrap();

    let dark_mode = use_preferred_dark();
    let theme = create_rw_signal(Theme::dark());
    create_effect(move |_| {
        theme.set(if dark_mode.get() {
            Theme::dark()
        } else {
            Theme::light()
        })
    });

    mount_to_body(move || {
        view! {
            <ThemeProvider theme>
                <GlobalStyle />
                <App/>
            </ThemeProvider>
        }
    })
}
