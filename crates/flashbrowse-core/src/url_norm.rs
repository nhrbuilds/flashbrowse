//! Scheme-less URL resolution for the navigation entry points.
//!
//! Chrome's address bar resolves a bare host for you; `Page.navigate` does not.
//! It takes an absolute URL and rejects `krawlup.com` with
//! `-32000: Cannot navigate to invalid URL`, so every string that reaches
//! `open`, `dump`, `bench` or MCP `browser_open` passes through here first and
//! `flashbrowse open krawlup.com` behaves like the address bar it looks like.
//!
//! The resolution is deliberately conservative:
//!
//! - A URL that already has a scheme we can navigate is returned exactly as
//!   typed, `file:` included. The MCP allowlist runs on the normalized value,
//!   so `file://` still reaches it and is still rejected there.
//! - A token before the first `:` that is not a scheme but a port is a host.
//!   `localhost:8099` is the reason this distinction exists: it parses as a
//!   URI scheme named `localhost`, which is not what the user meant.
//! - Local hosts get `http://`, everything else gets `https://`. A dev server
//!   on `localhost:3000` or a router on `192.168.1.1` is plain HTTP in
//!   practice, and assuming HTTPS there fails in a confusing way.
//! - An IPv6 literal is bracketed so it is a valid authority: `::1` becomes
//!   `http://[::1]`. A bracketed address is left exactly as the user typed it.
//! - Input that cannot be a URL (a search query, a bare path) is an error that
//!   names the fix, instead of a raw CDP error after a browser has launched.

use url::Url;

/// Schemes that are passed through untouched. These are the ones Chrome can
/// navigate; `ws:`, `mailto:` and friends are not.
const KNOWN_SCHEMES: &[&str] = &[
    "http",
    "https",
    "file",
    "data",
    "about",
    "chrome",
    "view-source",
    "blob",
];

/// Why a string could not be turned into a navigable URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// Nothing but whitespace.
    Empty,
    /// Embedded control characters (NUL, newline, ESC, ...).
    ControlChars,
    /// A scheme Chrome cannot navigate, e.g. `mailto:` or `javascript:`.
    UnsupportedScheme(String),
    /// Not a URL at all. The hint names the fix.
    NotAUrl {
        /// The input, trimmed.
        input: String,
        /// What to do instead.
        hint: &'static str,
    },
}

impl std::fmt::Display for UrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "url must not be empty"),
            Self::ControlChars => write!(f, "url contains control characters"),
            Self::UnsupportedScheme(s) => write!(
                f,
                "unsupported URL scheme '{s}' (allowed: {})",
                KNOWN_SCHEMES.join(", ")
            ),
            Self::NotAUrl { input, hint } => write!(f, "not a URL: '{input}' ({hint})"),
        }
    }
}

impl std::error::Error for UrlError {}

/// Turn user input into a URL `Page.navigate` accepts.
///
/// Returns the input unchanged when it already has a navigable scheme, adds
/// `https://` for a bare host and `http://` for a local one, and rejects
/// everything else with an error that says what to type instead.
///
/// ```
/// # use flashbrowse_core::normalize_url;
/// assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
/// assert_eq!(normalize_url("localhost:8099").unwrap(), "http://localhost:8099");
/// assert!(normalize_url("/etc/passwd").is_err());
/// ```
pub fn normalize_url(input: &str) -> Result<String, UrlError> {
    let t = input.trim();
    if t.is_empty() {
        return Err(UrlError::Empty);
    }
    if t.chars().any(char::is_control) {
        return Err(UrlError::ControlChars);
    }

    if let Some((token, rest)) = split_scheme(t) {
        let lower = token.to_ascii_lowercase();
        if KNOWN_SCHEMES.contains(&lower.as_str()) {
            // Checked before the whitespace test: a `data:` URI may legally
            // contain spaces, and it must survive untouched.
            return Ok(t.to_string());
        }
        // `C:\path` is a Windows file, not the scheme `c`.
        if token.len() == 1 && rest.starts_with('\\') {
            return Err(UrlError::NotAUrl {
                input: t.to_string(),
                hint: "use file:///C:/... for local files",
            });
        }
        // `host:port` looks like a scheme. A numeric remainder means the colon
        // was a port separator, so fall through and treat it as a host.
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(UrlError::UnsupportedScheme(lower));
        }
    }

    if t.starts_with('/') || t.starts_with("./") || t.starts_with("../") || t.starts_with('~') {
        return Err(UrlError::NotAUrl {
            input: t.to_string(),
            hint: "use file:/// for local files",
        });
    }

    if t.chars().any(char::is_whitespace) {
        return Err(UrlError::NotAUrl {
            input: t.to_string(),
            hint: "flashbrowse opens URLs, not search queries; percent-encode spaces",
        });
    }

    let authority = host_of(t);
    let scheme = if is_local_host(host_without_port(authority)) {
        "http"
    } else {
        "https"
    };
    // A bare IPv6 literal has to be bracketed to be a valid authority. A
    // bracketed one is left exactly as the user typed it.
    let candidate = format!(
        "{scheme}://{}{}",
        bracket_ipv6_authority(authority),
        t.strip_prefix(authority).unwrap_or_default()
    );

    let parsed = Url::parse(&candidate).map_err(|_| UrlError::NotAUrl {
        input: t.to_string(),
        hint: "expected a host like example.com, or a full URL",
    })?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(UrlError::NotAUrl {
            input: t.to_string(),
            hint: "expected a host like example.com, or a full URL",
        });
    }
    Ok(candidate)
}

