use std::{collections::HashMap, net::Ipv4Addr};

use futures::{StreamExt, TryStreamExt};
use gloo_net::http::Request;
use gloo_timers::future::{IntervalStream, TimeoutFuture};
use js_sys::{Date, Uint8Array};
use leptos::prelude::*;
use leptos::task::spawn_local;
use macaddr::MacAddr6;
use pixie_shared::{Action, Config, ImagesStats, StatusUpdate, Unit, util::BytesFmt};
use wasm_bindgen_futures::stream::JsStream;

fn send_req(url: String) {
    spawn_local(async move {
        if let Err(err) = Request::get(&url).send().await {
            log::error!("Request to {url} failed: {err}");
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    All,
    ActiveOrQueued,
    StaleOrOffline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PingBucket {
    Future,
    Online,
    Warning,
    Offline,
}

impl PingBucket {
    fn from_ping_ago(last_ping_timestamp: u64, now_secs: i64) -> Self {
        if last_ping_timestamp == 0 {
            return PingBucket::Offline;
        }
        let ago = now_secs - last_ping_timestamp as i64;
        match ago {
            ..0 => PingBucket::Future,
            0..120 => PingBucket::Online,
            120..300 => PingBucket::Warning,
            300.. => PingBucket::Offline,
        }
    }

    fn dot_class(self) -> &'static str {
        match self {
            PingBucket::Future => "bg-info shadow-[0_0_6px_var(--color-info)]",
            PingBucket::Online => "bg-success shadow-[0_0_6px_var(--color-success)]",
            PingBucket::Warning => "bg-warning shadow-[0_0_6px_var(--color-warning)]",
            PingBucket::Offline => "bg-error shadow-[0_0_6px_var(--color-error)]",
        }
    }

    fn grid_bg_class(self, is_active: bool) -> &'static str {
        if is_active {
            "bg-info text-info-content"
        } else {
            match self {
                PingBucket::Future => "bg-info/80 text-info-content",
                PingBucket::Online => "bg-success/85 text-success-content",
                PingBucket::Warning => "bg-warning/85 text-warning-content",
                PingBucket::Offline => "bg-error/85 text-error-content",
            }
        }
    }
}

fn format_ago(last_ping_timestamp: u64, now_secs: i64) -> String {
    if last_ping_timestamp == 0 {
        return "never".to_owned();
    }
    let ago = (now_secs - last_ping_timestamp as i64).max(0);
    if ago < 60 {
        format!("{ago}s ago")
    } else if ago < 3600 {
        format!("{}m {:02}s ago", ago / 60, ago % 60)
    } else {
        format!("{}h {:02}m ago", ago / 3600, (ago % 3600) / 60)
    }
}

fn action_badge_class(action: Action) -> &'static str {
    match action {
        Action::Wait => "badge badge-ghost badge-sm",
        Action::Flash => "badge badge-error badge-sm",
        Action::Store => "badge badge-warning badge-sm",
        Action::Boot | Action::Restart => "badge badge-success badge-sm",
        Action::Register => "badge badge-info badge-sm",
        Action::Shutdown => "badge badge-neutral badge-sm",
    }
}

fn unit_matches_filter(
    unit: &Unit,
    hostname: &str,
    mode: FilterMode,
    query: &str,
    now_secs: i64,
) -> bool {
    let is_active_or_queued = unit.curr_action.is_some() || unit.next_action != Action::Wait;
    let bucket = PingBucket::from_ping_ago(unit.last_ping_timestamp, now_secs);
    let is_stale =
        matches!(bucket, PingBucket::Warning | PingBucket::Offline) && unit.curr_action.is_none();

    match mode {
        FilterMode::All => {}
        FilterMode::ActiveOrQueued => {
            if !is_active_or_queued {
                return false;
            }
        }
        FilterMode::StaleOrOffline => {
            if !is_stale {
                return false;
            }
        }
    }

    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let q = q.to_ascii_lowercase();
    let ip_str = unit.static_ip().to_string();
    let mac_str = unit.mac.to_string().to_ascii_lowercase();
    let pos_short = format!("r{}c{}", unit.row, unit.col);
    let pos_long = format!("row {} col {}", unit.row, unit.col);

    hostname.to_ascii_lowercase().contains(&q)
        || ip_str.contains(&q)
        || mac_str.contains(&q)
        || unit.image.to_ascii_lowercase().contains(&q)
        || pos_short.contains(&q)
        || pos_long.contains(&q)
}

#[component]
fn ImagesSection(images: RwSignal<Option<ImagesStats>>) -> impl IntoView {
    let (collapsed, set_collapsed) = signal(false);

    let total_csize = move || images.with(|imgs| imgs.as_ref().map(|i| i.total_csize).unwrap_or(0));
    let reclaimable = move || images.with(|imgs| imgs.as_ref().map(|i| i.reclaimable).unwrap_or(0));
    let image_list = move || {
        images.with(|imgs| {
            imgs.as_ref()
                .map(|i| {
                    i.images
                        .iter()
                        .map(|(k, v)| (k.clone(), *v))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };

    view! {
        <div class="card bg-base-100 shadow-sm border border-base-300">
            <div class="card-body p-4 gap-3">
                <div class="flex flex-wrap items-center justify-between gap-2">
                    <div class="flex items-center gap-3">
                        <button
                            class="btn btn-ghost btn-xs btn-square"
                            on:click=move |_| set_collapsed.update(|c| *c = !*c)
                        >
                            {move || if collapsed.get() { "▸" } else { "▾" }}
                        </button>
                        <h2 class="card-title text-lg">"Images"</h2>
                        <span class="badge badge-neutral badge-sm">
                            {move || format!("Total: {}", BytesFmt(total_csize()))}
                        </span>
                        <span class=move || {
                            if reclaimable() > 0 {
                                "badge badge-warning badge-sm"
                            } else {
                                "badge badge-ghost badge-sm"
                            }
                        }>
                            {move || format!("Reclaimable: {}", BytesFmt(reclaimable()))}
                        </span>
                    </div>
                    <button
                        class="btn btn-primary btn-xs"
                        disabled=move || reclaimable() == 0
                        on:click=move |_| send_req("admin/gc".into())
                    >
                        "Reclaim disk space"
                    </button>
                </div>

                <Show when=move || !collapsed.get()>
                    <div class="overflow-x-auto">
                        <table class="table table-xs table-zebra">
                            <thead>
                                <tr>
                                    <th>"Image"</th>
                                    <th>"Version"</th>
                                    <th>"Uncompressed"</th>
                                    <th>"Compressed"</th>
                                    <th class="text-right">"Actions"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=image_list
                                    key=|(name, sizes)| (name.clone(), *sizes)
                                    children=move |(full_name, (size, csize))| {
                                        let url_flash = format!("admin/action/image:{full_name}/flash");
                                        let url_boot = format!("admin/action/image:{full_name}/boot");
                                        let url_cancel = format!("admin/action/image:{full_name}/wait");
                                        let url_rollback = format!("admin/rollback/{full_name}");
                                        let url_delete = format!("admin/delete/{full_name}");

                                        let mut it = full_name.split('@');
                                        let name = it.next().unwrap_or_default().to_owned();
                                        let version = it.next().map(ToOwned::to_owned);
                                        let has_version = version.is_some();

                                        view! {
                                            <tr>
                                                <td class="font-medium">{name}</td>
                                                <td class="font-mono text-xs opacity-75">
                                                    {version.unwrap_or_else(|| "current".to_owned())}
                                                </td>
                                                <td class="font-mono text-xs">{BytesFmt(size).to_string()}</td>
                                                <td class="font-mono text-xs">{BytesFmt(csize).to_string()}</td>
                                                <td class="text-right">
                                                    {if !has_version {
                                                        view! {
                                                            <div class="join">
                                                                <button
                                                                    class="btn btn-error btn-xs join-item"
                                                                    on:click=move |_| send_req(url_flash.clone())
                                                                >
                                                                    "Flash all"
                                                                </button>
                                                                <button
                                                                    class="btn btn-success btn-xs join-item"
                                                                    on:click=move |_| send_req(url_boot.clone())
                                                                >
                                                                    "Boot all"
                                                                </button>
                                                                <button
                                                                    class="btn btn-primary btn-xs join-item"
                                                                    on:click=move |_| send_req(url_cancel.clone())
                                                                >
                                                                    "Wait all"
                                                                </button>
                                                            </div>
                                                        }
                                                            .into_any()
                                                    } else {
                                                        view! {
                                                            <div class="join">
                                                                <button
                                                                    class="btn btn-outline btn-xs join-item"
                                                                    on:click=move |_| send_req(url_rollback.clone())
                                                                >
                                                                    "Rollback"
                                                                </button>
                                                                <button
                                                                    class="btn btn-error btn-xs join-item"
                                                                    on:click=move |_| send_req(url_delete.clone())
                                                                >
                                                                    "Delete"
                                                                </button>
                                                            </div>
                                                        }
                                                            .into_any()
                                                    }}
                                                </td>
                                            </tr>
                                        }
                                    }
                                />
                            </tbody>
                        </table>
                    </div>
                </Show>
            </div>
        </div>
    }
}

#[component]
fn RoomMapCard(
    group_id: u8,
    group_name: Memo<String>,
    unit_macs: Memo<Vec<MacAddr6>>,
    unit_signals: RwSignal<HashMap<MacAddr6, RwSignal<Unit>>>,
    hostmap: RwSignal<HashMap<Ipv4Addr, String>>,
    now_secs: RwSignal<i64>,
    highlighted_mac: RwSignal<Option<MacAddr6>>,
) -> impl IntoView {
    let (max_row, max_col) = (
        Memo::new(move |_| {
            let macs = unit_macs.get();
            unit_signals.with(|map| {
                macs.iter()
                    .filter_map(|m| map.get(m))
                    .map(|sig| sig.with(|u| u.row))
                    .max()
                    .unwrap_or(1)
                    .max(1)
            })
        }),
        Memo::new(move |_| {
            let macs = unit_macs.get();
            unit_signals.with(|map| {
                macs.iter()
                    .filter_map(|m| map.get(m))
                    .map(|sig| sig.with(|u| u.col))
                    .max()
                    .unwrap_or(1)
                    .max(1)
            })
        }),
    );

    let render_cell = move |mac: MacAddr6| {
        let Some(unit_sig) = unit_signals.with_untracked(|m| m.get(&mac).copied()) else {
            return ().into_any();
        };

        let row = Memo::new(move |_| unit_sig.with(|u| u.row));
        let col = Memo::new(move |_| unit_sig.with(|u| u.col));
        let ip = Memo::new(move |_| unit_sig.with(Unit::static_ip));
        let curr_action = Memo::new(move |_| unit_sig.with(|u| u.curr_action));
        let curr_progress = Memo::new(move |_| unit_sig.with(|u| u.curr_progress));
        let next_action = Memo::new(move |_| unit_sig.with(|u| u.next_action));
        let last_ping = Memo::new(move |_| unit_sig.with(|u| u.last_ping_timestamp));

        let bucket = Memo::new(move |_| PingBucket::from_ping_ago(last_ping.get(), now_secs.get()));

        let tooltip = move || {
            let host = hostmap.with(|h| h.get(&ip.get()).cloned().unwrap_or_default());
            let host_label = if host.is_empty() {
                ip.get().to_string()
            } else {
                format!("{host} ({})", ip.get())
            };
            let status_str = if let Some(act) = curr_action.get() {
                if let Some((done, tot)) = curr_progress.get() {
                    let pct = (done * 100).checked_div(tot).unwrap_or(0);
                    format!("{act}: {done}/{tot} ({pct}%)")
                } else {
                    act.to_string()
                }
            } else {
                format!("ping: {}", format_ago(last_ping.get(), now_secs.get()))
            };
            let next = next_action.get();
            let queued_str = if next != Action::Wait {
                format!(" (queued: {next})")
            } else {
                String::new()
            };
            format!(
                "{host_label} [R{} C{}] — {status_str}{queued_str}",
                row.get(),
                col.get()
            )
        };

        let cell_class = move || {
            let is_active = curr_action.get().is_some();
            let bg = bucket.get().grid_bg_class(is_active);
            let queued_ring = if next_action.get() != Action::Wait && !is_active {
                "ring-2 ring-warning"
            } else {
                ""
            };
            let c = col.get();
            let r = row.get();
            let mc = max_col.get();
            let mr = max_row.get();
            let tip_dir = if c <= 3 {
                "tooltip-right"
            } else if mc >= 6 && c + 2 >= mc {
                "tooltip-left"
            } else if r == mr {
                "tooltip-bottom"
            } else {
                "tooltip-top"
            };
            format!(
                "tooltip {tip_dir} relative hover:z-50 w-7 h-7 rounded text-[9px] \
                 tracking-tighter leading-none font-mono font-bold flex items-center \
                 justify-center cursor-pointer select-none hover:brightness-110 {bg} {queued_ring}"
            )
        };

        let label_class = move || {
            if curr_action.get().is_some() {
                "animate-pulse"
            } else {
                ""
            }
        };

        let cell_label = move || {
            if let Some((done, tot)) = curr_progress.get()
                && tot > 0
            {
                return format!("{}%", (done * 100) / tot);
            }
            if let Some(act) = curr_action.get() {
                return match act {
                    Action::Flash => "FL".to_owned(),
                    Action::Store => "ST".to_owned(),
                    Action::Register => "RG".to_owned(),
                    Action::Boot => "BT".to_owned(),
                    Action::Restart => "RS".to_owned(),
                    Action::Shutdown => "SD".to_owned(),
                    Action::Wait => "WT".to_owned(),
                };
            }
            format!("{},{}", row.get(), col.get())
        };

        let on_click = move |_| {
            highlighted_mac.set(Some(mac));
            if let Some(el) = document().get_element_by_id(&format!("unit-row-{mac}")) {
                el.scroll_into_view();
            }
        };

        view! {
            <div
                style=move || {
                    let grid_r = max_row.get().saturating_sub(row.get()) + 1;
                    format!("grid-column: {}; grid-row: {};", col.get(), grid_r)
                }
                class=cell_class
                data-tip=tooltip
                on:click=on_click
            >
                <span class=label_class>{cell_label}</span>
            </div>
        }
        .into_any()
    };

    let _ = group_id;

    view! {
        <div class="card bg-base-100 shadow-sm border border-base-300 overflow-visible hover:z-40">
            <div class="card-body p-3 gap-2 overflow-visible">
                <div class="flex items-center justify-between gap-4">
                    <h3 class="font-semibold text-sm">{move || group_name.get()}</h3>
                    <span class="badge badge-ghost badge-xs">
                        {move || format!("{} PCs", unit_macs.with(Vec::len))}
                    </span>
                </div>
                <div class="inline-grid gap-1.5 p-1 overflow-visible">
                    <For
                        each=move || unit_macs.get()
                        key=|mac| *mac
                        children=render_cell
                    />
                </div>
            </div>
        </div>
    }
}

#[component]
fn UnitRow(
    mac: MacAddr6,
    unit_sig: RwSignal<Unit>,
    hostmap: RwSignal<HashMap<Ipv4Addr, String>>,
    now_secs: RwSignal<i64>,
    filter_mode: RwSignal<FilterMode>,
    search_query: RwSignal<String>,
    highlighted_mac: RwSignal<Option<MacAddr6>>,
) -> impl IntoView {
    let ip = Memo::new(move |_| unit_sig.with(Unit::static_ip));
    let row = Memo::new(move |_| unit_sig.with(|u| u.row));
    let col = Memo::new(move |_| unit_sig.with(|u| u.col));
    let image = Memo::new(move |_| unit_sig.with(|u| u.image.clone()));
    let next_action = Memo::new(move |_| unit_sig.with(|u| u.next_action));
    let curr_action = Memo::new(move |_| unit_sig.with(|u| u.curr_action));
    let curr_progress = Memo::new(move |_| unit_sig.with(|u| u.curr_progress));
    let last_ping = Memo::new(move |_| unit_sig.with(|u| u.last_ping_timestamp));
    let last_comment = Memo::new(move |_| {
        unit_sig.with(|u| String::from_utf8_lossy(&u.last_ping_comment).into_owned())
    });
    let stats = Memo::new(move |_| unit_sig.with(|u| u.stats.clone()));

    let hostname =
        Memo::new(move |_| hostmap.with(|h| h.get(&ip.get()).cloned().unwrap_or_default()));

    let bucket = Memo::new(move |_| PingBucket::from_ping_ago(last_ping.get(), now_secs.get()));

    let is_visible = Memo::new(move |_| {
        let mode = filter_mode.get();
        let query = search_query.get();
        if mode == FilterMode::All && query.trim().is_empty() {
            return true;
        }
        let now = now_secs.get();
        let host = hostname.get();
        unit_sig.with(|u| unit_matches_filter(u, &host, mode, &query, now))
    });

    let mac_str = mac.to_string();
    let url_flash = format!("admin/action/mac:{mac_str}/flash");
    let url_store = format!("admin/action/mac:{mac_str}/store");
    let url_boot = format!("admin/action/mac:{mac_str}/boot");
    let url_restart = format!("admin/action/mac:{mac_str}/restart");
    let url_cancel = format!("admin/action/mac:{mac_str}/wait");
    let url_register = format!("admin/action/mac:{mac_str}/register");
    let url_shutdown = format!("admin/action/mac:{mac_str}/shutdown");
    let url_forget = format!("admin/forget/mac:{mac_str}");

    let row_id = format!("unit-row-{mac_str}");

    view! {
        <tr
            id=row_id
            style:display=move || if is_visible.get() { "" } else { "none" }
            class=move || {
                if highlighted_mac.get() == Some(mac) {
                    "bg-primary/15 transition-colors"
                } else {
                    ""
                }
            }
            on:click=move |_| {
                if highlighted_mac.get_untracked() == Some(mac) {
                    highlighted_mac.set(None);
                }
            }
        >
            <td class="w-8">
                <div
                    class="tooltip tooltip-right hover:z-50"
                    data-tip=move || format!("Last ping: {}", format_ago(last_ping.get(), now_secs.get()))
                >
                    <span class=move || {
                        format!("inline-block w-3 h-3 rounded-full {}", bucket.get().dot_class())
                    }></span>
                </div>
            </td>
            <td>
                <div class="flex flex-col">
                    <span class="font-medium text-xs">
                        {move || {
                            let h = hostname.get();
                            if h.is_empty() { ip.get().to_string() } else { h }
                        }}
                    </span>
                    <span class="font-mono text-[11px] opacity-60">
                        {move || ip.get().to_string()}
                    </span>
                </div>
            </td>
            <td class="font-mono text-xs">{mac_str.clone()}</td>
            <td>
                <span class="badge badge-outline badge-sm font-mono">
                    {move || format!("R{} C{}", row.get(), col.get())}
                </span>
            </td>
            <td>
                <span class="badge badge-neutral badge-sm">{move || image.get()}</span>
            </td>
            <td>
                <div
                    class="tooltip hover:z-50"
                    data-tip=move || {
                        let s = stats.get();
                        if s.cpu.is_empty() {
                            "No CPU info".to_owned()
                        } else {
                            s.cpu
                        }
                    }
                >
                    <span class="font-mono text-xs opacity-80">
                        {move || {
                            let s = stats.get();
                            if s.ram == 0 && s.disk == 0 {
                                "—".to_owned()
                            } else {
                                format!("{} / {}", BytesFmt(s.ram), BytesFmt(s.disk))
                            }
                        }}
                    </span>
                </div>
            </td>
            <td>
                <span class=move || action_badge_class(next_action.get())>
                    {move || next_action.get().to_string()}
                </span>
            </td>
            <td class="min-w-48">
                {move || {
                    if let Some(act) = curr_action.get() {
                        if let Some((done, tot)) = curr_progress.get() {
                            let pct = (done * 100).checked_div(tot).unwrap_or(0);
                            view! {
                                <div class="flex items-center gap-2">
                                    <progress
                                        class="progress progress-info w-24 h-2"
                                        value=done
                                        max=tot.max(1)
                                    ></progress>
                                    <span class="font-mono text-xs font-medium">
                                        {format!("{act} {done}/{tot} ({pct}%)")}
                                    </span>
                                </div>
                            }
                                .into_any()
                        } else {
                            view! {
                                <span class="badge badge-info badge-sm gap-1">
                                    <span class="loading loading-spinner loading-xs"></span>
                                    {act.to_string()}
                                </span>
                            }
                                .into_any()
                        }
                    } else {
                        let comment = last_comment.get();
                        let ago = format_ago(last_ping.get(), now_secs.get());
                        view! {
                            <div class="text-xs opacity-80 truncate max-w-xs">
                                <span class="font-mono">{ago}</span>
                                {if comment.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {comment}")
                                }}
                            </div>
                        }
                            .into_any()
                    }
                }}
            </td>
            <td class="text-right whitespace-nowrap">
                <div class="join">
                    <button
                        class="btn btn-error btn-xs join-item"
                        on:click={
                            let u = url_flash.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "flash"
                    </button>
                    <button
                        class="btn btn-warning btn-xs join-item"
                        on:click={
                            let u = url_store.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "store"
                    </button>
                    <button
                        class="btn btn-success btn-xs join-item"
                        on:click={
                            let u = url_boot.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "boot"
                    </button>
                    <button
                        class="btn btn-success btn-outline btn-xs join-item"
                        on:click={
                            let u = url_restart.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "restart"
                    </button>
                    <button
                        class="btn btn-primary btn-xs join-item"
                        on:click={
                            let u = url_cancel.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "wait"
                    </button>
                    <button
                        class="btn btn-ghost btn-xs join-item"
                        on:click={
                            let u = url_register.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "re-reg"
                    </button>
                    <button
                        class="btn btn-ghost btn-xs join-item"
                        on:click={
                            let u = url_shutdown.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "off"
                    </button>
                    <button
                        class="btn btn-error btn-outline btn-xs join-item"
                        on:click={
                            let u = url_forget.clone();
                            move |_| send_req(u.clone())
                        }
                    >
                        "forget"
                    </button>
                </div>
            </td>
        </tr>
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
struct GroupSummary {
    total: usize,
    online: usize,
    stale: usize,
    active: usize,
    queued: usize,
    progress_done: usize,
    progress_total: usize,
    slowest_label: Option<String>,
}

#[component]
fn GroupSection(
    group_name: Memo<String>,
    unit_macs: Memo<Vec<MacAddr6>>,
    unit_signals: RwSignal<HashMap<MacAddr6, RwSignal<Unit>>>,
    images: Memo<Vec<String>>,
    hostmap: RwSignal<HashMap<Ipv4Addr, String>>,
    now_secs: RwSignal<i64>,
    filter_mode: RwSignal<FilterMode>,
    search_query: RwSignal<String>,
    highlighted_mac: RwSignal<Option<MacAddr6>>,
) -> impl IntoView {
    let (collapsed, set_collapsed) = signal(false);

    let summary = Memo::new(move |_| {
        let now = now_secs.get();
        let macs = unit_macs.get();
        let mut s = GroupSummary {
            total: macs.len(),
            ..Default::default()
        };
        let mut slowest_pct = usize::MAX;

        unit_signals.with(|map| {
            for mac in &macs {
                if let Some(sig) = map.get(mac) {
                    sig.with(|u| {
                        let bucket = PingBucket::from_ping_ago(u.last_ping_timestamp, now);
                        if matches!(bucket, PingBucket::Online | PingBucket::Future) {
                            s.online += 1;
                        } else if u.curr_action.is_none() {
                            s.stale += 1;
                        }
                        if u.curr_action.is_some() {
                            s.active += 1;
                            if let Some((done, tot)) = u.curr_progress
                                && tot > 0
                            {
                                s.progress_done += done;
                                s.progress_total += tot;
                                let pct = (done * 100) / tot;
                                if pct < slowest_pct {
                                    slowest_pct = pct;
                                    s.slowest_label =
                                        Some(format!("R{}C{} ({done}/{tot})", u.row, u.col));
                                }
                            }
                        } else if u.next_action != Action::Wait {
                            s.queued += 1;
                        }
                    });
                }
            }
        });
        s
    });

    let url_flash = move || format!("admin/action/group:{}/flash", group_name.get());
    let url_boot = move || format!("admin/action/group:{}/boot", group_name.get());
    let url_restart = move || format!("admin/action/group:{}/restart", group_name.get());
    let url_cancel = move || format!("admin/action/group:{}/wait", group_name.get());

    view! {
        <div class="card bg-base-100 shadow-sm border border-base-300">
            <div class="card-body p-4 gap-3">
                <div class="flex flex-wrap items-center justify-between gap-3">
                    <div class="flex flex-wrap items-center gap-2">
                        <button
                            class="btn btn-ghost btn-xs btn-square"
                            on:click=move |_| set_collapsed.update(|c| *c = !*c)
                        >
                            {move || if collapsed.get() { "▸" } else { "▾" }}
                        </button>
                        <h2 class="card-title text-lg">{move || group_name.get()}</h2>
                        <span class="badge badge-neutral badge-sm">
                            {move || format!("{} total", summary.get().total)}
                        </span>
                        <span class="badge badge-success badge-sm">
                            {move || format!("{} online", summary.get().online)}
                        </span>
                        <Show when=move || { summary.get().active > 0 }>
                            <span class="badge badge-info badge-sm">
                                {move || format!("{} active", summary.get().active)}
                            </span>
                        </Show>
                        <Show when=move || { summary.get().queued > 0 }>
                            <span class="badge badge-warning badge-sm">
                                {move || format!("{} queued", summary.get().queued)}
                            </span>
                        </Show>
                        <Show when=move || { summary.get().stale > 0 }>
                            <span class="badge badge-error badge-sm">
                                {move || format!("{} stale/offline", summary.get().stale)}
                            </span>
                        </Show>
                    </div>

                    <div class="flex flex-wrap items-center gap-2">
                        <div class="join">
                            <button
                                class="btn btn-error btn-xs join-item"
                                on:click=move |_| send_req(url_flash())
                            >
                                "Flash all"
                            </button>
                            <button
                                class="btn btn-success btn-xs join-item"
                                on:click=move |_| send_req(url_boot())
                            >
                                "Boot all"
                            </button>
                            <button
                                class="btn btn-success btn-outline btn-xs join-item"
                                on:click=move |_| send_req(url_restart())
                            >
                                "Restart all"
                            </button>
                            <button
                                class="btn btn-primary btn-xs join-item"
                                on:click=move |_| send_req(url_cancel())
                            >
                                "Wait all"
                            </button>
                        </div>

                        <div class="join">
                            <For
                                each=move || images.get()
                                key=|img| img.clone()
                                children=move |image: String| {
                                    let label = format!("Image: {image}");
                                    let url = move || {
                                        format!("admin/image/group:{}/{}", group_name.get(), image)
                                    };
                                    view! {
                                        <button
                                            class="btn btn-outline btn-xs join-item"
                                            on:click=move |_| send_req(url())
                                        >
                                            {label}
                                        </button>
                                    }
                                }
                            />
                        </div>
                    </div>
                </div>

                <Show when=move || { summary.get().progress_total > 0 }>
                    {move || {
                        let s = summary.get();
                        let pct = (s.progress_done * 100)
                            .checked_div(s.progress_total)
                            .unwrap_or(0);
                        view! {
                            <div class="flex flex-wrap items-center gap-3 bg-base-200 rounded-lg px-3 py-2 text-xs">
                                <span class="font-semibold">
                                    {format!("Active progress: {pct}% ({} PCs)", s.active)}
                                </span>
                                <progress
                                    class="progress progress-info flex-1 min-w-40 h-2.5"
                                    value=s.progress_done
                                    max=s.progress_total.max(1)
                                ></progress>
                                {s.slowest_label.map(|slowest| {
                                    view! {
                                        <span class="font-mono opacity-80">
                                            {format!("Slowest: {slowest}")}
                                        </span>
                                    }
                                })}
                            </div>
                        }
                    }}
                </Show>

                <Show when=move || !collapsed.get()>
                    <div class="overflow-x-auto">
                        <table class="table table-xs table-zebra">
                            <thead>
                                <tr>
                                    <th></th>
                                    <th>"Host / IP"</th>
                                    <th>"MAC"</th>
                                    <th>"Pos"</th>
                                    <th>"Image"</th>
                                    <th>"RAM / Disk"</th>
                                    <th>"Next"</th>
                                    <th>"Current / Ping"</th>
                                    <th class="text-right">"Actions"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=move || unit_macs.get()
                                    key=|mac| *mac
                                    children=move |mac| {
                                        let Some(unit_sig) =
                                            unit_signals.with_untracked(|m| m.get(&mac).copied())
                                        else {
                                            return ().into_any();
                                        };
                                        view! {
                                            <UnitRow
                                                mac=mac
                                                unit_sig=unit_sig
                                                hostmap=hostmap
                                                now_secs=now_secs
                                                filter_mode=filter_mode
                                                search_query=search_query
                                                highlighted_mac=highlighted_mac
                                            />
                                        }
                                            .into_any()
                                    }
                                />
                            </tbody>
                        </table>
                    </div>
                </Show>
            </div>
        </div>
    }
}

#[component]
fn App() -> impl IntoView {
    let connected = RwSignal::new(true);
    let config = RwSignal::new(None::<Config>);
    let hostmap = RwSignal::new(HashMap::<Ipv4Addr, String>::new());
    let image_stats = RwSignal::new(None::<ImagesStats>);

    // Fine-grained unit storage: each MAC has its own RwSignal<Unit> so progress/ping
    // updates only notify the single affected workstation's subscribers.
    let unit_signals = RwSignal::new(HashMap::<MacAddr6, RwSignal<Unit>>::new());
    let unit_order = RwSignal::new(Vec::<(MacAddr6, u8, Ipv4Addr)>::new());

    let filter_mode = RwSignal::new(FilterMode::All);
    let search_query = RwSignal::new(String::new());
    let highlighted_mac = RwSignal::new(None::<MacAddr6>);

    let now_secs = RwSignal::new((Date::now() * 0.001) as i64);
    spawn_local(async move {
        let mut interval = IntervalStream::new(250);
        while interval.next().await.is_some() {
            let secs = (Date::now() * 0.001) as i64;
            if now_secs.get_untracked() != secs {
                now_secs.set(secs);
            }
        }
    });

    let apply_units = move |mut incoming: Vec<Unit>| {
        incoming.sort_by_key(Unit::static_ip);
        let new_order: Vec<(MacAddr6, u8, Ipv4Addr)> = incoming
            .iter()
            .map(|u| (u.mac, u.group, u.static_ip()))
            .collect();
        let order_changed = unit_order.with_untracked(|cur| *cur != new_order);

        unit_signals.update_untracked(|map| {
            if order_changed {
                let valid: std::collections::HashSet<MacAddr6> =
                    incoming.iter().map(|u| u.mac).collect();
                map.retain(|mac, _| valid.contains(mac));
            }
            for u in incoming {
                if let Some(&sig) = map.get(&u.mac) {
                    if sig.with_untracked(|existing| existing != &u) {
                        sig.set(u);
                    }
                } else {
                    map.insert(u.mac, RwSignal::new(u));
                }
            }
        });

        if order_changed {
            unit_signals.notify();
            unit_order.set(new_order);
        }
    };

    spawn_local(async move {
        loop {
            let Ok(res) = Request::get("admin/status").send().await else {
                connected.set(false);
                TimeoutFuture::new(1_500).await;
                continue;
            };
            let Some(body) = res.body() else {
                connected.set(false);
                TimeoutFuture::new(1_500).await;
                continue;
            };
            connected.set(true);

            let js_stream = JsStream::from(body.values());
            let mut stream =
                js_stream.map(|item| item.map(|js_val| Uint8Array::new(&js_val).to_vec()));

            let mut buf = Vec::new();
            while let Ok(Some(data)) = stream.try_next().await {
                let mut slice = &data[..];
                let mut latest_units: Option<Vec<Unit>> = None;

                while let Some(newline_pos) = slice.iter().position(|&b| b == b'\n') {
                    buf.extend_from_slice(&slice[..newline_pos]);
                    if let Ok(msg) = serde_json::from_slice::<StatusUpdate>(&buf) {
                        match msg {
                            StatusUpdate::Units(u) => latest_units = Some(u),
                            StatusUpdate::Config(c) => config.set(Some(c)),
                            StatusUpdate::HostMap(h) => hostmap.set(h),
                            StatusUpdate::ImagesStats(i) => image_stats.set(Some(i)),
                        }
                    }
                    buf.clear();
                    slice = &slice[newline_pos + 1..];
                }
                buf.extend_from_slice(slice);

                if let Some(u) = latest_units {
                    apply_units(u);
                }
            }

            connected.set(false);
            TimeoutFuture::new(1_500).await;
        }
    });

    let images = Memo::new(move |_| {
        config.with(|c| c.as_ref().map(|x| x.images.clone()).unwrap_or_default())
    });

    let group_ids = Memo::new(move |_| {
        config.with(|c| {
            c.as_ref()
                .map(|cfg| cfg.groups.iter().map(|(_, id)| *id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });

    let global_summary = Memo::new(move |_| {
        let now = now_secs.get();
        let mut s = GroupSummary::default();
        unit_signals.with(|map| {
            s.total = map.len();
            for sig in map.values() {
                sig.with(|u| {
                    let bucket = PingBucket::from_ping_ago(u.last_ping_timestamp, now);
                    if matches!(bucket, PingBucket::Online | PingBucket::Future) {
                        s.online += 1;
                    } else if u.curr_action.is_none() {
                        s.stale += 1;
                    }
                    if u.curr_action.is_some() {
                        s.active += 1;
                    } else if u.next_action != Action::Wait {
                        s.queued += 1;
                    }
                });
            }
        });
        s
    });

    view! {
        <div class="min-h-screen flex flex-col">
            <header class="navbar bg-base-100 border-b border-base-300 sticky top-0 z-30 px-4 gap-4 flex-wrap">
                <div class="flex items-center gap-3">
                    <span class="text-xl font-bold tracking-tight">"Pixie"</span>
                    <Show
                        when=move || connected.get()
                        fallback=|| {
                            view! {
                                <span class="badge badge-error badge-sm gap-1">
                                    "Disconnected (reconnecting…)"
                                </span>
                            }
                        }
                    >
                        <span class="badge badge-success badge-sm gap-1">"Connected"</span>
                    </Show>
                    <div class="hidden sm:flex items-center gap-1.5 text-xs">
                        <span class="badge badge-neutral badge-sm">
                            {move || format!("{} PCs", global_summary.get().total)}
                        </span>
                        <span class="badge badge-success badge-sm">
                            {move || format!("{} online", global_summary.get().online)}
                        </span>
                        <Show when=move || { global_summary.get().active > 0 }>
                            <span class="badge badge-info badge-sm">
                                {move || format!("{} active", global_summary.get().active)}
                            </span>
                        </Show>
                        <Show when=move || { global_summary.get().queued > 0 }>
                            <span class="badge badge-warning badge-sm">
                                {move || format!("{} queued", global_summary.get().queued)}
                            </span>
                        </Show>
                        <Show when=move || { global_summary.get().stale > 0 }>
                            <span class="badge badge-error badge-sm">
                                {move || format!("{} stale", global_summary.get().stale)}
                            </span>
                        </Show>
                    </div>
                </div>

                <div class="flex flex-wrap items-center gap-2 ml-auto">
                    <div class="join">
                        <button
                            class=move || {
                                if filter_mode.get() == FilterMode::All {
                                    "btn btn-xs btn-active btn-primary join-item"
                                } else {
                                    "btn btn-xs join-item"
                                }
                            }
                            on:click=move |_| filter_mode.set(FilterMode::All)
                        >
                            {move || format!("All ({})", global_summary.get().total)}
                        </button>
                        <button
                            class=move || {
                                if filter_mode.get() == FilterMode::ActiveOrQueued {
                                    "btn btn-xs btn-active btn-info join-item"
                                } else {
                                    "btn btn-xs join-item"
                                }
                            }
                            on:click=move |_| filter_mode.set(FilterMode::ActiveOrQueued)
                        >
                            {move || {
                                let s = global_summary.get();
                                format!("Active / Queued ({})", s.active + s.queued)
                            }}
                        </button>
                        <button
                            class=move || {
                                if filter_mode.get() == FilterMode::StaleOrOffline {
                                    "btn btn-xs btn-active btn-error join-item"
                                } else {
                                    "btn btn-xs join-item"
                                }
                            }
                            on:click=move |_| filter_mode.set(FilterMode::StaleOrOffline)
                        >
                            {move || format!("Stale / Offline ({})", global_summary.get().stale)}
                        </button>
                    </div>

                    <input
                        type="search"
                        placeholder="Search host, IP, MAC, R1C2…"
                        class="input input-xs input-bordered w-48 sm:w-64"
                        prop:value=move || search_query.get()
                        on:input=move |ev| search_query.set(event_target_value(&ev))
                    />
                </div>
            </header>

            <main class="flex-1 max-w-[1600px] w-full mx-auto p-4 flex flex-col gap-4">
                <ImagesSection images=image_stats />

                <div class="flex flex-wrap gap-3">
                    <For
                        each=move || group_ids.get()
                        key=|id| *id
                        children=move |id: u8| {
                            let group_name = Memo::new(move |_| {
                                config.with(|c| {
                                    c.as_ref()
                                        .and_then(|cfg| cfg.groups.get_by_second(&id))
                                        .cloned()
                                        .unwrap_or_else(|| format!("Group {id}"))
                                })
                            });
                            let unit_macs = Memo::new(move |_| {
                                unit_order.with(|list| {
                                    list.iter()
                                        .filter(|(_, g, _)| *g == id)
                                        .map(|(mac, _, _)| *mac)
                                        .collect::<Vec<_>>()
                                })
                            });
                            view! {
                                <RoomMapCard
                                    group_id=id
                                    group_name=group_name
                                    unit_macs=unit_macs
                                    unit_signals=unit_signals
                                    hostmap=hostmap
                                    now_secs=now_secs
                                    highlighted_mac=highlighted_mac
                                />
                            }
                        }
                    />
                </div>

                <For
                    each=move || group_ids.get()
                    key=|id| *id
                    children=move |id: u8| {
                        let group_name = Memo::new(move |_| {
                            config.with(|c| {
                                c.as_ref()
                                    .and_then(|cfg| cfg.groups.get_by_second(&id))
                                    .cloned()
                                    .unwrap_or_else(|| format!("Group {id}"))
                            })
                        });
                        let unit_macs = Memo::new(move |_| {
                            unit_order.with(|list| {
                                list.iter()
                                    .filter(|(_, g, _)| *g == id)
                                    .map(|(mac, _, _)| *mac)
                                    .collect::<Vec<_>>()
                            })
                        });
                        view! {
                            <GroupSection
                                group_name=group_name
                                unit_macs=unit_macs
                                unit_signals=unit_signals
                                images=images
                                hostmap=hostmap
                                now_secs=now_secs
                                filter_mode=filter_mode
                                search_query=search_query
                                highlighted_mac=highlighted_mac
                            />
                        }
                    }
                />
            </main>
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Debug).unwrap();

    leptos::mount::mount_to_body(App);
}
