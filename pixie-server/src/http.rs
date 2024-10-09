use std::{net::Ipv4Addr, sync::Arc};

use anyhow::Result;
use axum::{
    body::Body,
    extract::{self, Path},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::StreamExt;
use macaddr::MacAddr6;

use pixie_shared::{HttpConfig, StatusUpdate, Unit};
use tokio::net::TcpListener;
use tokio_stream::wrappers::WatchStream;
use tower_http::{
    services::ServeDir, trace::TraceLayer, validate_request::ValidateRequestHeaderLayer,
};

use crate::state::State;

enum UnitSelector {
    MacAddr(MacAddr6),
    IpAddr(Ipv4Addr),
    All,
    Group(u8),
    Image(String),
}

impl UnitSelector {
    fn parse(state: &State, selector: String) -> Option<UnitSelector> {
        if let Ok(mac) = selector.parse::<MacAddr6>() {
            Some(UnitSelector::MacAddr(mac))
        } else if let Ok(ip) = selector.parse::<Ipv4Addr>() {
            Some(UnitSelector::IpAddr(ip))
        } else if selector == "all" {
            Some(UnitSelector::All)
        } else if let Some(&group) = state.config.groups.get_by_first(&selector) {
            Some(UnitSelector::Group(group))
        } else if state.config.images.contains(&selector) {
            Some(UnitSelector::Image(selector))
        } else {
            None
        }
    }

    fn select(&self, unit: &Unit) -> bool {
        match self {
            UnitSelector::MacAddr(mac) => unit.mac == *mac,
            UnitSelector::IpAddr(ip) => unit.static_ip() == *ip,
            UnitSelector::All => true,
            UnitSelector::Group(group) => unit.group == *group,
            UnitSelector::Image(image) => unit.image == *image,
        }
    }
}

async fn action(
    Path((unit_filter, action_name)): Path<(String, String)>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    let Ok(action) = action_name.parse() else {
        return (
            StatusCode::BAD_REQUEST,
            format!("Unknown action: {:?}\n", action_name),
        );
    };

    let Some(unit_selector) = UnitSelector::parse(&state, unit_filter) else {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid unit selector\n".to_owned(),
        );
    };

    let mut updated = 0;
    state.units.send_if_modified(|units| {
        for unit in units.iter_mut() {
            if unit_selector.select(unit) {
                unit.next_action = action;
                updated += 1;
            }
        }
        updated > 0
    });

    if updated > 0 {
        (StatusCode::OK, format!("{updated} computer(s) affected\n"))
    } else {
        (StatusCode::BAD_REQUEST, "Unknown PC\n".to_owned())
    }
}

async fn image(
    Path((unit_filter, image)): Path<(String, String)>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    if !state.config.images.contains(&image) {
        return (
            StatusCode::BAD_REQUEST,
            format!("Unknown image: {:?}\n", image),
        );
    }

    let Some(unit_selector) = UnitSelector::parse(&state, unit_filter) else {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid unit selector\n".to_owned(),
        );
    };

    let mut updated = 0;
    state.units.send_if_modified(|units| {
        for unit in units.iter_mut() {
            if unit_selector.select(unit) {
                unit.image = image.clone();
                updated += 1;
            }
        }
        updated > 0
    });

    if updated > 0 {
        (StatusCode::OK, format!("{updated} computer(s) affected\n"))
    } else {
        (StatusCode::BAD_REQUEST, "Unknown PC\n".to_owned())
    }
}

async fn forget(
    Path(unit_filter): Path<String>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    let Some(unit_selector) = UnitSelector::parse(&state, unit_filter) else {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid unit selector\n".to_owned(),
        );
    };

    let mut updated = 0;
    state.units.send_if_modified(|units| {
        let len_before = units.len();
        units.retain(|unit| !unit_selector.select(unit));
        updated = len_before - units.len();
        updated > 0
    });

    if updated > 0 {
        (StatusCode::OK, format!("{updated} computer(s) removed\n"))
    } else {
        (StatusCode::BAD_REQUEST, "Unknown PC\n".to_owned())
    }
}

async fn gc(extract::State(state): extract::State<Arc<State>>) -> impl IntoResponse {
    state.gc_chunks().unwrap();
    "Garbage collection completed\n"
}

async fn status(extract::State(state): extract::State<Arc<State>>) -> impl IntoResponse {
    let initial_messages = [StatusUpdate::Config(state.config.clone())];

    let units_rx = WatchStream::new(state.units.subscribe());
    let image_rx = WatchStream::new(state.image_stats.subscribe());
    let hostmap_rx = WatchStream::new(state.hostmap.subscribe());

    let messages = futures::stream::iter(initial_messages).chain(futures::stream::select(
        futures::stream::select(
            image_rx.map(StatusUpdate::ImageStats),
            units_rx.map(StatusUpdate::Units),
        ),
        hostmap_rx.map(StatusUpdate::HostMap),
    ));
    let lines = messages.map(|msg| serde_json::to_string(&msg).map(|x| x + "\n"));

    let mut res = Response::new(Body::from_stream(lines));
    res.headers_mut()
        .insert("Content-Type", "application/json".parse().unwrap());
    res
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let HttpConfig {
        listen_on,
        ref password,
    } = state.config.http;

    let admin_path = state.storage_dir.join("admin");

    let router = Router::new()
        .route("/admin/status", get(status))
        .route("/admin/gc", get(gc))
        .route("/admin/action/:unit/:action", get(action))
        .route("/admin/image/:unit/:image", get(image))
        .route("/admin/forget/:unit", get(forget))
        .nest_service(
            "/",
            ServeDir::new(&admin_path).append_index_html_on_directories(true),
        )
        .layer(ValidateRequestHeaderLayer::basic("admin", password))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(listen_on).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
