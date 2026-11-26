//! Controller addresses and the rule for when plain HTTP is acceptable.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use bella_core::defaults::CONTROLLER_PORT;
use rustls::pki_types::{DnsName, ServerName};

/// Transport of a controller URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// TLS with the pinned fleet certificate.
    Https,
    /// No encryption and no server authentication.
    Http,
}

impl Scheme {
    /// `https` or `http`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Scheme::Https => "https",
            Scheme::Http => "http",
        }
    }
}

/// A controller URL was rejected.
///
/// Messages never repeat a path, query or credentials from the input, since
/// those are the places a secret pasted by mistake would be.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("the controller URL is empty")]
    Empty,
    #[error("the controller URL contains whitespace or control characters")]
    IllegalCharacter,
    #[error("the controller URL must start with https:// (or http:// for a loopback address)")]
    MissingScheme,
    #[error("unsupported scheme `{scheme}`: use https:// (or http:// for a loopback address)")]
    UnsupportedScheme { scheme: String },
    #[error("the controller URL must not contain a user name or password")]
    UserInfo,
    #[error("the controller URL has no host")]
    MissingHost,
    #[error("`{host}` is not a valid host name or IP address")]
    InvalidHost { host: String },
    #[error("an IPv6 address must be written in brackets, for example https://[::1]:7420")]
    Ipv6NeedsBrackets,
    #[error("IPv6 zone identifiers (`%…`) are not supported in a controller URL")]
    Ipv6Zone,
    #[error("`{port}` is not a valid port (1-65535)")]
    InvalidPort { port: String },
    #[error("the controller URL must end after the port: remove the path")]
    UnexpectedPath,
    #[error("the controller URL must end after the port: remove the `?` query")]
    UnexpectedQuery,
    #[error("the controller URL must end after the port: remove the `#` fragment")]
    UnexpectedFragment,
    #[error(
        "plain http:// to `{host}` is refused because it would send source code and credentials \
         unencrypted; use https://, or pass the explicit insecure option if this network is trusted"
    )]
    InsecureNotAllowed { host: String },
}

/// A validated host: an IP address or a DNS/mDNS name in lower case.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Host(HostKind);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HostKind {
    Ip(IpAddr),
    Name(DnsName<'static>),
}

impl Host {
    /// Parses an IPv4 address, an IPv6 address (with or without brackets)
    /// or a host name.
    ///
    /// IPv4 addresses must be four decimal parts; shortened, octal and
    /// hexadecimal spellings (`127.1`, `0x7f.0.0.1`) are rejected instead
    /// of being treated as names, because resolvers disagree about them.
    /// Names are ASCII labels of letters, digits, `-` and `_`, without a
    /// trailing dot.
    pub fn parse(input: &str) -> Result<Host, UrlError> {
        let invalid = || UrlError::InvalidHost {
            host: input.to_owned(),
        };
        if input.is_empty() {
            return Err(UrlError::MissingHost);
        }
        if let Some(inner) = input.strip_prefix('[') {
            let inner = inner.strip_suffix(']').ok_or_else(invalid)?;
            return parse_ipv6(inner).ok_or_else(|| {
                if inner.contains('%') {
                    UrlError::Ipv6Zone
                } else {
                    invalid()
                }
            });
        }
        if input.contains(':') {
            return parse_ipv6(input).ok_or_else(|| {
                if input.contains('%') {
                    UrlError::Ipv6Zone
                } else {
                    invalid()
                }
            });
        }
        if let Ok(v4) = input.parse::<Ipv4Addr>() {
            return Ok(Host(HostKind::Ip(IpAddr::V4(v4))));
        }
        if input.ends_with('.') {
            return Err(invalid());
        }
        // `DnsName` also refuses names whose last label is numeric, which
        // is what keeps malformed IPv4 spellings out.
        let name = DnsName::try_from(input).map_err(|_| invalid())?;
        Ok(Host(HostKind::Name(name.to_lowercase_owned())))
    }

    /// The address, when the host is an IP address.
    pub fn ip(&self) -> Option<IpAddr> {
        match &self.0 {
            HostKind::Ip(ip) => Some(*ip),
            HostKind::Name(_) => None,
        }
    }

