//! URL checks for links in messages (N-6) and relay addresses from friend codes and settings
//! (N-7).

use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

/// Normalized form of a link that may be opened: `http`/`https` only, no user info
/// (`https://bank.example@evil.example`), host in ASCII (punycode), so that look-alike letters
/// from other scripts (`pаypal.com` with a Cyrillic "а") show up as `xn--...`. `None` if the
/// link should stay plain text.
pub fn link(raw: &str) -> Option<String> {
    let u = Url::parse(raw.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    u.host()?;
    Some(u.into())
}

/// `true` if the host of a normalized link uses non-ASCII letters (shown as punycode).
pub fn is_idn(normalized: &str) -> bool {
    Url::parse(normalized)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.split('.').any(|l| l.starts_with("xn--")))
        })
        .unwrap_or(false)
}

/// May the app contact this relay? Only `https` to a public host: no user info, no
/// `localhost`/`.local`, no IP literals from private, loopback, link-local or other
/// non-public ranges. Otherwise a crafted friend code could make the app send requests into
/// the local network (blind SSRF) or to plain-text endpoints.
pub fn is_public_https(raw: &str) -> bool {
    let Ok(u) = Url::parse(raw.trim()) else {
        return false;
    };
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    match u.host() {
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            let local_suffix = [
                ".localhost",
                ".local",
                ".lan",
                ".home",
                ".internal",
                ".intranet",
                ".corp",
                ".home.arpa",
            ];
            d.contains('.') && d != "localhost" && !local_suffix.iter().any(|s| d.ends_with(s))
        }
        Some(Host::Ipv4(ip)) => is_public_v4(ip),
        Some(Host::Ipv6(ip)) => is_public_v6(ip),
        None => false,
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        // 100.64.0.0/10 (Carrier-NAT), 198.18.0.0/15 (Benchmark), 240.0.0.0/4 (reserviert)
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
        || (o[0] == 198 && (o[1] & 0xFE) == 18)
        || o[0] >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xFE00) == 0xFC00 // fc00::/7 unique local
        || (s[0] & 0xFFC0) == 0xFE80 // fe80::/10 link-local
        || s[0] == 0x2001 && s[1] == 0x0DB8) // Dokumentation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_normalized_and_tricks_refused() {
        assert_eq!(
            link("https://example.com/a?b=1").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(
            link("HTTP://Example.COM").as_deref(),
            Some("http://example.com/")
        );
        assert_eq!(link("https://bank.example@evil.example/"), None);
        assert_eq!(link("https://user:pw@example.com/"), None);
        assert_eq!(link("javascript:alert(1)"), None);
        assert_eq!(link("file:///C:/Windows"), None);
        let idn = link("https://p\u{0430}ypal.com/").unwrap();
        assert!(idn.starts_with("https://xn--"), "{idn}");
        assert!(is_idn(&idn));
        assert!(!is_idn("https://example.com/"));
    }

    #[test]
    fn relays_must_be_public_https() {
        assert!(is_public_https("https://relay.example.org"));
        assert!(is_public_https("https://relay.example.org:8443/"));
        assert!(is_public_https("https://8.8.8.8/"));
        for bad in [
            "http://relay.example.org",
            "https://localhost",
            "https://printer.local",
            "https://router.home.arpa",
            "https://intranet",
            "https://192.168.0.1",
            "https://10.0.0.5",
            "https://127.0.0.1",
            "https://169.254.1.1",
            "https://100.64.1.1",
            "https://0.0.0.0",
            "https://[::1]",
            "https://[fd00::1]",
            "https://[fe80::1]",
            "https://[::ffff:192.168.0.1]",
            "https://u:p@relay.example.org",
            "ftp://relay.example.org",
            "kein url",
        ] {
            assert!(!is_public_https(bad), "{bad}");
        }
    }
}
