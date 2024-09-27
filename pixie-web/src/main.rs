use std::{collections::HashMap, fmt, net::Ipv4Addr};

use futures::StreamExt;
use leptos::*;
use leptos_use::{use_preferred_dark, use_timestamp};
use pixie_shared::{Config, ImageStat, StatusUpdate, Unit};
use reqwest::Url;
use thaw::{
    Button, ButtonColor, ButtonGroup, ButtonVariant, GlobalStyle, Popover, PopoverPlacement,
    PopoverTrigger, Space, Table, Theme, ThemeProvider,
};

fn send_req(url: String) {
    let url = if url.starts_with("http") {
        Url::parse(&url).expect("invalid url")
    } else {
        let location = window().location();
        Url::parse(&location.href().expect("no href"))
            .expect("invalid href")
            .join(&url)
            .expect("invalid url")
    };
    spawn_local(async move {
        reqwest::get(url.clone())
            .await
            .unwrap_or_else(|_| panic!("Request to {} failed", url));
    });
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

#[component]
fn Images(#[prop(into)] images: Signal<Option<ImageStat>>) -> impl IntoView {
    let image_row = move |(name, image): (String, (u64, u64))| {
        let url_pull = format!("/admin/action/{name}/pull");
        let url_boot = format!("/admin/action/{name}/reboot");
        let url_cancel = format!("/admin/action/{name}/wait");
        view! {
            <tr>
                <td>{name}</td>
                <td>{Bytes(image.0).to_string()}</td>
                <td>{Bytes(image.1).to_string()}</td>
                <td>
                    <ButtonGroup>
                        <Button
                            color=ButtonColor::Error
                            on_click=move |_| send_req(url_pull.clone())
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
                <td>{move || Bytes(total_csize().unwrap_or_default()).to_string()}</td>
                <td></td>
            </tr>
            <tr>
                <td>"Reclaimable"</td>
                <td>{move || Bytes(reclaimable().unwrap_or_default()).to_string()}</td>
                <td></td>
                <td>
                    <Button
                        color=ButtonColor::Primary
                        on_click=move |_| send_req("/admin/gc".into())
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
        let url_pull = move || format!("/admin/action/{}/pull", mac());
        let url_push = move || format!("/admin/action/{}/push", mac());
        let url_boot = move || format!("/admin/action/{}/reboot", mac());
        let url_cancel = move || format!("/admin/action/{}/wait", mac());
        let url_register = move || format!("/admin/action/{}/register", mac());

        let fmt_ca = move || {
            let unit = unit.get();
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
                        <Button color=ButtonColor::Error on_click=move |_| send_req(url_pull())>
                            "flash"
                        </Button>
                        <Button color=ButtonColor::Warning on_click=move |_| send_req(url_push())>
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
                    </ButtonGroup>
                </td>
                <td class="expand">{fmt_ca}</td>
            </tr>
        }
        .into_view()
    };

    let url_pull = move || format!("/admin/action/{}/pull", group_name.get());
    let url_boot = move || format!("/admin/action/{}/reboot", group_name.get());
    let url_cancel = move || format!("/admin/action/{}/wait", group_name.get());

    let image_button = move |image: String| {
        let text = format!("Set image to {:?}", image);
        let url = move || format!("/admin/image/{}/{}", group_name.get(), image);
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
                <Button color=ButtonColor::Error on_click=move |_| send_req(url_pull())>
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
                </tr>
                <For each=move || 0..units.get().len() key=|x| *x children=render_unit/>
            </Table>
        </Space>
    }
}

#[component]
fn App() -> impl IntoView {
    let (config, set_config) = create_signal(None::<Config>);
    let (hostmap, set_hostname) = create_signal(None::<HashMap<Ipv4Addr, String>>);
    let (units, set_units) = create_signal(None::<Vec<Unit>>);
    let (image_stats, set_image_stats) = create_signal(None::<ImageStat>);

    let images = Signal::derive(move || {
        config
            .get()
            .map(|x| x.images.clone())
            .unwrap_or_else(Vec::new)
    });

    let location =
        Url::parse(&window().location().href().expect("no href")).expect("invalid url href");
    let status_url = location
        .join("admin/status")
        .expect("could not make relative URL");

    let handle_message = move |msg| match msg {
        StatusUpdate::Units(u) => {
            set_units.set(Some(u));
        }
        StatusUpdate::Config(c) => {
            set_config.set(Some(c));
        }
        StatusUpdate::HostMap(h) => {
            set_hostname.set(Some(h));
        }
        StatusUpdate::ImageStats(i) => {
            set_image_stats.set(Some(i));
        }
    };

    spawn_local(async move {
        let stream = reqwest::get(status_url)
            .await
            .expect("could not connect to server");
        let mut buf = vec![];
        stream
            .bytes_stream()
            .for_each(|x| {
                let data = x.unwrap();
                let mut data = &data[..];
                while let Some((newline_pos, _)) =
                    data.iter().enumerate().find(|(_, x)| **x == b'\n')
                {
                    buf.extend_from_slice(&data[..newline_pos]);
                    let msg: StatusUpdate =
                        serde_json::from_slice(&buf).expect("invalid message from server");
                    buf.clear();
                    handle_message(msg);
                    data = &data[newline_pos + 1..];
                }
                buf.extend_from_slice(data);

                async {}
            })
            .await;
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

    view! {
        <Images images=image_stats/>
        <For
            each=move || {
                config.get().clone().into_iter().flat_map(|x| x.groups.into_iter().map(|x| x.1))
            }
            key=|x| *x
            children=render_group
        />
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