    /// The name, when the host is not an IP address.
    pub fn name(&self) -> Option<&str> {
        match &self.0 {
            HostKind::Ip(_) => None,
            HostKind::Name(name) => Some(name.as_ref()),
        }
    }

    /// Whether traffic to this host stays on the local machine.
    ///
    /// True for `127.0.0.0/8`, `::1`, an IPv4-mapped IPv6 loopback address,
    /// and the exact name `localhost`. Other names are never loopback, not
    /// even `*.localhost`: this crate does not resolve names, and only
    /// `localhost` itself is reliably bound to the loopback interface.
    pub fn is_loopback(&self) -> bool {
        match &self.0 {
            HostKind::Ip(IpAddr::V4(v4)) => v4.is_loopback(),
            HostKind::Ip(IpAddr::V6(v6)) => v6.to_canonical().is_loopback(),
            HostKind::Name(name) => name.as_ref() == "localhost",
        }
    }

    /// The name a TLS client passes to rustls for this host. The pinned
    /// verifier ignores it; rustls still requires one.
    pub fn server_name(&self) -> ServerName<'static> {
        match &self.0 {
            HostKind::Ip(ip) => ServerName::IpAddress((*ip).into()),
            HostKind::Name(name) => ServerName::DnsName(name.clone()),
        }
    }
}

/// The host as it is passed to a resolver or socket: IPv6 addresses are
/// written without brackets.
impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            HostKind::Ip(ip) => write!(f, "{ip}"),
            HostKind::Name(name) => f.write_str(name.as_ref()),
        }
    }
}

fn parse_ipv6(input: &str) -> Option<Host> {
    input
        .parse::<Ipv6Addr>()
        .ok()
        .map(|v6| Host(HostKind::Ip(IpAddr::V6(v6))))
}

/// Whether `host` (an IP address, optionally bracketed, or a name) refers
/// to the local machine. See [`Host::is_loopback`]. Anything that is not a
/// valid host is not loopback.
pub fn is_loopback(host: &str) -> bool {
    Host::parse(host).is_ok_and(|host| host.is_loopback())
}

/// The base address of a controller: scheme, host and port, nothing else.
///
/// A value of this type has passed the transport rule from decision D5:
/// `http` appears only with a loopback host, or because the caller passed
/// `allow_insecure`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControllerUrl {
    scheme: Scheme,
    host: Host,
    port: u16,
}

impl ControllerUrl {
    /// Parses `https://host[:port]` or `http://host[:port]`.
    ///
    /// The port defaults to `bella_core::defaults::CONTROLLER_PORT`. A
    /// single trailing `/` is accepted; any path, query, fragment or user
    /// information is an error. Surrounding whitespace is ignored.
    ///
    /// `http` is accepted for a loopback host ([`Host::is_loopback`]). For
    /// any other host it is accepted only when `allow_insecure` is true,
    /// which must come from an explicit choice by the person running the
    /// program and never from a default.
    pub fn parse(input: &str, allow_insecure: bool) -> Result<ControllerUrl, UrlError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(UrlError::Empty);
        }
        if input.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(UrlError::IllegalCharacter);
        }
        let (scheme_text, rest) = input.split_once("://").ok_or(UrlError::MissingScheme)?;
        let scheme = if scheme_text.eq_ignore_ascii_case("https") {
            Scheme::Https
        } else if scheme_text.eq_ignore_ascii_case("http") {
            Scheme::Http
        } else {
            return Err(UrlError::UnsupportedScheme {
                scheme: sanitize_scheme(scheme_text),
            });
        };
        let (host, port) = parse_authority(rest)?;
        ControllerUrl::new(scheme, host, port, allow_insecure)
    }

    /// Builds a URL from parts, applying the same transport rule as
    /// [`ControllerUrl::parse`]. Port 0 is not a usable address.
    pub fn new(
        scheme: Scheme,
        host: Host,
        port: u16,
        allow_insecure: bool,
    ) -> Result<ControllerUrl, UrlError> {
        if port == 0 {
            return Err(UrlError::InvalidPort {
                port: "0".to_owned(),
            });
        }
        if scheme == Scheme::Http && !host.is_loopback() && !allow_insecure {
            return Err(UrlError::InsecureNotAllowed {
                host: host.to_string(),
            });
        }
        Ok(ControllerUrl { scheme, host, port })
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether connections use TLS.
    pub fn is_tls(&self) -> bool {
        self.scheme == Scheme::Https
    }

    /// Whether the host is the local machine. See [`Host::is_loopback`].
    pub fn is_loopback(&self) -> bool {
        self.host.is_loopback()
    }

    /// Whether this URL carries traffic unencrypted to another machine,
    /// which is only possible when it was built with `allow_insecure`.
    pub fn is_insecure_remote(&self) -> bool {
        self.scheme == Scheme::Http && !self.host.is_loopback()
    }

    /// `host:port`, with an IPv6 address in brackets.
    pub fn authority(&self) -> String {
        match self.host.ip() {
            Some(IpAddr::V6(v6)) => format!("[{v6}]:{}", self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }

    /// The TLS server name for this host. See [`Host::server_name`].
    pub fn server_name(&self) -> ServerName<'static> {
        self.host.server_name()
    }
}

/// `scheme://host:port`, always with the port and without a trailing slash.
impl fmt::Display for ControllerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme.as_str(), self.authority())
    }
}

