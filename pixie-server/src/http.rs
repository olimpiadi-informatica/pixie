use crate::state::{State, UnitSelector};
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
use pixie_shared::{Action, HttpConfig, StatusUpdate};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::WatchStream;
use tower_http::{
    services::ServeDir, trace::TraceLayer, validate_request::ValidateRequestHeaderLayer,
};

async fn action(
    Path((unit_filter, action)): Path<(String, Action)>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    let Some(unit_selector) = UnitSelector::parse(&state, unit_filter) else {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid unit selector\n".to_owned(),
        );
    };

    let updated = state.set_unit_next_action(unit_selector, action);
    if updated > 0 {
        (StatusCode::OK, format!("{updated} computer(s) affected\n"))
    } else {
        (StatusCode::BAD_REQUEST, "Unknown PC\n".to_owned())
    }
}

async fn curr_action(
    Path((unit_filter, action)): Path<(String, Action)>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    let Some(unit_selector) = UnitSelector::parse(&state, unit_filter) else {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid unit selector\n".to_owned(),
        );
    };

    let updated = state.set_unit_current_action(unit_selector, action);
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

    match state.set_unit_image(unit_selector, image) {
        Ok(updated @ 1..) => (StatusCode::OK, format!("{updated} computer(s) affected\n")),
        Ok(0) => (StatusCode::BAD_REQUEST, "Unknown PC\n".to_owned()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {e}\n")),
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

    let updated = state.forget_unit(unit_selector);
    if updated > 0 {
        (StatusCode::OK, format!("{updated} computer(s) removed\n"))
    } else {
        (StatusCode::BAD_REQUEST, "Unknown PC\n".to_owned())
    }
}

async fn rollback(
    Path(image): Path<String>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    match state.rollback_image(&image) {
        Ok(()) => (StatusCode::NO_CONTENT, String::new()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{}\n", e)),
    }
}

async fn delete_image(
    Path(image): Path<String>,
    extract::State(state): extract::State<Arc<State>>,
) -> impl IntoResponse {
    match state.delete_image(&image) {
        Ok(()) => (StatusCode::NO_CONTENT, String::new()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")),
    }
}

async fn gc(extract::State(state): extract::State<Arc<State>>) -> impl IntoResponse {
    match state.gc_chunks() {
        Ok(()) => (StatusCode::NO_CONTENT, String::new()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")),
    }
}

async fn status(extract::State(state): extract::State<Arc<State>>) -> impl IntoResponse {
    let initial_messages = [StatusUpdate::Config(state.config.clone())];

    let units_rx = WatchStream::new(state.subscribe_units());
    let image_rx = WatchStream::new(state.subscribe_images());
    let hostmap_rx = WatchStream::new(state.subscribe_hostmap());

    let messages = futures::stream::iter(initial_messages).chain(futures::stream::select(
        futures::stream::select(
            image_rx.map(StatusUpdate::ImagesStats),
            units_rx.map(StatusUpdate::Units),
        ),
        hostmap_rx.map(StatusUpdate::HostMap),
    ));
    let lines = messages.map(|msg| serde_json::to_string(&msg).map(|x| x + "\n"));

    Response::builder()
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(lines))
        .unwrap()
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let HttpConfig {
        listen_on,
        ref password,
    } = state.config.http;

    let admin_path = state.storage_dir.join("admin");

    let mut router = Router::new()
        .route("/admin/status", get(status))
        .route("/admin/gc", get(gc))
        .route("/admin/action/:unit/:action", get(action))
        .route("/admin/curr_action/:unit/:action", get(curr_action))
        .route("/admin/image/:unit/:image", get(image))
        .route("/admin/forget/:unit", get(forget))
        .route("/admin/rollback/:image", get(rollback))
        .route("/admin/delete/:image", get(delete_image))
        .nest_service(
            "/",
            ServeDir::new(&admin_path).append_index_html_on_directories(true),
        );
    if let Some(password) = password {
        router = router.layer(ValidateRequestHeaderLayer::basic("admin", password));
    }
    router = router.layer(TraceLayer::new_for_http());

    let listener = TcpListener::bind(listen_on).await?;
    axum::serve(listener, router.with_state(state)).await?;

    Ok(())
}
