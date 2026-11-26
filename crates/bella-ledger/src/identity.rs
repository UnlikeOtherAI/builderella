//! Principals, token authentication and pairing codes.
//!
//! The ledger never sees a token or a pairing code, only the hash the
//! caller computed from it.

use crate::db::{
    Ctx, all_rows, count, enum_text, enumeration, exec, id, ms, opt_row, opt_time, text, time,
};
use crate::events::emit;
use crate::{Authenticated, Ledger, LedgerError, Principal, Result};
use bella_core::UnixMillis;
use bella_core::api::{EntityKind, Role};
use bella_core::ids::{MachineId, PrincipalId};
use bella_core::machine::MachineProfile;
use rusqlite::{Row, params};
use serde_json::json;

const PRINCIPAL_COLUMNS: &str = "id, name, role, created_at, revoked_at";

fn principal_from(row: &Row<'_>) -> Result<Principal> {
    Ok(Principal {
        id: id(row, "id")?,
        name: text(row, "name")?,
        role: enumeration(row, "role")?,
        created_at: time(row, "created_at")?,
        revoked_at: opt_time(row, "revoked_at")?,
    })
}

pub(crate) fn load_principal(c: &Ctx<'_>, principal: &PrincipalId) -> Result<Principal> {
    opt_row(
        c.conn,
        &format!("SELECT {PRINCIPAL_COLUMNS} FROM principals WHERE id = ?1"),
        params![principal.as_str()],
        principal_from,
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("principal {principal}")))
}

/// The principal, provided it is not revoked and holds at least `minimum`.
pub(crate) fn require_role(
    c: &Ctx<'_>,
    principal: &PrincipalId,
    minimum: Role,
) -> Result<Principal> {
    let found = load_principal(c, principal)?;
    if found.revoked_at.is_some() {
        return Err(LedgerError::Unauthorized(format!(
            "principal {principal} is revoked"
        )));
    }
    if found.role < minimum {
        return Err(LedgerError::Forbidden(format!(
            "this needs the {} role; {} has {}",
            enum_text(&minimum)?,
            found.name,
            enum_text(&found.role)?
        )));
    }
    Ok(found)
}

/// A token hash must not identify two different bearers.
pub(crate) fn ensure_hash_unused(c: &Ctx<'_>, token_hash: &str) -> Result<()> {
    if token_hash.is_empty() {
        return Err(LedgerError::Invalid("empty token hash".into()));
    }
    let used = count(
        c.conn,
        "SELECT (SELECT COUNT(*) FROM principals WHERE token_hash = ?1)
              + (SELECT COUNT(*) FROM machines WHERE token_hash = ?1)",
        params![token_hash],
    )?;
    if used > 0 {
        return Err(LedgerError::Conflict("token hash is already in use".into()));
    }
    Ok(())
}

/// Mark a pairing code used. Succeeds exactly once per code, and only
/// before it expires.
pub(crate) fn consume_pairing_code(c: &Ctx<'_>, code_hash: &str) -> Result<MachineProfile> {
    let changed = exec(
        c.conn,
        "UPDATE pairing_codes SET consumed_at = ?1
         WHERE code_hash = ?2 AND consumed_at IS NULL AND expires_at > ?1",
        params![ms(c.now), code_hash],
    )?;
    let found = opt_row(
        c.conn,
        "SELECT profile, consumed_at, expires_at FROM pairing_codes WHERE code_hash = ?1",
        params![code_hash],
        |row| {
            Ok((
                enumeration::<MachineProfile, _>(row, "profile")?,
                opt_time(row, "consumed_at")?,
                time(row, "expires_at")?,
            ))
        },
    )?;
    let Some((profile, consumed_at, expires_at)) = found else {
        return Err(LedgerError::Unauthorized("unknown pairing code".into()));
    };
    if changed == 1 {
        emit(
            c,
            EntityKind::Fleet,
            "pairing_code",
            None,
            "pairing_code.consumed",
            json!({ "profile": profile }),
        )?;
        return Ok(profile);
    }
    if consumed_at.is_some() {
        Err(LedgerError::Unauthorized(
            "pairing code was already used".into(),
        ))
    } else {
        Err(LedgerError::Unauthorized(format!(
            "pairing code expired at {}",
            expires_at.0
        )))
    }
}

