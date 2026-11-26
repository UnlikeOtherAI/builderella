//! What a client or worker stores to reach its controller again.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::fingerprint::{Fingerprint, FingerprintError};
use crate::private_file::{self, PrivateFileError, PrivateFileMode};
use crate::tls::{TlsError, pinned_client_config};
use crate::token::{TokenError, TokenKind};
use crate::url::{ControllerUrl, UrlError};

/// Directory name under the platform configuration directory.
const APP_DIR: &str = "builderella";
/// File name of the default credentials file.
const CREDENTIALS_FILE: &str = "credentials.json";

/// Controller address, pinned fingerprint and bearer token.
///
/// The fingerprint is present exactly when the URL is HTTPS;
/// [`ClientCredentials::validate`] enforces this and is applied by
/// [`ClientCredentials::load`] and [`ClientCredentials::save`]. `Debug`
/// output hides the token.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub url: ControllerUrl,
    pub fingerprint: Option<Fingerprint>,
    pub token: String,
}

/// Credentials could not be loaded, saved or used.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error(transparent)]
    File(#[from] PrivateFileError),
    #[error("{} is not a valid credentials file: {reason}", path.display())]
    Format { path: PathBuf, reason: String },
    #[error("invalid controller URL in the credentials: {0}")]
    Url(#[source] UrlError),
    #[error("invalid fingerprint in the credentials: {0}")]
    Fingerprint(#[source] FingerprintError),
    #[error(
        "an https controller needs a certificate fingerprint to pin; pair with the controller again"
    )]
    FingerprintRequired,
    #[error("a plain http controller has no certificate, so a fingerprint cannot apply")]
    FingerprintNotApplicable,
    #[error("invalid token in the credentials: {0}")]
    Token(#[source] TokenError),
}

/// On-disk form. `allow_insecure` records, in the file itself, that plain
/// HTTP to another machine was chosen explicitly; without it such a URL is
/// refused on load, so editing only the URL cannot downgrade a client.
#[derive(Serialize)]
struct StoredCredentials {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<Fingerprint>,
    token: String,
    #[serde(skip_serializing_if = "is_false")]
    allow_insecure: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Reads the on-disk form field by field.
///
/// Parsing by hand keeps every error message to a field name and a
/// position. A derived deserializer quotes unexpected values back, which
/// could put the token from a hand-edited file into a log.
fn parse_stored(path: &Path, bytes: &[u8]) -> Result<StoredCredentials, CredentialsError> {
    let format = |reason: String| CredentialsError::Format {
        path: path.to_owned(),
        reason,
    };
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        format(format!(
            "it is not valid JSON (line {}, column {})",
            error.line(),
            error.column()
        ))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| format("it must contain a JSON object".to_owned()))?;
    let text = |key: &str| match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(format(format!("`{key}` must be a string"))),
    };
    let required = |key: &str| text(key)?.ok_or_else(|| format(format!("`{key}` is missing")));

    let url = required("url")?.to_owned();
    let token = required("token")?.to_owned();
    let fingerprint = text("fingerprint")?
        .map(Fingerprint::parse)
        .transpose()
        .map_err(CredentialsError::Fingerprint)?;
    let allow_insecure = match object.get("allow_insecure") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(format("`allow_insecure` must be true or false".to_owned())),
    };
    Ok(StoredCredentials {
        url,
        fingerprint,
        token,
        allow_insecure,
    })
}