/// Split `scheme:rest` when `scheme` is syntactically a URI scheme.
fn split_scheme(input: &str) -> Option<(&str, &str)> {
    let (token, rest) = input.split_once(':')?;
    let mut chars = token.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((token, rest))
}

/// Authority portion: everything before the path, query or fragment.
fn host_of(input: &str) -> &str {
    input.split(['/', '?', '#']).next().unwrap_or_default()
}

/// Drop the port, keeping IPv6 brackets out of the way.
fn host_without_port(hostport: &str) -> &str {
    split_host_port(hostport).0
}

/// Split an authority into its host and its optional port.
///
/// A bracketed host (`[::1]:8080`) yields the host without the brackets. An
/// unbracketed authority holding more than one colon is a bare IPv6 literal:
/// RFC 3986 requires brackets for an IPv6 address in a URI, and nothing else
/// can put two colons inside a host.
fn split_host_port(hostport: &str) -> (&str, Option<&str>) {
    if let Some(rest) = hostport.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, tail)) => {
                let port = tail.strip_prefix(':').filter(|p| !p.is_empty());
                (host, port)
            }
            None => (rest, None),
        };
    }
    if hostport.matches(':').count() > 1 {
        return (hostport, None);
    }
    match hostport.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (hostport, None),
    }
}

/// Bracket a bare IPv6 authority so it parses as a URL.
///
/// `::1` becomes `[::1]`. An authority that is not an IPv6 literal is
/// returned untouched, brackets included.
fn bracket_ipv6_authority(authority: &str) -> String {
    if authority.starts_with('[') {
        return authority.to_string();
    }
    let (host, port) = split_host_port(authority);
    if !host.contains(':') {
        return authority.to_string();
    }
    match port {
        Some(p) => format!("[{host}]:{p}"),
        None => format!("[{host}]"),
    }
}

/// Loopback, unspecified, RFC1918, link-local and mDNS names are plain HTTP.
fn is_local_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    if h == "::1" || h == "::" || h == "0.0.0.0" {
        return true;
    }
    if h.ends_with(".local") {
        return true;
    }
    if let Some((a, b, _, _)) = parse_ipv4(&h) {
        return a == 127
            || a == 10
            || (a == 192 && b == 168)
            || (a == 172 && (16..=31).contains(&b))
            || (a == 169 && b == 254);
    }
    false
}

