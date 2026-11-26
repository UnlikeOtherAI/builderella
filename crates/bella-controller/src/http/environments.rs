//! Retained environments: list, read, renew, finish.

use super::{ApiPath, ApiQuery, parse_id};
use crate::auth::{Developer, Viewer};
use crate::error::ApiResult;
use crate::state::App;
use crate::sweeps::finish_environment;
use axum::Json;
use axum::extract::State;
use bella_core::api::EnvironmentView;
use bella_core::ids::EnvironmentId;
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ListQuery {
    /// `true`: also list environments that have ended.
    #[serde(default)]
    include_ended: bool,
}

/// `GET /environments`: environments that have not ended.
pub(crate) async fn list(
    State(app): State<App>,
    _viewer: Viewer,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<EnvironmentView>>> {
    Ok(Json(
        app.db(move |ledger| ledger.environments(query.include_ended))
            .await?,
    ))
}

/// `GET /environments/{id}`.
pub(crate) async fn show(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<EnvironmentView>> {
    let id: EnvironmentId = parse_id(&id)?;
    Ok(Json(app.db(move |ledger| ledger.environment(&id)).await?))
}

/// `POST /environments/{id}/renew`: extend the lease from now, never past
/// its hard deadline. A renewal that is refused changes nothing: the
/// deadline that was in force stays in force.
pub(crate) async fn renew(
    State(app): State<App>,
    Developer(principal): Developer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<EnvironmentView>> {
    let id: EnvironmentId = parse_id(&id)?;
    let wanted = id.clone();
    let view = app
        .db(move |ledger| ledger.renew_environment(&wanted))
        .await?;
    app.events.touch();
    info!(
        environment_id = %id,
        job_id = %view.job_id,
        principal_id = %principal.id,
        effective_deadline = view.effective_deadline.0,
        "environment renewed"
    );
    Ok(Json(view))
}

/// `POST /environments/{id}/finish`: ask the worker to end the environment.
/// Its resources stay reserved until the worker proves the cleanup.
pub(crate) async fn finish(
    State(app): State<App>,
    Developer(principal): Developer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<EnvironmentView>> {
    let id: EnvironmentId = parse_id(&id)?;
    let reason = format!("finished by {}", principal.name);
    Ok(Json(finish_environment(&app, &id, &reason).await?))
}