/// Splits `host[:port][/]` and rejects everything else a URL could carry.
pub(crate) fn parse_authority(rest: &str) -> Result<(Host, u16), UrlError> {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match tail.as_bytes().first() {
        None => {}
        Some(b'/') if tail.len() == 1 => {}
        Some(b'/') => {
            let after_slash = &tail[1..];
            return Err(match after_slash.as_bytes().first() {
                Some(b'?') => UrlError::UnexpectedQuery,
                Some(b'#') => UrlError::UnexpectedFragment,
                _ => UrlError::UnexpectedPath,
            });
        }
        Some(b'?') => return Err(UrlError::UnexpectedQuery),
        Some(_) => return Err(UrlError::UnexpectedFragment),
    }
    if authority.contains('@') {
        return Err(UrlError::UserInfo);
    }
    if authority.is_empty() {
        return Err(UrlError::MissingHost);
    }

    let (host_text, port_text) = if authority.starts_with('[') {
        let close = authority.find(']').ok_or_else(|| UrlError::InvalidHost {
            host: authority.to_owned(),
        })?;
        let (host_text, after) = authority.split_at(close + 1);
        match after.strip_prefix(':') {
            Some(port) => (host_text, Some(port)),
            None if after.is_empty() => (host_text, None),
            None => {
                return Err(UrlError::InvalidHost {
                    host: authority.to_owned(),
                });
            }
        }
    } else {
        match authority.matches(':').count() {
            0 => (authority, None),
            1 => {
                let (host_text, port) = authority.split_once(':').unwrap_or((authority, ""));
                (host_text, Some(port))
            }
            _ => return Err(UrlError::Ipv6NeedsBrackets),
        }
    };

    let host = Host::parse(host_text)?;
    let port = match port_text {
        None => CONTROLLER_PORT,
        Some(text) => parse_port(text)?,
    };
    Ok((host, port))
}

fn parse_port(text: &str) -> Result<u16, UrlError> {
    let invalid = || UrlError::InvalidPort {
        port: text.to_owned(),
    };
    // `u16::from_str` would accept a leading `+`.
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(invalid()),
    }
}

