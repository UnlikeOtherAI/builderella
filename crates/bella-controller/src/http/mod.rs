//! The REST API under `/api/v1`: routing, request decoding and the response
//! headers every API answer carries.

mod environments;
mod events;
mod fleet;
mod identity;
mod jobs;
mod objects;
mod projects;
mod snapshots;

use crate::auth::Reader;
use crate::dashboard;
use crate::error::{ApiResult, Failure};
use crate::state::App;
use crate::worker;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request};
use axum::http::header::{CACHE_CONTROL, X_CONTENT_TYPE_OPTIONS};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bella_core::ErrorCode;
use bella_core::api::API_PREFIX;
use bella_core::protocol::PROTOCOL_VERSION;
use serde::de::DeserializeOwned;
use std::str::FromStr;

/// The version this controller reports in `GET /version` and `GET /fleet`.
pub(crate) const CONTROLLER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A JSON request body. A malformed or oversized body is answered with the
/// API's own error shape.
pub(crate) struct ApiJson<T>(pub T);

impl<T: DeserializeOwned> FromRequest<App> for ApiJson<T> {
    type Rejection = Failure;

    async fn from_request(request: Request, app: &App) -> Result<Self, Failure> {
        match Json::<T>::from_request(request, app).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(json_failure(&rejection)),
        }
    }
}

fn json_failure(rejection: &JsonRejection) -> Failure {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        Failure::new(ErrorCode::TooLarge, "the request body is too large")
    } else {
        Failure::bad_request(rejection.body_text())
    }
}

/// Path parameters, as text. Handlers parse identifiers themselves so a
/// malformed one is reported in the API's error shape.
pub(crate) struct ApiPath(pub String);

impl FromRequestParts<App> for ApiPath {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Failure> {
        match axum::extract::Path::<String>::from_request_parts(parts, app).await {
            Ok(axum::extract::Path(value)) => Ok(ApiPath(value)),
            Err(rejection) => Err(path_failure(&rejection)),
        }
    }
}

fn path_failure(rejection: &PathRejection) -> Failure {
    Failure::bad_request(rejection.body_text())
}

/// Query parameters.
pub(crate) struct ApiQuery<T>(pub T);

impl<T: DeserializeOwned> FromRequestParts<App> for ApiQuery<T> {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Failure> {
        match axum::extract::Query::<T>::from_request_parts(parts, app).await {
            Ok(axum::extract::Query(value)) => Ok(ApiQuery(value)),
            Err(rejection) => Err(query_failure(&rejection)),
        }
    }
}

fn query_failure(rejection: &QueryRejection) -> Failure {
    Failure::bad_request(rejection.body_text())
}

/// Parse an identifier from a path segment.
pub(crate) fn parse_id<T>(text: &str) -> ApiResult<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    text.parse()
        .map_err(|error: T::Err| Failure::bad_request(error.to_string()))
}

/// Project names are labels chosen by people: anything printable, of a
/// length that fits a table.
pub(crate) fn validate_project_name(name: &str) -> ApiResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed != name {
        return Err(Failure::bad_request(
            "a project name must not be empty or padded with whitespace",
        ));
    }
    if name.chars().count() > 128 || name.chars().any(char::is_control) {
        return Err(Failure::bad_request(
            "a project name has at most 128 characters and no control characters",
        ));
    }
    Ok(())
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn version(_caller: Reader) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": CONTROLLER_VERSION,
        "protocol_version": PROTOCOL_VERSION,
        "api_prefix": API_PREFIX,
    }))
}

async fn no_such_route() -> Failure {
    Failure::not_found("the route")
}

/// API answers are never cached and never sniffed.
async fn api_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

pub(crate) fn router(app: App) -> Router {
    let small = DefaultBodyLimit::max(app.tuning.max_json_bytes);
    let large = DefaultBodyLimit::max(app.tuning.max_manifest_json_bytes);
    let api = Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/whoami", get(identity::whoami))
        .route("/tokens", post(identity::create_token))
        .route("/enroll/pairing-codes", post(identity::create_pairing_code))
        .route("/enroll", post(identity::enroll))
        .route("/worker/ws", get(worker::upgrade))
        .route(
            "/objects/missing",
            post(objects::missing).layer(large.clone()),
        )
        .route(
            "/objects/{digest}",
            put(objects::upload)
                .get(objects::download)
                .head(objects::head)
                // Object bodies are streamed and limited by the store.
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/snapshots", post(snapshots::register).layer(large))
        .route("/snapshots/{id}", get(snapshots::show))
        .route("/projects", get(projects::list))
        .route("/projects/{name}", put(projects::update))
        .route("/plan", post(jobs::plan))
        .route("/jobs", post(jobs::submit).get(jobs::list))
        .route("/jobs/{id}", get(jobs::show))
        .route("/jobs/{id}/wait", get(jobs::wait))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route("/jobs/{id}/logs", get(jobs::logs))
        .route("/jobs/{id}/artifacts", get(jobs::artifacts))
        .route("/jobs/{id}/explain", get(jobs::explain))
        .route("/environments", get(environments::list))
        .route("/environments/{id}", get(environments::show))
        .route("/environments/{id}/renew", post(environments::renew))
        .route("/environments/{id}/finish", post(environments::finish))
        .route("/fleet", get(fleet::overview))
        .route(
            "/machines/{id}",
            get(fleet::machine).patch(fleet::update_machine),
        )
        .route("/queue", get(fleet::queue))
        .route("/events", get(events::stream))
        .fallback(no_such_route)
        .method_not_allowed_fallback(no_such_route)
        .layer(small)
        .layer(axum::middleware::from_fn(api_headers));

    Router::new()
        .nest(API_PREFIX, api)
        .route("/healthz", get(healthz))
        .route("/", get(dashboard::index))
        .route("/index.html", get(dashboard::index))
        .fallback(dashboard::asset)
        .with_state(app)
}
