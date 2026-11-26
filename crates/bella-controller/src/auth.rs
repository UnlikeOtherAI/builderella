//! Bearer-token authentication and the role each route requires.
//!
//! A token is hashed and looked up in the ledger on every request, so a
//! revoked principal or a retired machine loses access with its next
//! request. The role that counts is the one the ledger holds, never the tag
//! inside the token.

use crate::error::Failure;
use crate::state::App;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use bella_core::api::Role;
use bella_core::ids::MachineId;
use bella_ledger::{Authenticated, LedgerError, Principal};
use bella_net::{TokenHash, TokenKind};

/// Extract the bearer token of a request, if it has a well-formed one.
fn bearer(parts: &Parts) -> Result<&str, Failure> {
    let header = parts
        .headers
        .get(AUTHORIZATION)
        .ok_or_else(|| Failure::unauthorized("a bearer token is required"))?;
    let value = header
        .to_str()
        .map_err(|_| Failure::unauthorized("the authorization header is not valid"))?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(|| Failure::unauthorized("the authorization header is not a bearer token"))?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Failure::unauthorized(
            "the authorization header is not a bearer token",
        ));
    }
    let token = token.trim();
    // Rejects anything that is not shaped like a token before the ledger is
    // asked, so junk costs no query.
    if TokenKind::from_secret(token).is_err() {
        return Err(Failure::unauthorized("the token is not valid"));
    }
    Ok(token)
}

/// Whoever a valid token belongs to.
pub(crate) async fn authenticate(parts: &Parts, app: &App) -> Result<Authenticated, Failure> {
    let hash = TokenHash::of(bearer(parts)?).to_hex();
    match app.db(move |ledger| ledger.authenticate(&hash)).await {
        Ok(who) => Ok(who),
        // One message for every way a token can be wrong.
        Err(LedgerError::Unauthorized(_)) => Err(Failure::unauthorized("the token is not valid")),
        Err(error) => Err(error.into()),
    }
}

async fn principal(parts: &Parts, app: &App, minimum: Role) -> Result<Principal, Failure> {
    match authenticate(parts, app).await? {
        Authenticated::Principal(principal) if principal.role >= minimum => Ok(principal),
        Authenticated::Principal(principal) => Err(Failure::forbidden(format!(
            "this needs the {} role; the token has the {} role",
            role_name(minimum),
            role_name(principal.role)
        ))),
        Authenticated::Machine { .. } => Err(Failure::forbidden(
            "a machine token cannot be used for this route",
        )),
    }
}

pub(crate) fn role_name(role: Role) -> &'static str {
    match role {
        Role::Viewer => "viewer",
        Role::Developer => "developer",
        Role::Admin => "admin",
    }
}

macro_rules! role_extractor {
    ($(#[$meta:meta])* $name:ident, $role:expr) => {
        $(#[$meta])*
        pub(crate) struct $name(pub Principal);

        impl FromRequestParts<App> for $name {
            type Rejection = Failure;

            async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Failure> {
                principal(parts, app, $role).await.map($name)
            }
        }
    };
}

role_extractor!(
    /// Any principal: read-only routes.
    Viewer,
    Role::Viewer
);
role_extractor!(
    /// Submitting, cancelling and environment actions.
    Developer,
    Role::Developer
);
role_extractor!(
    /// Tokens, pairing codes, machine and project policy.
    Admin,
    Role::Admin
);

/// A machine token: the worker socket.
pub(crate) struct MachineAuth {
    pub id: MachineId,
}

impl FromRequestParts<App> for MachineAuth {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Failure> {
        match authenticate(parts, app).await? {
            Authenticated::Machine { id, .. } => Ok(MachineAuth { id }),
            Authenticated::Principal(_) => Err(Failure::forbidden(
                "this route takes a machine token, not a principal's",
            )),
        }
    }
}

/// Any principal, or a machine. Workers fetch source objects and snapshots
/// with their own token.
pub(crate) struct Reader;

impl FromRequestParts<App> for Reader {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Failure> {
        authenticate(parts, app).await.map(|_| Reader)
    }
}

/// A developer or administrator, or a machine: whoever may add objects.
/// Workers upload artifacts with their own token.
pub(crate) struct Writer;

impl FromRequestParts<App> for Writer {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Failure> {
        match authenticate(parts, app).await? {
            Authenticated::Principal(principal) if principal.role < Role::Developer => {
                Err(Failure::forbidden(
                    "uploading objects needs the developer role; the token has the viewer role",
                ))
            }
            _ => Ok(Writer),
        }
    }
}