fn parse_ipv4(s: &str) -> Option<(u8, u8, u8, u8)> {
    let mut parts = s.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let c = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((a, b, c, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_host_defaults_to_https() {
        assert_eq!(normalize_url("krawlup.com").unwrap(), "https://krawlup.com");
        assert_eq!(
            normalize_url("  krawlup.com  ").unwrap(),
            "https://krawlup.com"
        );
        assert_eq!(
            normalize_url("example.com/a?b=1#c").unwrap(),
            "https://example.com/a?b=1#c"
        );
    }

    #[test]
    fn local_hosts_default_to_http() {
        for input in [
            "localhost",
            "localhost:8099",
            "app.localhost:3000",
            "127.0.0.1",
            "127.0.0.1:8099",
            "[::1]",
            "[::1]:3000",
            "0.0.0.0:8080",
            "192.168.1.5:8080",
            "10.0.0.2",
            "172.16.4.4:81",
            "169.254.1.1",
            "dev.local",
        ] {
            let out = normalize_url(input).unwrap();
            assert!(out.starts_with("http://"), "{input} -> {out}");
        }
    }

    #[test]
    fn bare_ipv6_loopback_is_bracketed_and_http() {
        // `::1` has to be bracketed to be a valid authority, and the README
        // promises it resolves like every other local host.
        assert_eq!(normalize_url("::1").unwrap(), "http://[::1]");
        assert_eq!(normalize_url("[::1]").unwrap(), "http://[::1]");
        assert_eq!(normalize_url("[::1]:3000").unwrap(), "http://[::1]:3000");
    }

    #[test]
    fn unbracketed_ipv6_with_a_colon_is_an_address() {
        // Two colons in an authority can only be an IPv6 literal: RFC 3986
        // requires brackets for an IPv6 address in a URI. The address reading
        // wins over a host:port reading, because `::1:8080` is itself a valid
        // IPv6 address and nothing in the input says which one was meant.
        assert_eq!(normalize_url("::1:8080").unwrap(), "https://[::1:8080]");
    }

    #[test]
    fn public_hosts_are_not_local() {
        // 172.15 and 172.32 sit outside the RFC1918 range on purpose.
        for input in ["8.8.8.8", "172.15.0.1", "172.32.0.1", "192.169.1.1"] {
            let out = normalize_url(input).unwrap();
            assert!(out.starts_with("https://"), "{input} -> {out}");
        }
    }

    #[test]
    fn explicit_schemes_are_untouched() {
        for input in [
            "http://example.com",
            "HTTPS://EXAMPLE.COM",
            "file:///tmp/x.html",
            "data:text/html,<b>hi</b>",
            "about:blank",
            "view-source:https://example.com",
            "blob:https://example.com/1",
        ] {
            assert_eq!(normalize_url(input).unwrap(), input, "{input}");
        }
    }

    #[test]
    fn data_uri_with_spaces_survives() {
        // The whitespace rejection must not apply once a known scheme is seen.
        let uri = "data:text/html,<h1>a b</h1>";
        assert_eq!(normalize_url(uri).unwrap(), uri);
    }

    #[test]
    fn host_port_is_not_a_scheme() {
        assert_eq!(
            normalize_url("example.com:8080").unwrap(),
            "https://example.com:8080"
        );
        assert_eq!(
            normalize_url("example.com:8080/path").unwrap(),
            "https://example.com:8080/path"
        );
    }

    #[test]
    fn unknown_schemes_are_rejected() {
        match normalize_url("javascript:alert(1)") {
            Err(UrlError::UnsupportedScheme(s)) => assert_eq!(s, "javascript"),
            other => panic!("javascript -> {other:?}"),
        }
        assert!(matches!(
            normalize_url("mailto:a@b.c"),
            Err(UrlError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            normalize_url("ftp://example.com"),
            Err(UrlError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn paths_are_rejected_with_a_file_hint() {
        for input in ["/etc/passwd", "./index.html", "../up.html", "~/x.html"] {
            match normalize_url(input) {
                Err(UrlError::NotAUrl { hint, .. }) => {
                    assert!(hint.contains("file:///"), "{input} -> {hint}");
                }
                other => panic!("{input} -> {other:?}"),
            }
        }
    }

    #[test]
    fn windows_path_is_a_file_not_a_scheme() {
        match normalize_url(r"C:\Users\x\page.html") {
            Err(UrlError::NotAUrl { hint, .. }) => assert!(hint.contains("file:///")),
            other => panic!("windows path -> {other:?}"),
        }
    }

    #[test]
    fn search_queries_are_rejected() {
        match normalize_url("hello world") {
            Err(UrlError::NotAUrl { hint, .. }) => assert!(hint.contains("search")),
            other => panic!("query -> {other:?}"),
        }
    }

    #[test]
    fn empty_and_control_chars_are_rejected() {
        assert_eq!(normalize_url("   "), Err(UrlError::Empty));
        assert_eq!(
            normalize_url("example.com\u{0}"),
            Err(UrlError::ControlChars)
        );
        assert_eq!(
            normalize_url("example.com\npath"),
            Err(UrlError::ControlChars)
        );
    }

    #[test]
    fn case_is_preserved_for_known_schemes() {
        assert_eq!(
            normalize_url("HTTP://Example.COM/Path").unwrap(),
            "HTTP://Example.COM/Path"
        );
    }
}