impl ClientCredentials {
    /// Builds credentials and checks them with
    /// [`ClientCredentials::validate`].
    pub fn new(
        url: ControllerUrl,
        fingerprint: Option<Fingerprint>,
        token: String,
    ) -> Result<ClientCredentials, CredentialsError> {
        let credentials = ClientCredentials {
            url,
            fingerprint,
            token,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    /// Checks that HTTPS comes with a fingerprint, that plain HTTP has
    /// none, and that the token is a well-formed Builderella token.
    pub fn validate(&self) -> Result<(), CredentialsError> {
        match (self.url.is_tls(), self.fingerprint) {
            (true, None) => return Err(CredentialsError::FingerprintRequired),
            (false, Some(_)) => return Err(CredentialsError::FingerprintNotApplicable),
            _ => {}
        }
        TokenKind::from_secret(&self.token).map_err(CredentialsError::Token)?;
        Ok(())
    }

    /// The kind of token held.
    pub fn token_kind(&self) -> Result<TokenKind, TokenError> {
        TokenKind::from_secret(&self.token)
    }

    /// The TLS client configuration for these credentials: pinned to the
    /// stored fingerprint, or `None` when the URL is plain HTTP.
    pub fn tls_config(&self) -> Result<Option<Arc<rustls::ClientConfig>>, TlsError> {
        self.fingerprint.map(pinned_client_config).transpose()
    }

    /// Reads credentials written by [`ClientCredentials::save`].
    ///
    /// Returns the permission check alongside the credentials. On
    /// [`PrivateFileMode::TooOpen`] the token may have been read by other
    /// users of the machine; the caller must at least warn.
    pub fn load(path: &Path) -> Result<(ClientCredentials, PrivateFileMode), CredentialsError> {
        let file = private_file::read_private(path)?;
        let stored = parse_stored(path, &file.bytes)?;
        let url = ControllerUrl::parse(&stored.url, stored.allow_insecure)
            .map_err(CredentialsError::Url)?;
        let credentials = ClientCredentials::new(url, stored.fingerprint, stored.token)?;
        Ok((credentials, file.mode))
    }

    /// Writes the credentials as JSON with [`crate::write_private`]:
    /// owner-only on Unix, atomically replaced, parent directory created.
    pub fn save(&self, path: &Path) -> Result<(), CredentialsError> {
        self.validate()?;
        let stored = StoredCredentials {
            url: self.url.to_string(),
            fingerprint: self.fingerprint,
            token: self.token.clone(),
            allow_insecure: self.url.is_insecure_remote(),
        };
        let mut json =
            serde_json::to_vec_pretty(&stored).map_err(|error| CredentialsError::Format {
                path: path.to_owned(),
                reason: error.to_string(),
            })?;
        json.push(b'\n');
        private_file::write_private(path, &json)?;
        Ok(())
    }

    /// `credentials.json` inside [`config_dir`], or `None` when the
    /// environment does not say where the user's files live.
    pub fn default_path() -> Option<PathBuf> {
        config_dir().map(|dir| dir.join(CREDENTIALS_FILE))
    }
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("url", &self.url.to_string())
            .field("fingerprint", &self.fingerprint)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The per-user configuration directory for Builderella:
///
/// * Windows: `%APPDATA%\builderella`
/// * macOS: `$XDG_CONFIG_HOME/builderella` if that variable is set,
///   otherwise `~/Library/Application Support/builderella`
/// * other Unix: `$XDG_CONFIG_HOME/builderella`, otherwise
///   `~/.config/builderella`
///
/// Variables that are unset, empty or not absolute paths are ignored.
/// `None` means none of them gave an answer; the directory itself is not
/// created or checked.
pub fn config_dir() -> Option<PathBuf> {
    resolve_config_dir(OsFamily::CURRENT, &|key| std::env::var_os(key))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OsFamily {
    Windows,
    MacOs,
    OtherUnix,
}

impl OsFamily {
    const CURRENT: OsFamily = if cfg!(windows) {
        OsFamily::Windows
    } else if cfg!(target_os = "macos") {
        OsFamily::MacOs
    } else {
        OsFamily::OtherUnix
    };
}

fn resolve_config_dir(family: OsFamily, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |key: &str| {
        env(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let base = match family {
        OsFamily::Windows => absolute("APPDATA")?,
        OsFamily::MacOs => absolute("XDG_CONFIG_HOME").or_else(|| {
            absolute("HOME").map(|home| home.join("Library").join("Application Support"))
        })?,
        OsFamily::OtherUnix => {
            absolute("XDG_CONFIG_HOME").or_else(|| absolute("HOME").map(|h| h.join(".config")))?
        }
    };
    Some(base.join(APP_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Token;
    use std::collections::HashMap;
    use std::fs;

    fn token(kind: TokenKind) -> String {
        Token::generate(kind).unwrap().0
    }

    fn fingerprint() -> Fingerprint {
        Fingerprint::of_certificate(b"fleet certificate")
    }

    fn tls_credentials() -> ClientCredentials {
        ClientCredentials::new(
            ControllerUrl::parse("https://192.168.50.7:7420", false).unwrap(),
            Some(fingerprint()),
            token(TokenKind::Developer),
        )
        .unwrap()
    }

    #[test]
    fn tls_credentials_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("credentials.json");
        let credentials = tls_credentials();
        credentials.save(&path).unwrap();

        let (loaded, mode) = ClientCredentials::load(&path).unwrap();
        assert_eq!(loaded, credentials);
        assert!(!mode.is_too_open());
        assert_eq!(loaded.token_kind(), Ok(TokenKind::Developer));
        assert!(loaded.tls_config().unwrap().is_some());

        let text = fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["url"], "https://192.168.50.7:7420");
        assert_eq!(json["fingerprint"], fingerprint().to_hex());
        assert_eq!(json["token"], credentials.token);
        assert!(json.get("allow_insecure").is_none());
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn loopback_http_credentials_have_no_fingerprint_and_no_tls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let credentials = ClientCredentials::new(
            ControllerUrl::parse("http://127.0.0.1:7420", false).unwrap(),
            None,
            token(TokenKind::Admin),
        )
        .unwrap();
        credentials.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("fingerprint"));
        assert!(!text.contains("allow_insecure"));
        let (loaded, _) = ClientCredentials::load(&path).unwrap();
        assert_eq!(loaded, credentials);
        assert!(loaded.tls_config().unwrap().is_none());
    }

    #[test]
    fn insecure_remote_http_is_recorded_and_required_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let credentials = ClientCredentials::new(
            ControllerUrl::parse("http://192.168.50.7:7420", true).unwrap(),
            None,
            token(TokenKind::Machine),
        )
        .unwrap();
        credentials.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"allow_insecure\": true"), "{text}");
        assert_eq!(ClientCredentials::load(&path).unwrap().0, credentials);

        // The same file without the explicit marker is refused.
        let without = text.replace("\"allow_insecure\": true", "\"allow_insecure\": false");
        fs::write(&path, without).unwrap();
        let error = ClientCredentials::load(&path).unwrap_err();
        assert!(
            matches!(
                error,
                CredentialsError::Url(UrlError::InsecureNotAllowed { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn editing_an_https_file_down_to_http_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tls_credentials().save(&path).unwrap();
        let downgraded = fs::read_to_string(&path)
            .unwrap()
            .replace("https://", "http://");
        fs::write(&path, downgraded).unwrap();
        assert!(matches!(
            ClientCredentials::load(&path).unwrap_err(),
            CredentialsError::Url(UrlError::InsecureNotAllowed { .. })
        ));
    }

    #[test]
    fn https_without_a_fingerprint_is_never_accepted() {
        let url = ControllerUrl::parse("https://192.168.50.7", false).unwrap();
        assert!(matches!(
            ClientCredentials::new(url.clone(), None, token(TokenKind::Viewer)),
            Err(CredentialsError::FingerprintRequired)
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let json = serde_json::json!({ "url": url.to_string(), "token": token(TokenKind::Viewer) });
        fs::write(&path, json.to_string()).unwrap();
        assert!(matches!(
            ClientCredentials::load(&path).unwrap_err(),
            CredentialsError::FingerprintRequired
        ));

        // Bypassing the constructor does not get past `save` either.
        let unpinned = ClientCredentials {
            url,
            fingerprint: None,
            token: token(TokenKind::Viewer),
        };
        assert!(matches!(
            unpinned.save(&path),
            Err(CredentialsError::FingerprintRequired)
        ));
        assert!(matches!(
            ClientCredentials::new(
                ControllerUrl::parse("http://localhost", false).unwrap(),
                Some(fingerprint()),
                token(TokenKind::Viewer),
            ),
            Err(CredentialsError::FingerprintNotApplicable)
        ));
    }

    #[test]
    fn malformed_files_are_rejected_without_echoing_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        let missing = ClientCredentials::load(&path).unwrap_err();
        assert!(matches!(&missing, CredentialsError::File(e) if e.is_not_found()));

        fs::write(&path, b"not json").unwrap();
        assert!(matches!(
            ClientCredentials::load(&path).unwrap_err(),
            CredentialsError::Format { .. }
        ));

        let secret = token(TokenKind::Developer);
        let broken = format!("{secret}x");
        let json = serde_json::json!({
            "url": "https://192.168.50.7",
            "fingerprint": fingerprint().to_hex(),
            "token": broken,
        });
        fs::write(&path, json.to_string()).unwrap();
        let error = ClientCredentials::load(&path).unwrap_err();
        assert!(matches!(error, CredentialsError::Token(_)), "{error}");
        assert!(!error.to_string().contains(&secret[12..24]));

        let json = serde_json::json!({
            "url": "https://192.168.50.7",
            "fingerprint": "abc",
            "token": secret,
        });
        fs::write(&path, json.to_string()).unwrap();
        assert!(matches!(
            ClientCredentials::load(&path).unwrap_err(),
            CredentialsError::Fingerprint(FingerprintError::Length { found: 3 })
        ));

        // A token pasted into the wrong field is not quoted back.
        for json in [
            serde_json::json!({ "url": "https://192.168.50.7", "allow_insecure": secret }),
            serde_json::json!({ "url": [secret], "token": secret }),
            serde_json::json!({ "url": "https://192.168.50.7", "token": { "value": secret } }),
            serde_json::json!([secret]),
            serde_json::json!({ "url": "https://192.168.50.7" }),
        ] {
            fs::write(&path, json.to_string()).unwrap();
            let error = ClientCredentials::load(&path).unwrap_err();
            assert!(matches!(error, CredentialsError::Format { .. }), "{error}");
            assert!(!error.to_string().contains(&secret[12..24]), "{error}");
        }
        fs::write(&path, format!("{{\"token\": \"{secret}")).unwrap();
        let error = ClientCredentials::load(&path).unwrap_err();
        assert!(error.to_string().contains("not valid JSON"), "{error}");
        assert!(!error.to_string().contains(&secret[12..24]), "{error}");

        let json = serde_json::json!({
            "url": "ftp://192.168.50.7",
            "fingerprint": fingerprint().to_hex(),
            "token": secret,
        });
        fs::write(&path, json.to_string()).unwrap();
        assert!(matches!(
            ClientCredentials::load(&path).unwrap_err(),
            CredentialsError::Url(UrlError::UnsupportedScheme { .. })
        ));
    }

    #[test]
    fn debug_output_hides_the_token() {
        let credentials = tls_credentials();
        let debug = format!("{credentials:?}");
        assert!(!debug.contains(&credentials.token));
        assert!(!debug.contains(&credentials.token[10..20]));
        assert!(debug.contains("https://192.168.50.7:7420"));
        assert!(debug.contains("<redacted>"));
    }

    #[cfg(unix)]
    #[test]
    fn the_credentials_file_is_owner_only_and_a_loosened_one_is_flagged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tls_credentials().save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            ClientCredentials::load(&path).unwrap().1,
            PrivateFileMode::OwnerOnly
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            ClientCredentials::load(&path).unwrap().1,
            PrivateFileMode::TooOpen { mode: 0o644 }
        );
    }

    fn env_of(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.as_os_str().to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn config_directory_follows_each_platform_convention() {
        // Any absolute path of the host works as a stand-in base.
        let base = std::env::temp_dir();
        let home = base.join("home");
        let xdg = base.join("xdg");
        let appdata = base.join("appdata");

        let env = env_of(&[("HOME", &home), ("APPDATA", &appdata)]);
        assert_eq!(
            resolve_config_dir(OsFamily::Windows, &env),
            Some(appdata.join("builderella"))
        );
        assert_eq!(
            resolve_config_dir(OsFamily::MacOs, &env),
            Some(
                home.join("Library")
                    .join("Application Support")
                    .join("builderella")
            )
        );
        assert_eq!(
            resolve_config_dir(OsFamily::OtherUnix, &env),
            Some(home.join(".config").join("builderella"))
        );

        let env = env_of(&[("HOME", &home), ("XDG_CONFIG_HOME", &xdg)]);
        assert_eq!(
            resolve_config_dir(OsFamily::MacOs, &env),
            Some(xdg.join("builderella"))
        );
        assert_eq!(
            resolve_config_dir(OsFamily::OtherUnix, &env),
            Some(xdg.join("builderella"))
        );
        // Windows uses %APPDATA% only.
        assert_eq!(resolve_config_dir(OsFamily::Windows, &env), None);
    }

    #[test]
    fn unusable_environment_values_are_ignored() {
        let home = std::env::temp_dir().join("home");
        let relative = Path::new("relative/dir");
        let empty = Path::new("");

        let env = env_of(&[("HOME", &home), ("XDG_CONFIG_HOME", relative)]);
        assert_eq!(
            resolve_config_dir(OsFamily::OtherUnix, &env),
            Some(home.join(".config").join("builderella"))
        );
        let env = env_of(&[("HOME", &home), ("XDG_CONFIG_HOME", empty)]);
        assert_eq!(
            resolve_config_dir(OsFamily::OtherUnix, &env),
            Some(home.join(".config").join("builderella"))
        );
        let env = env_of(&[("HOME", relative), ("APPDATA", empty)]);
        for family in [OsFamily::Windows, OsFamily::MacOs, OsFamily::OtherUnix] {
            assert_eq!(resolve_config_dir(family, &env), None, "{family:?}");
        }
    }

    #[test]
    fn default_path_is_the_credentials_file_in_the_config_directory() {
        match (config_dir(), ClientCredentials::default_path()) {
            (Some(dir), Some(path)) => {
                assert!(dir.is_absolute());
                assert!(dir.ends_with("builderella"));
                assert_eq!(path, dir.join("credentials.json"));
            }
            (None, None) => {}
            other => panic!("config_dir and default_path disagree: {other:?}"),
        }
    }
}