impl Ledger {
    /// Create a principal identified by the hash of its bearer token.
    /// A hash already used by a principal or machine is a conflict.
    pub fn create_principal(&self, name: &str, role: Role, token_hash: &str) -> Result<Principal> {
        self.write(|c| {
            if name.trim().is_empty() {
                return Err(LedgerError::Invalid("empty principal name".into()));
            }
            ensure_hash_unused(c, token_hash)?;
            let principal = Principal {
                id: PrincipalId::generate(),
                name: name.to_string(),
                role,
                created_at: c.now,
                revoked_at: None,
            };
            exec(
                c.conn,
                "INSERT INTO principals (id, name, role, token_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    principal.id.as_str(),
                    principal.name,
                    enum_text(&role)?,
                    token_hash,
                    ms(c.now)
                ],
            )?;
            emit(
                c,
                EntityKind::Fleet,
                principal.id.as_str(),
                None,
                "principal.created",
                json!({ "name": name, "role": role }),
            )?;
            Ok(principal)
        })
    }

    pub fn principal(&self, principal: &PrincipalId) -> Result<Principal> {
        self.read(|c| load_principal(c, principal))
    }

    /// Every principal, oldest first, including revoked ones.
    pub fn principals(&self) -> Result<Vec<Principal>> {
        self.read(|c| {
            all_rows(
                c.conn,
                &format!("SELECT {PRINCIPAL_COLUMNS} FROM principals ORDER BY created_at, id"),
                params![],
                principal_from,
            )
        })
    }

    /// Resolve a token hash to its bearer. Unknown hashes, revoked
    /// principals and retired or revoked machines are
    /// [`LedgerError::Unauthorized`].
    pub fn authenticate(&self, token_hash: &str) -> Result<Authenticated> {
        self.read(|c| {
            let principal = opt_row(
                c.conn,
                &format!("SELECT {PRINCIPAL_COLUMNS} FROM principals WHERE token_hash = ?1"),
                params![token_hash],
                principal_from,
            )?;
            if let Some(principal) = principal {
                if principal.revoked_at.is_some() {
                    return Err(LedgerError::Unauthorized("token is revoked".into()));
                }
                return Ok(Authenticated::Principal(principal));
            }
            let machine = opt_row(
                c.conn,
                "SELECT id, name FROM machines
                 WHERE token_hash = ?1 AND lifecycle NOT IN ('retired', 'revoked')",
                params![token_hash],
                |row| Ok((id::<MachineId, _>(row, "id")?, text(row, "name")?)),
            )?;
            match machine {
                Some((id, name)) => Ok(Authenticated::Machine { id, name }),
                None => Err(LedgerError::Unauthorized("unknown token".into())),
            }
        })
    }

    /// Revoke a principal's token. Returns `false` when it was already
    /// revoked. Its jobs are not affected.
    pub fn revoke_principal(&self, principal: &PrincipalId) -> Result<bool> {
        self.write(|c| {
            load_principal(c, principal)?;
            let changed = exec(
                c.conn,
                "UPDATE principals SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                params![ms(c.now), principal.as_str()],
            )?;
            if changed == 1 {
                emit(
                    c,
                    EntityKind::Fleet,
                    principal.as_str(),
                    None,
                    "principal.revoked",
                    serde_json::Value::Null,
                )?;
            }
            Ok(changed == 1)
        })
    }

    /// The authorisation check the API layer calls before a privileged
    /// operation: the principal exists, is not revoked and holds at least
    /// `minimum`. Otherwise `NotFound`, `Unauthorized` or `Forbidden`.
    pub fn require_role(&self, principal: &PrincipalId, minimum: Role) -> Result<Principal> {
        self.read(|c| require_role(c, principal, minimum))
    }

    /// [`Ledger::require_role`] for [`Role::Admin`]: the check for changing
    /// machine or project policy, enrolling machines and minting tokens.
    /// The policy mutators repeat it inside their own transaction.
    pub fn require_admin(&self, principal: &PrincipalId) -> Result<Principal> {
        self.require_role(principal, Role::Admin)
    }

    /// Store the hash of a new single-use pairing code.
    pub fn create_pairing_code(
        &self,
        code_hash: &str,
        profile: MachineProfile,
        expires_at: UnixMillis,
        created_by: Option<&PrincipalId>,
    ) -> Result<()> {
        self.write(|c| {
            if code_hash.is_empty() {
                return Err(LedgerError::Invalid("empty pairing code hash".into()));
            }
            if expires_at <= c.now {
                return Err(LedgerError::Invalid(
                    "pairing code would already be expired".into(),
                ));
            }
            let exists = count(
                c.conn,
                "SELECT COUNT(*) FROM pairing_codes WHERE code_hash = ?1",
                params![code_hash],
            )?;
            if exists > 0 {
                return Err(LedgerError::Conflict("pairing code already exists".into()));
            }
            exec(
                c.conn,
                "INSERT INTO pairing_codes (code_hash, profile, created_by, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    code_hash,
                    enum_text(&profile)?,
                    created_by.map(PrincipalId::as_str),
                    ms(c.now),
                    ms(expires_at)
                ],
            )?;
            emit(
                c,
                EntityKind::Fleet,
                "pairing_code",
                None,
                "pairing_code.created",
                json!({ "profile": profile, "expires_at": expires_at }),
            )?;
            Ok(())
        })
    }

    /// Use a pairing code. Atomic: of any number of concurrent or repeated
    /// calls for one code exactly one succeeds, and none after it expires.
    /// Prefer [`Ledger::enroll_with_pairing_code`], which consumes the code
    /// and enrolls the machine in the same transaction.
    pub fn consume_pairing_code(&self, code_hash: &str) -> Result<MachineProfile> {
        self.write(|c| consume_pairing_code(c, code_hash))
    }
}
