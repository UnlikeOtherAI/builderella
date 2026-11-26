//! Who am I, tokens, pairing codes and enrollment.

use super::ApiJson;
use crate::auth::{Admin, Viewer, role_name};
use crate::error::{ApiResult, Failure};
use crate::server::PeerAddr;
use crate::state::{App, lock};
use axum::extract::State;
use axum::{Extension, Json};
use bella_core::ErrorCode;
use bella_core::api::{
    CreatePairingCodeRequest, CreateTokenRequest, EnrollRequest, EnrollResponse,
    PairingCodeResponse, TokenResponse, WhoAmI,
};
use bella_net::{Enrollment, PairingCode, Token, TokenKind};
use std::time::Duration;
use tracing::info;

pub(crate) async fn whoami(State(app): State<App>, Viewer(principal): Viewer) -> Json<WhoAmI> {
    Json(WhoAmI {
        fleet_id: app.fleet_id.clone(),
        principal_id: principal.id,
        name: principal.name,
        role: principal.role,
    })
}

fn validate_label(what: &str, name: &str) -> ApiResult<()> {
    if name.trim().is_empty() {
        return Err(Failure::bad_request(format!("{what} must not be empty")));
    }
    if name.chars().count() > 128 || name.chars().any(char::is_control) {
        return Err(Failure::bad_request(format!(
            "{what} has at most 128 characters and no control characters"
        )));
    }
    Ok(())
}

/// `POST /tokens`: mint a token for a new principal. The token is returned
/// once; only its hash is stored.
pub(crate) async fn create_token(
    State(app): State<App>,
    Admin(admin): Admin,
    ApiJson(request): ApiJson<CreateTokenRequest>,
) -> ApiResult<Json<TokenResponse>> {
    validate_label("a principal's name", &request.name)?;
    let (token, hash) = Token::generate(TokenKind::for_role(request.role))
        .map_err(|error| Failure::internal(format!("no random bytes for a token: {error}")))?;
    let hash = hash.to_hex();
    let (name, role) = (request.name, request.role);
    let principal = app
        .db(move |ledger| ledger.create_principal(&name, role, &hash))
        .await?;
    app.events.touch();
    info!(
        principal_id = %principal.id,
        role = role_name(principal.role),
        created_by = %admin.id,
        "principal created"
    );
    Ok(Json(TokenResponse {
        principal_id: principal.id,
        token,
    }))
}

/// `POST /enroll/pairing-codes`: a single-use, short-lived code for one
/// machine, with the string a worker enrolls from.
pub(crate) async fn create_pairing_code(
    State(app): State<App>,
    Admin(admin): Admin,
    ApiJson(request): ApiJson<CreatePairingCodeRequest>,
) -> ApiResult<Json<PairingCodeResponse>> {
    let ttl = match request.ttl_secs {
        None => app.tuning.pairing_code_ttl,
        Some(0) => {
            return Err(Failure::bad_request(
                "a pairing code must be valid for at least one second",
            ));
        }
        Some(secs) => Duration::from_secs(secs),
    };
    if ttl > app.tuning.max_pairing_code_ttl {
        return Err(Failure::bad_request(format!(
            "a pairing code is valid for at most {} seconds",
            app.tuning.max_pairing_code_ttl.as_secs()
        )));
    }
    let code = PairingCode::generate().map_err(|error| {
        Failure::internal(format!("no random bytes for a pairing code: {error}"))
    })?;
    let hash = code.hash().to_hex();
    let display = code.to_string();
    let enrollment = Enrollment::new(app.public_url.clone(), app.fingerprint, code)
        .map_err(|error| Failure::internal(format!("cannot build the enrollment string: {error}")))?
        .to_string();
    let expires_at = app.now().plus(ttl);
    let (profile, creator) = (request.profile, admin.id.clone());
    app.db(move |ledger| ledger.create_pairing_code(&hash, profile, expires_at, Some(&creator)))
        .await?;
    app.events.touch();
    info!(created_by = %admin.id, ?profile, expires_at = expires_at.0, "pairing code created");
    Ok(Json(PairingCodeResponse {
        code: display,
        expires_at,
        enrollment,
    }))
}

/// `POST /enroll`: exchange a pairing code for a machine identity. The only
/// route that takes a secret without a bearer token, so it is rate limited
/// per source address and fleet-wide before the code is looked at.
pub(crate) async fn enroll(
    State(app): State<App>,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    ApiJson(request): ApiJson<EnrollRequest>,
) -> ApiResult<Json<EnrollResponse>> {
    let limited = lock(&app.enroll_limiter).check(peer.ip(), app.now());
    if let Err(retry_after) = limited {
        info!(source = %peer.ip(), retry_after, "enrollment attempt rate limited");
        return Err(Failure::new(
            ErrorCode::Unavailable,
            "too many enrollment attempts; try again later",
        )
        .with_details(serde_json::json!({ "retry_after_secs": retry_after }))
        .retry_after(retry_after));
    }
    let invalid = || Failure::unauthorized("the pairing code is not valid");
    let code = PairingCode::parse(&request.code).map_err(|_| invalid())?;
    let name = match request.name.trim() {
        "" => request.inventory.os.hostname.trim(),
        name => name,
    };
    let name = if name.is_empty() { "machine" } else { name }.to_string();
    validate_label("a machine's name", &name)?;

    let (token, token_hash) = Token::generate(TokenKind::Machine)
        .map_err(|error| Failure::internal(format!("no random bytes for a token: {error}")))?;
    let (code_hash, token_hash) = (code.hash().to_hex(), token_hash.to_hex());
    let (boot_id, inventory) = (request.boot_id, request.inventory);
    let enrolled = app
        .db(move |ledger| {
            ledger.enroll_with_pairing_code(&code_hash, &name, &token_hash, &boot_id, &inventory)
        })
        .await
        .map_err(|error| match error {
            // Unknown, expired and used codes are not told apart.
            bella_ledger::LedgerError::Unauthorized(_) => invalid(),
            other => other.into(),
        })?;
    app.changed();
    info!(
        machine_id = %enrolled.machine_id,
        name = %enrolled.name,
        source = %peer.ip(),
        "machine enrolled"
    );
    Ok(Json(EnrollResponse {
        fleet_id: enrolled.fleet_id,
        machine_id: enrolled.machine_id,
        machine_token: token,
        policy: enrolled.policy,
    }))
}