/// Keeps an echoed scheme short and printable.
fn sanitize_scheme(scheme: &str) -> String {
    scheme
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .take(16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict(input: &str) -> Result<ControllerUrl, UrlError> {
        ControllerUrl::parse(input, false)
    }

    #[test]
    fn https_urls_parse_and_print_canonically() {
        let url = strict("https://build-box.local:9000").unwrap();
        assert_eq!(url.scheme(), Scheme::Https);
        assert_eq!(url.host().name(), Some("build-box.local"));
        assert_eq!(url.port(), 9000);
        assert!(url.is_tls());
        assert!(!url.is_loopback());
        assert!(!url.is_insecure_remote());
        assert_eq!(url.to_string(), "https://build-box.local:9000");
        assert_eq!(url.authority(), "build-box.local:9000");

        // Scheme and host are case-insensitive; a lone trailing slash and
        // surrounding whitespace are tolerated.
        assert_eq!(strict("  HTTPS://Build-Box.LOCAL:9000/\n"), Ok(url));
    }

    #[test]
    fn the_port_defaults_to_the_controller_port() {
        assert_eq!(CONTROLLER_PORT, 7420);
        let url = strict("https://10.1.2.3").unwrap();
        assert_eq!(url.port(), CONTROLLER_PORT);
        assert_eq!(url.to_string(), "https://10.1.2.3:7420");
        assert_eq!(
            url.host().ip(),
            Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)))
        );
        assert_eq!(strict("https://10.1.2.3:7420"), Ok(url));
    }

    #[test]
    fn ipv6_needs_brackets_and_prints_with_them() {
        let url = strict("https://[fd00::12]:8443").unwrap();
        assert_eq!(url.to_string(), "https://[fd00::12]:8443");
        assert_eq!(url.host().to_string(), "fd00::12");
        assert_eq!(
            strict("https://[fd00::12]").unwrap().to_string(),
            "https://[fd00::12]:7420"
        );
        assert_eq!(strict("https://fd00::12"), Err(UrlError::Ipv6NeedsBrackets));
        assert_eq!(strict("https://[fe80::1%en0]:1"), Err(UrlError::Ipv6Zone));
        assert!(matches!(
            strict("https://[fd00::12"),
            Err(UrlError::InvalidHost { .. })
        ));
        assert!(matches!(
            strict("https://[fd00::12]x"),
            Err(UrlError::InvalidHost { .. })
        ));
        assert!(matches!(
            strict("https://[10.1.2.3]"),
            Err(UrlError::InvalidHost { .. })
        ));
    }

    #[test]
    fn plain_http_is_allowed_on_loopback_only() {
        for input in [
            "http://127.0.0.1:7420",
            "http://127.8.9.10",
            "http://localhost:7420",
            "http://LOCALHOST",
            "http://[::1]:7420",
            "http://[::ffff:127.0.0.1]",
        ] {
            let url = strict(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert!(url.is_loopback(), "{input}");
            assert!(!url.is_tls());
            assert!(!url.is_insecure_remote());
        }
    }

    #[test]
    fn plain_http_to_a_lan_address_is_rejected_without_the_flag() {
        for (input, host) in [
            ("http://192.168.50.7:7420", "192.168.50.7"),
            ("http://10.1.2.3", "10.1.2.3"),
            ("http://build-box.local:7420", "build-box.local"),
            ("http://[fd00::12]:7420", "fd00::12"),
            ("http://0.0.0.0:7420", "0.0.0.0"),
            // Names that merely look local are not loopback.
            ("http://app.localhost", "app.localhost"),
            ("http://localhost.example.com", "localhost.example.com"),
            ("http://[::ffff:10.1.2.3]", "::ffff:10.1.2.3"),
        ] {
            assert_eq!(
                strict(input),
                Err(UrlError::InsecureNotAllowed {
                    host: host.to_owned()
                }),
                "{input}"
            );
        }
        let message = strict("http://192.168.50.7").unwrap_err().to_string();
        assert!(message.contains("192.168.50.7"));
        assert!(message.contains("unencrypted"));
    }

    #[test]
    fn the_explicit_flag_allows_plain_http_and_is_visible_afterwards() {
        let url = ControllerUrl::parse("http://192.168.50.7:7420", true).unwrap();
        assert_eq!(url.scheme(), Scheme::Http);
        assert!(url.is_insecure_remote());
        assert_eq!(url.to_string(), "http://192.168.50.7:7420");
        // The flag does not change how https is treated.
        let secure = ControllerUrl::parse("https://192.168.50.7", true).unwrap();
        assert!(secure.is_tls());
        assert!(!secure.is_insecure_remote());
    }

    #[test]
    fn building_from_parts_applies_the_same_rule() {
        let lan = Host::parse("192.168.50.7").unwrap();
        assert!(matches!(
            ControllerUrl::new(Scheme::Http, lan.clone(), 7420, false),
            Err(UrlError::InsecureNotAllowed { .. })
        ));
        assert!(ControllerUrl::new(Scheme::Http, lan.clone(), 7420, true).is_ok());
        assert!(ControllerUrl::new(Scheme::Https, lan.clone(), 7420, false).is_ok());
        assert!(matches!(
            ControllerUrl::new(Scheme::Https, lan, 0, false),
            Err(UrlError::InvalidPort { .. })
        ));
    }

    #[test]
    fn only_scheme_host_and_port_are_allowed() {
        assert_eq!(strict(""), Err(UrlError::Empty));
        assert_eq!(strict("   "), Err(UrlError::Empty));
        assert_eq!(strict("10.1.2.3:7420"), Err(UrlError::MissingScheme));
        assert_eq!(strict("https:/10.1.2.3"), Err(UrlError::MissingScheme));
        assert_eq!(
            strict("ftp://10.1.2.3"),
            Err(UrlError::UnsupportedScheme {
                scheme: "ftp".to_owned()
            })
        );
        assert_eq!(strict("https://"), Err(UrlError::MissingHost));
        assert_eq!(strict("https://:7420"), Err(UrlError::MissingHost));
        assert_eq!(strict("https://host/api/v1"), Err(UrlError::UnexpectedPath));
        assert_eq!(strict("https://host//"), Err(UrlError::UnexpectedPath));
        assert_eq!(strict("https://host?x=1"), Err(UrlError::UnexpectedQuery));
        assert_eq!(strict("https://host/?x=1"), Err(UrlError::UnexpectedQuery));
        assert_eq!(
            strict("https://host#frag"),
            Err(UrlError::UnexpectedFragment)
        );
        assert_eq!(
            strict("https://host/#frag"),
            Err(UrlError::UnexpectedFragment)
        );
        assert_eq!(strict("https://user:pw@host"), Err(UrlError::UserInfo));
        assert_eq!(strict("https://ho st"), Err(UrlError::IllegalCharacter));
        assert_eq!(strict("https://host\u{7}"), Err(UrlError::IllegalCharacter));
    }

    #[test]
    fn errors_do_not_repeat_secrets_from_the_input() {
        for input in [
            "https://user:hunter2@host",
            "https://host/?token=hunter2",
            "https://host/hunter2",
            "https://host#hunter2",
        ] {
            let message = strict(input).unwrap_err().to_string();
            assert!(!message.contains("hunter2"), "{message}");
        }
    }

    #[test]
    fn ports_are_validated_strictly() {
        for port in [
            "0", "65536", "99999", "123456", "+80", "-1", "8o", "", " 80",
        ] {
            let input = format!("https://host:{port}");
            let result = strict(&input);
            assert!(
                matches!(
                    result,
                    Err(UrlError::InvalidPort { .. }) | Err(UrlError::IllegalCharacter)
                ),
                "{input}: {result:?}"
            );
        }
        assert_eq!(strict("https://host:1").unwrap().port(), 1);
        assert_eq!(strict("https://host:65535").unwrap().port(), 65535);
        assert_eq!(strict("https://host:07420").unwrap().port(), 7420);
    }

    #[test]
    fn hosts_are_validated() {
        for host in [
            "127.1",
            "0x7f.0.0.1",
            "256.1.1.1",
            "1.2.3",
            "host.",
            "-host",
            "host-",
            "a..b",
            "bücher.local",
            "host name",
            "*.local",
        ] {
            assert!(
                Host::parse(host).is_err(),
                "{host:?} should not be a valid host"
            );
        }
        assert!(Host::parse(&"a".repeat(64)).is_err());
        assert!(Host::parse(&"a".repeat(63)).is_ok());
        assert_eq!(
            Host::parse("Build_Box-2.Local").unwrap().name(),
            Some("build_box-2.local")
        );
        assert_eq!(Host::parse(""), Err(UrlError::MissingHost));
    }

    #[test]
    fn loopback_helper_matches_the_documented_set() {
        for host in [
            "127.0.0.1",
            "127.255.255.254",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
            "localhost",
            "LocalHost",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "",
            "0.0.0.0",
            "::",
            "10.1.2.3",
            "192.168.50.7",
            "169.254.1.1",
            "128.0.0.1",
            "localhost.",
            "localhost.localdomain",
            "app.localhost",
            "127.1",
            "not a host",
            "::ffff:10.1.2.3",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn server_names_follow_the_host_kind() {
        let by_ip = strict("https://10.1.2.3").unwrap().server_name();
        assert!(matches!(by_ip, ServerName::IpAddress(_)));
        let by_name = strict("https://Build-Box.local").unwrap().server_name();
        match by_name {
            ServerName::DnsName(name) => assert_eq!(name.as_ref(), "build-box.local"),
            other => panic!("unexpected server name {other:?}"),
        }
    }
}
