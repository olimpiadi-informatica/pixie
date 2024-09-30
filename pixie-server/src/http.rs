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

use pixie_shared::{HttpConfig, StatusUpdate};
use tokio::net::TcpListener;
use tokio_stream::wrappers::WatchStream;
use tower_http::{
    services::ServeDir, trace::TraceLayer, validate_request::ValidateRequestHeaderLayer,
};

use crate::state::State;

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

    let mut updated = 0usize;

    state.units.send_if_modified(|units| {
        if let Ok(mac) = unit_filter.parse::<MacAddr6>() {
            for unit in units.iter_mut() {
                if unit.mac == mac {
                    unit.next_action = action;
                    updated += 1;
                }
            }
        } else if let Ok(ip) = unit_filter.parse::<Ipv4Addr>() {
            for unit in units.iter_mut() {
                if unit.static_ip() == ip {
                    unit.next_action = action;
                    updated += 1;
                }
            }
        } else if unit_filter == "all" {
            for unit in units.iter_mut() {
                unit.next_action = action;
                updated += 1;
            }
        } else if let Some(&group) = state.config.groups.get_by_first(&unit_filter) {
            for unit in units.iter_mut() {
                if unit.group == group {
                    unit.next_action = action;
                    updated += 1;
                }
            }
        } else if state.config.images.contains(&unit_filter) {
            for unit in units.iter_mut() {
                if unit.image == unit_filter {
                    unit.next_action = action;
                    updated += 1;
                }
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

    let mut updated = 0usize;

    state.units.send_if_modified(|units| {
        if let Ok(mac) = unit_filter.parse::<MacAddr6>() {
            for unit in units.iter_mut() {
                if unit.mac == mac {
                    unit.image = image.clone();
                    updated += 1;
                }
            }
        } else if let Ok(ip) = unit_filter.parse::<Ipv4Addr>() {
            for unit in units.iter_mut() {
                if unit.static_ip() == ip {
                    unit.image = image.clone();
                    updated += 1;
                }
            }
        } else if unit_filter == "all" {
            for unit in units.iter_mut() {
                unit.image = image.clone();
                updated += 1;
            }
        } else if let Some(&group) = state.config.groups.get_by_first(&unit_filter) {
            for unit in units.iter_mut() {
                if unit.group == group {
                    unit.image = image.clone();
                    updated += 1;
                }
            }
        } else if state.config.images.contains(&unit_filter) {
            for unit in units.iter_mut() {
                if unit.image == unit_filter {
                    unit.image = image.clone();
                    updated += 1;
                }
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

async fn gc(extract::State(state): extract::State<Arc<State>>) -> String {
    state.gc_chunks().unwrap();
    "".to_owned()
}

async fn status(extract::State(state): extract::State<Arc<State>>) -> impl IntoResponse {
    let initial_messages = vec![
        StatusUpdate::Config(state.config.clone()),
        StatusUpdate::HostMap(state.hostmap.clone()),
    ];
    let mut units_rx = state.units.subscribe();
    units_rx.mark_changed();
    let units_rx = WatchStream::new(units_rx);

    let mut image_rx = state.image_stats.subscribe();
    image_rx.mark_changed();
    let image_rx = WatchStream::new(image_rx);

    let messages =
        futures::stream::iter(initial_messages.into_iter()).chain(futures::stream::select(
            image_rx.map(StatusUpdate::ImageStats),
            units_rx.map(StatusUpdate::Units),
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
