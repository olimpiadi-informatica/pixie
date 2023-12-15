use std::{net::Ipv4Addr, sync::Arc};

use anyhow::Result;
use axum::{
    extract::{self, Path},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use macaddr::MacAddr6;

use pixie_shared::HttpConfig;
use tokio::net::TcpListener;
use tower_http::{
    compression::CompressionLayer, services::ServeDir, trace::TraceLayer,
    validate_request::ValidateRequestHeaderLayer,
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

async fn get_config(extract::State(state): extract::State<Arc<State>>) -> String {
    serde_json::to_string(&state.config).unwrap()
}

async fn get_hostmap(extract::State(state): extract::State<Arc<State>>) -> String {
    serde_json::to_string(&state.hostmap).unwrap()
}

async fn get_units(extract::State(state): extract::State<Arc<State>>) -> String {
    let units = state.units.borrow();
    serde_json::to_string(&*units).unwrap()
}

async fn get_images(extract::State(state): extract::State<Arc<State>>) -> String {
    let image_stats = state.image_stats.borrow();
    serde_json::to_string(&*image_stats).unwrap()
}

async fn gc(extract::State(state): extract::State<Arc<State>>) -> String {
    state.gc_chunks().await.unwrap();
    "".to_owned()
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let HttpConfig {
        listen_on,
        ref password,
    } = state.config.http;

    let admin_path = state.storage_dir.join("admin");

    let router = Router::new()
        .route("/admin/config", get(get_config))
        .route("/admin/hostmap", get(get_hostmap))
        .route("/admin/units", get(get_units))
        .route("/admin/images", get(get_images))
        .route("/admin/gc", get(gc))
        .route("/admin/action/:unit/:action", get(action))
        .route("/admin/image/:unit/:image", get(image))
        .nest_service(
            "/",
            ServeDir::new(&admin_path).append_index_html_on_directories(true),
        )
        .layer(CompressionLayer::new())
        .layer(ValidateRequestHeaderLayer::basic(&"admin", &password))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(listen_on).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
