//! The controller's TLS identity: one self-signed certificate per fleet.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bella_core::{Clock, SystemClock};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, DnsName, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use time::OffsetDateTime;

use crate::fingerprint::Fingerprint;
use crate::private_file::{self, PrivateFileError, PrivateFileMode};

/// File name of the certificate (PEM) inside the identity directory.
pub const CERT_FILE: &str = "controller.crt";
/// File name of the private key (PKCS#8 PEM) inside the identity directory.
pub const KEY_FILE: &str = "controller.key";

/// How long a generated certificate is valid: ten years.
pub const VALIDITY: Duration = Duration::from_secs(3652 * 24 * 60 * 60);

/// Generated certificates are valid from one day before the clock reading,
/// so a peer whose clock runs slightly behind does not see a certificate
/// from the future.
const BACKDATE: Duration = Duration::from_secs(24 * 60 * 60);

const COMMON_NAME: &str = "Builderella fleet controller";

/// The fleet certificate and its private key.
///
/// `Debug` output shows the fingerprint and never the key.
pub struct ServerIdentity {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: Fingerprint,
    generated: bool,
    key_file_mode: PrivateFileMode,
}

/// The fleet identity could not be created or loaded.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(
        "`{name}` is not a valid subject alternative name: expected an IP address or a DNS name"
    )]
    InvalidSubjectAltName { name: String },
    #[error("could not generate the fleet certificate: {reason}")]
    Generate { reason: String },
    #[error("the system clock reads a time that cannot be written into a certificate")]
    ClockOutOfRange,
    #[error(transparent)]
    File(#[from] PrivateFileError),
    #[error(
        "{} exists but its private key {} is missing. Restore the key, or remove the certificate \
         to create a new fleet identity (every machine must then be paired again)",
        cert.display(),
        key.display()
    )]
    KeyMissing { cert: PathBuf, key: PathBuf },
    #[error("{} is not a usable PEM certificate: {reason}", path.display())]
    InvalidCertificate { path: PathBuf, reason: String },
    #[error("{} is not a usable PEM private key: {reason}", path.display())]
    InvalidKey { path: PathBuf, reason: String },
    #[error(
        "the private key in {} does not belong to the certificate in {}",
        key.display(),
        cert.display()
    )]
    KeyMismatch { cert: PathBuf, key: PathBuf },
}

impl ServerIdentity {
    /// Loads the identity stored in `dir`, creating it on first use.
    ///
    /// **First use** (no `controller.crt`): generates an ECDSA P-256 key
    /// and a self-signed certificate valid for [`VALIDITY`], naming
    /// `subject_alt_names` (IP addresses or DNS names). The key is written
    /// with [`crate::write_private`]; the certificate is written last and
    /// its appearance is what marks the identity as complete.
    ///
    /// **Afterwards**: returns exactly what is on disk. Nothing is
    /// regenerated and `subject_alt_names` is ignored, so the fingerprint
    /// every paired machine has pinned stays the same across restarts,
    /// address changes and upgrades. Pinned clients do not check names, so
    /// a stale name list costs them nothing.
    ///
    /// Failure cases are deliberate:
    ///
    /// * Certificate present, key missing: [`IdentityError::KeyMissing`].
    ///   The identity is not silently replaced, because that would cut off
    ///   every paired machine and the key may only be temporarily
    ///   unavailable.
    /// * Key present, certificate missing: the leftover of a first start
    ///   that was interrupted before any fingerprint could have been
    ///   handed out. A fresh identity replaces it.
    /// * Key and certificate that do not belong together:
    ///   [`IdentityError::KeyMismatch`].
    ///
    /// Only one process may create the identity in a directory at a time.
    /// The controller is the single writer of its state directory, which
    /// already ensures this.
    pub fn load_or_generate(
        dir: &Path,
        subject_alt_names: &[String],
    ) -> Result<ServerIdentity, IdentityError> {
        Self::load_or_generate_at(dir, subject_alt_names, &SystemClock)
    }

