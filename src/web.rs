//! Admin web service (no auth, intended for LAN-only use). Serves a single
//! embedded HTML page plus a small JSON API for managing devices and viewing
//! activity.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::activity::Activity;
use crate::bridge::{DeviceCommand, Manager};

const INDEX_HTML: &str = include_str!("web/index.html");

#[derive(Clone)]
pub struct WebState {
    pub manager: Arc<Manager>,
    pub activity: Activity,
}

/// Wraps `anyhow::Error` into a JSON HTTP error response.
struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": format!("{:#}", self.0) }));
        (StatusCode::BAD_REQUEST, body).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/info", get(info))
        .route("/api/devices", get(list_devices).post(add_device))
        .route(
            "/api/devices/:id",
            axum::routing::put(edit_device).delete(remove_device),
        )
        .route("/api/devices/:id/command", axum::routing::post(command))
        .route("/api/activity", get(activity))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn info(State(state): State<WebState>) -> impl IntoResponse {
    Json(state.manager.info().await)
}

async fn list_devices(State(state): State<WebState>) -> impl IntoResponse {
    Json(state.manager.list().await)
}

#[derive(Deserialize)]
struct AddReq {
    ip: String,
    name: Option<String>,
}

async fn add_device(
    State(state): State<WebState>,
    Json(req): Json<AddReq>,
) -> Result<impl IntoResponse, AppError> {
    let view = state.manager.add_device(req.ip.trim(), req.name).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Deserialize)]
struct EditReq {
    ip: Option<String>,
    name: Option<String>,
}

async fn edit_device(
    State(state): State<WebState>,
    Path(id): Path<u64>,
    Json(req): Json<EditReq>,
) -> Result<impl IntoResponse, AppError> {
    let view = state
        .manager
        .edit_device(id, req.ip.map(|s| s.trim().to_string()), req.name)
        .await?;
    Ok(Json(view))
}

async fn remove_device(
    State(state): State<WebState>,
    Path(id): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    state.manager.remove_device(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn command(
    State(state): State<WebState>,
    Path(id): Path<u64>,
    Json(cmd): Json<DeviceCommand>,
) -> Result<impl IntoResponse, AppError> {
    state.manager.command(id, cmd).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ActivityQuery {
    limit: Option<usize>,
}

async fn activity(
    State(state): State<WebState>,
    Query(q): Query<ActivityQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(200).min(1000);
    Json(state.activity.recent(limit))
}
