//! Projects and their policies.

use super::{ApiJson, ApiPath, validate_project_name};
use crate::auth::{Admin, Viewer};
use crate::error::{ApiResult, Failure};
use crate::state::App;
use axum::Json;
use axum::extract::State;
use bella_core::api::{ProjectPolicy, ProjectView};
use tracing::info;

/// `GET /projects`.
pub(crate) async fn list(
    State(app): State<App>,
    _viewer: Viewer,
) -> ApiResult<Json<Vec<ProjectView>>> {
    Ok(Json(app.db(|ledger| ledger.projects()).await?))
}

/// `PUT /projects/{name}`: set a project's policy, creating the project if
/// needed. Administrators only; the ledger checks the role again inside its
/// own transaction.
pub(crate) async fn update(
    State(app): State<App>,
    Admin(admin): Admin,
    ApiPath(name): ApiPath,
    ApiJson(policy): ApiJson<ProjectPolicy>,
) -> ApiResult<Json<ProjectView>> {
    validate_project_name(&name)?;
    if policy.weight == 0 {
        return Err(Failure::bad_request(
            "a project's fair-share weight must be at least 1",
        ));
    }
    let actor = admin.id.clone();
    let view = app
        .db(move |ledger| ledger.update_project_policy(&actor, &name, &policy))
        .await?;
    app.changed();
    info!(project_id = %view.id, changed_by = %admin.id, "project policy set");
    Ok(Json(view))
}