    /// [`ServerIdentity::load_or_generate`] with an explicit clock for the
    /// validity period of a newly generated certificate.
    pub fn load_or_generate_at(
        dir: &Path,
        subject_alt_names: &[String],
        clock: &dyn Clock,
    ) -> Result<ServerIdentity, IdentityError> {
        if let Some(identity) = Self::load(dir)? {
            return Ok(identity);
        }
        let (cert_pem, key_pem) = generate(subject_alt_names, clock)?;
        private_file::write_private(&dir.join(KEY_FILE), key_pem.as_bytes())?;
        private_file::write_public(&dir.join(CERT_FILE), cert_pem.as_bytes())?;
        // Return what was stored, not what was generated, so the caller
        // serves precisely the identity the next start will load.
        let mut identity = Self::load(dir)?.ok_or_else(|| {
            IdentityError::File(PrivateFileError::Io {
                action: "read back",
                path: dir.join(CERT_FILE),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "the certificate disappeared after it was written",
                ),
            })
        })?;
        identity.generated = true;
        Ok(identity)
    }

    /// Loads the identity in `dir` without ever creating one. `Ok(None)`
    /// means no certificate is stored there yet.
    pub fn load(dir: &Path) -> Result<Option<ServerIdentity>, IdentityError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);

        let cert_file = match private_file::read_private(&cert_path) {
            Ok(file) => file,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let key_file = match private_file::read_private(&key_path) {
            Ok(file) => file,
            Err(error) if error.is_not_found() => {
                return Err(IdentityError::KeyMissing {
                    cert: cert_path,
                    key: key_path,
                });
            }
            Err(error) => return Err(error.into()),
        };

        let invalid_cert = |reason: String| IdentityError::InvalidCertificate {
            path: cert_path.clone(),
            reason,
        };
        let cert_pem = String::from_utf8(cert_file.bytes)
            .map_err(|_| invalid_cert("the file is not text".to_owned()))?;
        let mut certificates = CertificateDer::pem_slice_iter(cert_pem.as_bytes());
        let cert_der = match certificates.next() {
            Some(Ok(der)) => der,
            Some(Err(error)) => return Err(invalid_cert(error.to_string())),
            None => return Err(invalid_cert("it contains no certificate".to_owned())),
        };
        if certificates.next().is_some() {
            return Err(invalid_cert(
                "it contains more than one certificate; the fleet identity is a single self-signed certificate"
                    .to_owned(),
            ));
        }

        let invalid_key = |reason: String| IdentityError::InvalidKey {
            path: key_path.clone(),
            reason,
        };
        let key = PrivateKeyDer::from_pem_slice(&key_file.bytes)
            .map_err(|error| invalid_key(error.to_string()))?;

        // Prove the pair is usable now instead of at the first handshake.
        let signing_key = provider()
            .key_provider
            .load_private_key(key.clone_key())
            .map_err(|error| invalid_key(error.to_string()))?;
        let certified = CertifiedKey::new(vec![cert_der.clone()], signing_key);
        match certified.keys_match() {
            Ok(()) => {}
            Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch)) => {
                return Err(IdentityError::KeyMismatch {
                    cert: cert_path,
                    key: key_path,
                });
            }
            Err(rustls::Error::InconsistentKeys(_)) => {
                return Err(invalid_key(
                    "its public key could not be compared with the certificate".to_owned(),
                ));
            }
            Err(error) => return Err(invalid_cert(error.to_string())),
        }

        Ok(Some(ServerIdentity {
            fingerprint: Fingerprint::of_certificate(cert_der.as_ref()),
            cert_pem,
            cert_der,
            key,
            generated: false,
            key_file_mode: key_file.mode,
        }))
    }

    /// The certificate as stored, PEM encoded. Safe to hand to anyone.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The certificate's DER encoding, the bytes the fingerprint covers.
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// SHA-256 of [`ServerIdentity::cert_der`]: what clients pin.
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// True when this call created the identity, false when it was loaded.
    /// A controller uses this to announce a new fleet fingerprint once.
    pub fn was_generated(&self) -> bool {
        self.generated
    }

    /// Result of the permission check on the key file when it was read. A
    /// controller should warn loudly on [`PrivateFileMode::TooOpen`]: the
    /// fleet key may have been copied.
    pub fn key_file_mode(&self) -> PrivateFileMode {
        self.key_file_mode
    }

    /// A copy of the private key for building a server configuration.
    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("fingerprint", &self.fingerprint)
            .field("generated", &self.generated)
            .field("key_file_mode", &self.key_file_mode)
            .finish_non_exhaustive()
    }
}

/// The crypto provider every configuration in this crate is built with.
/// Constructed per use so that no process-wide default has to be installed.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Returns `(certificate PEM, private key PEM)` for a new identity.
fn generate(
    subject_alt_names: &[String],
    clock: &dyn Clock,
) -> Result<(String, String), IdentityError> {
    for name in subject_alt_names {
        let is_ip = name.parse::<IpAddr>().is_ok();
        if !is_ip && DnsName::try_from(name.as_str()).is_err() {
            return Err(IdentityError::InvalidSubjectAltName { name: name.clone() });
        }
    }
    let generate_error = |error: rcgen::Error| IdentityError::Generate {
        reason: error.to_string(),
    };

    let now = Duration::from_millis(clock.now().0);
    let mut params = CertificateParams::new(subject_alt_names.to_vec()).map_err(generate_error)?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, COMMON_NAME);
    params.distinguished_name = name;
    params.not_before = timestamp(now.saturating_sub(BACKDATE))?;
    params.not_after = timestamp(now.saturating_add(VALIDITY))?;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(generate_error)?;
    let certificate = params.self_signed(&key_pair).map_err(generate_error)?;
    Ok((certificate.pem(), key_pair.serialize_pem()))
}

fn timestamp(since_epoch: Duration) -> Result<OffsetDateTime, IdentityError> {
    let seconds =
        i64::try_from(since_epoch.as_secs()).map_err(|_| IdentityError::ClockOutOfRange)?;
    let moment =
        OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| IdentityError::ClockOutOfRange)?;
    // X.509 dates end at the year 9999.
    if moment.year() > 9999 {
        return Err(IdentityError::ClockOutOfRange);
    }
    Ok(moment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::{ManualClock, UnixMillis};
    use rustls::RootCertStore;
    use rustls::client::verify_server_cert_signed_by_trust_anchor;
    use rustls::pki_types::UnixTime;
    use rustls::server::ParsedCertificate;
    use std::fs;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn names() -> Vec<String> {
        vec![
            "build-box.local".to_owned(),
            "192.168.50.7".to_owned(),
            "fd00::12".to_owned(),
        ]
    }

    /// 2026-01-01T00:00:00Z.
    fn start() -> UnixMillis {
        UnixMillis(1_767_225_600_000)
    }

    /// Checks the certificate against itself as the only trust anchor at
    /// `at`, which exercises the validity period and the self-signature.
    fn valid_at(identity: &ServerIdentity, at: UnixMillis) -> Result<(), rustls::Error> {
        let mut roots = RootCertStore::empty();
        roots.add(identity.cert_der().clone()).unwrap();
        let parsed = ParsedCertificate::try_from(identity.cert_der()).unwrap();
        verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &roots,
            &[],
            UnixTime::since_unix_epoch(Duration::from_millis(at.0)),
            provider().signature_verification_algorithms.all,
        )
    }

    #[test]
    fn generate_then_load_keeps_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let first = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        assert!(first.was_generated());

        let second = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        assert!(!second.was_generated());
        assert_eq!(second.fingerprint(), first.fingerprint());
        assert_eq!(second.cert_pem(), first.cert_pem());
        assert_eq!(second.cert_der(), first.cert_der());

        // A different name list on a later start must not mint a new
        // identity either.
        let other_names = vec!["10.1.2.3".to_owned()];
        let third = ServerIdentity::load_or_generate(dir.path(), &other_names).unwrap();
        assert!(!third.was_generated());
        assert_eq!(third.fingerprint(), first.fingerprint());

        let loaded = ServerIdentity::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.fingerprint(), first.fingerprint());
    }

    #[test]
    fn the_fingerprint_is_sha256_of_the_stored_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let identity = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();

        let on_disk = fs::read_to_string(dir.path().join(CERT_FILE)).unwrap();
        assert_eq!(on_disk, identity.cert_pem());
        assert!(on_disk.starts_with("-----BEGIN CERTIFICATE-----"));
        let der = CertificateDer::from_pem_slice(on_disk.as_bytes()).unwrap();
        assert_eq!(&der, identity.cert_der());

        let expected = <sha2::Sha256 as sha2::Digest>::digest(der.as_ref());
        assert_eq!(
            identity.fingerprint().as_bytes().as_slice(),
            expected.as_slice()
        );
    }

    #[test]
    fn the_key_is_ecdsa_p256_in_pkcs8_pem() {
        let dir = tempfile::tempdir().unwrap();
        let identity = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        let key_pem = fs::read_to_string(dir.path().join(KEY_FILE)).unwrap();
        assert!(key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(matches!(identity.private_key(), PrivateKeyDer::Pkcs8(_)));

        // DER encoding of the OID 1.2.840.10045.3.1.7 (prime256v1), which
        // appears in the certificate's public key and nowhere by accident.
        let p256_oid = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        let der: &[u8] = identity.cert_der().as_ref();
        assert!(der.windows(p256_oid.len()).any(|w| w == p256_oid));
        let common_name = COMMON_NAME.as_bytes();
        assert!(der.windows(common_name.len()).any(|w| w == common_name));
    }

    #[test]
    fn each_directory_gets_its_own_identity() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = ServerIdentity::load_or_generate(a.path(), &names()).unwrap();
        let second = ServerIdentity::load_or_generate(b.path(), &names()).unwrap();
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn the_certificate_is_valid_for_ten_years() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::at(start());
        let identity = ServerIdentity::load_or_generate_at(dir.path(), &names(), &clock).unwrap();

        assert!(valid_at(&identity, start()).is_ok());
        // Tolerates a peer clock that is a little behind, but not a lot.
        assert!(valid_at(&identity, start().minus(DAY / 2)).is_ok());
        assert!(valid_at(&identity, start().minus(DAY * 2)).is_err());
        // Ten years, to the day.
        assert!(valid_at(&identity, start().plus(DAY * 3651)).is_ok());
        assert!(matches!(
            valid_at(&identity, start().plus(DAY * 3653)),
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired | rustls::CertificateError::ExpiredContext { .. }
            ))
        ));
        assert_eq!(VALIDITY, DAY * 3652);
    }

    #[test]
    fn a_missing_key_is_an_error_and_the_certificate_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        let cert_before = fs::read(dir.path().join(CERT_FILE)).unwrap();
        fs::remove_file(dir.path().join(KEY_FILE)).unwrap();

        let error = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap_err();
        assert!(matches!(error, IdentityError::KeyMissing { .. }), "{error}");
        assert!(error.to_string().contains(KEY_FILE));
        assert_eq!(fs::read(dir.path().join(CERT_FILE)).unwrap(), cert_before);
        assert!(!dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn a_key_left_by_an_interrupted_first_start_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        // What a crash between the two writes leaves behind.
        let abandoned = tempfile::tempdir().unwrap();
        ServerIdentity::load_or_generate(abandoned.path(), &names()).unwrap();
        fs::copy(abandoned.path().join(KEY_FILE), dir.path().join(KEY_FILE)).unwrap();
        assert!(ServerIdentity::load(dir.path()).unwrap().is_none());

        let identity = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        assert!(identity.was_generated());
        let again = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        assert_eq!(again.fingerprint(), identity.fingerprint());
        assert_ne!(
            fs::read(dir.path().join(KEY_FILE)).unwrap(),
            fs::read(abandoned.path().join(KEY_FILE)).unwrap()
        );
    }

    #[test]
    fn a_key_from_another_identity_is_rejected() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        ServerIdentity::load_or_generate(a.path(), &names()).unwrap();
        ServerIdentity::load_or_generate(b.path(), &names()).unwrap();
        fs::copy(b.path().join(KEY_FILE), a.path().join(KEY_FILE)).unwrap();

        let error = ServerIdentity::load_or_generate(a.path(), &names()).unwrap_err();
        assert!(
            matches!(error, IdentityError::KeyMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn damaged_files_are_reported_with_their_path() {
        let dir = tempfile::tempdir().unwrap();
        ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        let cert_path = dir.path().join(CERT_FILE);
        let key_path = dir.path().join(KEY_FILE);
        let good_cert = fs::read(&cert_path).unwrap();
        let good_key = fs::read(&key_path).unwrap();

        fs::write(&key_path, b"not a key").unwrap();
        let error = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap_err();
        assert!(matches!(error, IdentityError::InvalidKey { .. }), "{error}");
        assert!(error.to_string().contains(KEY_FILE));
        fs::write(&key_path, &good_key).unwrap();

        fs::write(&cert_path, b"not a certificate").unwrap();
        let error = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap_err();
        assert!(
            matches!(error, IdentityError::InvalidCertificate { .. }),
            "{error}"
        );
        assert!(error.to_string().contains(CERT_FILE));

        let mut two = good_cert.clone();
        two.extend_from_slice(&good_cert);
        fs::write(&cert_path, &two).unwrap();
        let error = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap_err();
        assert!(
            error.to_string().contains("more than one certificate"),
            "{error}"
        );

        // Restoring the original files restores the original identity.
        fs::write(&cert_path, &good_cert).unwrap();
        assert!(ServerIdentity::load_or_generate(dir.path(), &names()).is_ok());
    }

    #[test]
    fn invalid_subject_alt_names_are_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["host name", "bücher.local", "", "*.local", "https://host"] {
            let error =
                ServerIdentity::load_or_generate(dir.path(), &[bad.to_owned()]).unwrap_err();
            assert!(
                matches!(&error, IdentityError::InvalidSubjectAltName { name } if name == bad),
                "{bad:?}: {error}"
            );
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        // No names at all is allowed: pinned clients do not need any.
        assert!(ServerIdentity::load_or_generate(dir.path(), &[]).is_ok());
    }

    #[test]
    fn a_clock_beyond_the_certificate_date_range_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::at(UnixMillis(u64::MAX));
        let error = ServerIdentity::load_or_generate_at(dir.path(), &names(), &clock).unwrap_err();
        assert!(matches!(error, IdentityError::ClockOutOfRange), "{error}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn debug_output_shows_the_fingerprint_and_not_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let identity = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        let debug = format!("{identity:?}");
        assert!(debug.contains(&identity.fingerprint().to_hex()));
        assert!(!debug.contains("PRIVATE"));
        assert!(!debug.contains("key:"));
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only_and_a_loosened_one_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let identity = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        let key_path = dir.path().join(KEY_FILE);
        assert_eq!(
            fs::metadata(&key_path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(identity.key_file_mode(), PrivateFileMode::OwnerOnly);

        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
        let reloaded = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        assert_eq!(
            reloaded.key_file_mode(),
            PrivateFileMode::TooOpen { mode: 0o644 }
        );
        assert_eq!(reloaded.fingerprint(), identity.fingerprint());
    }

    #[cfg(not(unix))]
    #[test]
    fn the_key_file_permissions_are_reported_as_not_checked_off_unix() {
        let dir = tempfile::tempdir().unwrap();
        let identity = ServerIdentity::load_or_generate(dir.path(), &names()).unwrap();
        assert_eq!(identity.key_file_mode(), PrivateFileMode::NotChecked);
    }
}
