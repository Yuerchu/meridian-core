//! Untrusted OneBot media is never a local path or a general-purpose URL fetch.
//!
//! Private origins require an exact origin + fixed-address grant from local
//! administrator-owned config. Empty grants preserve public-only default deny.
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use super::PrivateMediaOrigin;
use futures::{Stream, StreamExt};

pub(super) fn validate_origins(origins: &[PrivateMediaOrigin]) -> Result<(), String> {
    if origins.len() > 32 {
        return Err("at most 32 private media origins are allowed".into());
    }
    let mut seen = std::collections::HashSet::new();
    for entry in origins {
        let url = parse_url(&entry.origin)?;
        let authority = entry
            .origin
            .split_once("://")
            .map(|(_, rest)| rest.trim_end_matches('/'))
            .unwrap_or("");
        if authority.contains(['/', '?', '#', '*'])
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !seen.insert(url.origin().ascii_serialization())
        {
            return Err(
                "private media grants require distinct exact origins without paths, queries or fragments".into(),
            );
        }
        // No catch-all destinations, multicast, broadcast or mapped addresses.
        if entry.ip.is_unspecified()
            || entry.ip.is_multicast()
            || matches!(entry.ip, IpAddr::V4(ip) if ip.is_broadcast())
            || matches!(entry.ip, IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some())
        {
            return Err("private media grant requires a fixed unicast IP".into());
        }
        if let Ok(literal) = url.host_str().unwrap_or("").trim_matches(['[', ']']).parse::<IpAddr>()
            && literal != entry.ip
        {
            return Err("private media literal origin must match its fixed IP".into());
        }
    }
    Ok(())
}

pub(super) struct Download {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

fn parse_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|_| "invalid media URL")?;
    // Check the original authority too: Url normalises an empty `user@` away.
    let authority = raw
        .split_once(':')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(url.scheme()))
        .and_then(|(_, rest)| rest.strip_prefix("//"))
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or(""));
    if raw.trim() != raw
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || authority.is_none_or(|a| a.is_empty() || a.contains('@') || a.contains('\\'))
        || raw.contains('\\')
        || raw.chars().any(char::is_control)
    {
        return Err("media URL requires HTTP(S) without userinfo".into());
    }
    Ok(url)
}

/// Conservative special-purpose exclusions. std::net::IpAddr::is_global is
/// still unstable on the pinned Rust 1.98.1, and adding a dependency is outside
/// this patch's scope. Prefer a maintained classification facility when one is
/// available. Reject entire special-purpose blocks, including their exceptions,
/// rather than accidentally admitting an internal/transition destination.
fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let n = u32::from(ip);
            const DENIED: &[(u32, u32)] = &[
                (0x00000000, 8),
                (0x0a000000, 8),
                (0x64400000, 10),
                (0x7f000000, 8),
                (0xa9fe0000, 16),
                (0xac100000, 12),
                (0xc0000000, 24),
                (0xc0000200, 24),
                (0xc0586300, 24),
                (0xc0a80000, 16),
                (0xc6120000, 15),
                (0xc6336400, 24),
                (0xcb007100, 24),
                (0xe0000000, 3),
            ];
            !DENIED
                .iter()
                .any(|&(network, prefix)| n >> (32 - prefix) == network >> (32 - prefix))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Only native global unicast, not mapped/compatible IPv4, NAT64,
            // local, multicast or other reserved space. Exclude IETF protocol
            // assignments, documentation, 6to4 and the newer documentation /20.
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

fn validate_addresses(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    if addresses.is_empty() || addresses.len() > 64 || addresses.iter().any(|a| !public_address(a.ip())) {
        return Err("media DNS must resolve exclusively to public addresses".into());
    }
    Ok(addresses)
}

pub(super) async fn fetch(
    raw: &str,
    max_bytes: u64,
    deadline: Duration,
    origins: &[PrivateMediaOrigin],
) -> Result<Download, String> {
    fetch_with(
        origins,
        raw,
        max_bytes,
        deadline,
        |host, port| async move {
            tokio::net::lookup_host((host.as_str(), port))
                .await
                .map(|addresses| addresses.take(65).collect())
                .map_err(|_| "media DNS lookup failed".into())
        },
        |request| async move { request.send().await.map_err(|_| "media request failed".into()) },
    )
    .await
}

// Test seams replace only DNS and sending, never validation/client policy or
// the enclosing deadline. Production and synthetic tests exercise one path.
async fn fetch_with<R, RF, S, SF>(
    origins: &[PrivateMediaOrigin],
    raw: &str,
    max_bytes: u64,
    deadline: Duration,
    resolve: R,
    send: S,
) -> Result<Download, String>
where
    R: FnOnce(String, u16) -> RF,
    RF: std::future::Future<Output = Result<Vec<SocketAddr>, String>>,
    S: FnOnce(reqwest::RequestBuilder) -> SF,
    SF: std::future::Future<Output = Result<reqwest::Response, String>>,
{
    tokio::time::timeout(deadline, async {
        validate_origins(origins)?;
        let url = parse_url(raw)?;
        let host = url.host_str().ok_or("missing media host")?;
        let port = url.port_or_known_default().ok_or("missing media port")?;
        let grant = origins.iter().find(|entry| {
            // All entries were validated above. Keep URL host for TLS + Host.
            parse_url(&entry.origin).is_ok_and(|origin| origin.origin() == url.origin())
        });
        let addresses = match grant {
            Some(entry) => vec![SocketAddr::new(entry.ip, port)],
            None => {
                let addresses = match host.trim_matches(['[', ']']).parse::<IpAddr>() {
                    Ok(ip) => vec![SocketAddr::new(ip, port)],
                    Err(_) => resolve(host.to_owned(), port).await?,
                };
                validate_addresses(addresses)?
            }
        };
        // A fresh, direct client: no environment proxy, redirect, shared pool or
        // second DNS lookup. Keep the URL host intact for Host and TLS verification.
        let client = pinned_client(host, &addresses, deadline)
            .build()
            .map_err(|_| "could not create media client")?;
        let response = send(client.get(url)).await?;
        if !response.status().is_success() {
            return Err(format!("media HTTP {}", response.status().as_u16()));
        }
        if response.content_length().is_some_and(|n| n > max_bytes) {
            return Err("media too large".into());
        }
        for value in response.headers().get_all(reqwest::header::CONTENT_LENGTH) {
            let length = value
                .to_str()
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or("invalid media content length")?;
            if length > max_bytes {
                return Err("media too large".into());
            }
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = collect_body(response.bytes_stream(), max_bytes).await?;
        Ok(Download { bytes, content_type })
    })
    .await
    .map_err(|_| "media download timed out".to_string())?
}

fn pinned_client(host: &str, addresses: &[SocketAddr], deadline: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .resolve_to_addrs(host, addresses)
        .connect_timeout(Duration::from_secs(10))
        .timeout(deadline)
}

async fn collect_body<S, E>(stream: S, limit: u64) -> Result<Vec<u8>, String>
where
    S: Stream<Item = Result<bytes::Bytes, E>>,
{
    futures::pin_mut!(stream);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "media body read failed")?;
        if chunk.len() as u64 > limit.saturating_sub(bytes.len() as u64) {
            return Err("media too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(origin: &str, ip: &str) -> PrivateMediaOrigin {
        PrivateMediaOrigin {
            origin: origin.into(),
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn grants_require_distinct_exact_origins_and_fixed_ips() {
        for (origin, ip) in [
            ("http://local.test/path", "127.0.0.1"),
            ("http://local.test/a/..", "127.0.0.1"),
            ("http://*.local.test", "127.0.0.1"),
            ("http://local.test?x=1", "127.0.0.1"),
            ("http://local.test#fragment", "127.0.0.1"),
            ("file://local.test", "127.0.0.1"),
            ("http://127.0.0.1", "127.0.0.2"),
            ("http://local.test", "0.0.0.0"),
            ("http://local.test", "224.0.0.1"),
            ("http://local.test", "::ffff:127.0.0.1"),
        ] {
            assert!(validate_origins(&[grant(origin, ip)]).is_err(), "{origin} {ip}");
        }
        assert!(
            validate_origins(&[
                grant("https://local.test", "127.0.0.1"),
                grant("https://local.test:443/", "127.0.0.2"),
            ])
            .is_err()
        );
        assert!(
            serde_json::from_str::<PrivateMediaOrigin>(
                r#"{"origin":"http://local.test","ip":"127.0.0.1","cidr":"127.0.0.0/8"}"#
            )
            .is_err()
        );
        assert!(validate_origins(&[grant("https://local.test:8443", "192.168.1.2")]).is_ok());
    }

    #[tokio::test]
    async fn grant_matches_only_scheme_host_and_effective_port() {
        let grants = [grant("https://ADAPTER.invalid:8443/", "192.168.1.2")];
        let result = fetch_with(
            &grants,
            "https://adapter.invalid:8443/path?query=yes",
            32,
            Duration::from_secs(1),
            |_, _| async { panic!("matching grant must skip DNS") },
            |builder| async {
                let request = builder.build().unwrap();
                assert_eq!(request.url().host_str(), Some("adapter.invalid"));
                Ok(response(200, None, reqwest::Body::from("ok")))
            },
        )
        .await
        .unwrap();
        assert_eq!(result.bytes, b"ok");
        for url in [
            "http://adapter.invalid:8443/path",
            "https://adapter.invalid/path",
            "https://other.invalid:8443/path",
        ] {
            let result = fetch_with(
                &grants,
                url,
                32,
                Duration::from_secs(1),
                |_, port| async move { Ok(vec![SocketAddr::new("192.168.1.2".parse().unwrap(), port)]) },
                |_| async { panic!("different origin must not get private exception") },
            )
            .await;
            assert!(result.is_err(), "{url}");
        }
        assert!(
            fetch_with(
                &grants,
                "https://adapter.invalid:8443/path",
                32,
                Duration::from_secs(1),
                |_, _| async { panic!("matching grant must skip DNS") },
                |_| async { Ok(response(302, None, reqwest::Body::from(""))) },
            )
            .await
            .is_err(),
            "granted origins cannot redirect"
        );
    }

    #[tokio::test]
    async fn granted_origin_really_pins_ip_and_preserves_host() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://adapter.invalid:{port}");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /media?q=1 http/1.1\r\n"), "{request}");
            assert!(
                request.contains(&format!("host: adapter.invalid:{port}\r\n")),
                "{request}"
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let result = fetch(
            &format!("{origin}/media?q=1"),
            32,
            Duration::from_secs(2),
            &[grant(&origin, "127.0.0.1")],
        )
        .await
        .unwrap();
        assert_eq!(result.bytes, b"ok");
        server.await.unwrap();
    }

    #[test]
    fn urls_are_not_paths_or_credentials() {
        for raw in [
            "file:///etc/passwd",
            "C:/audio.wav",
            "//example.test/a",
            "ftp://example.test/a",
            "http://name@example.test/a",
            "http://:pw@example.test/a",
            "http://@example.test/a",
            "http:////@example.test/a",
            "https:////example.test/a",
            "http:example.test/path://synthetic",
            "https:example.test/?next=scheme://synthetic",
            "http://example.test\\@localhost/a",
            " http://example.test/a",
            "http://example.test/\n",
        ] {
            assert!(parse_url(raw).is_err(), "{raw:?}");
        }
        assert!(parse_url("https://example.test:8443/image?a=1").is_ok());
        assert!(parse_url("HTTPS://example.test/image").is_ok());
    }

    #[test]
    fn special_addresses_and_mixed_dns_fail_closed() {
        for raw in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.0.9",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001::1",
            "2001:db8::1",
            "2002:808:808::1",
            "3fff::1",
        ] {
            assert!(!public_address(raw.parse().unwrap()), "{raw}");
        }
        let public: SocketAddr = "8.8.8.8:443".parse().unwrap();
        let private: SocketAddr = "127.0.0.1:443".parse().unwrap();
        assert!(validate_addresses(vec![public]).is_ok());
        assert!(public_address("2606:4700:4700::1111".parse().unwrap()));
        assert!(validate_addresses(vec![public, private]).is_err());
        assert!(validate_addresses(vec![private, public]).is_err());
        assert!(validate_addresses(vec![]).is_err());
        assert!(validate_addresses(vec![public; 65]).is_err());
    }

    #[tokio::test]
    async fn synthetic_chunk_stream_enforces_actual_byte_bound() {
        let chunks = || {
            futures::stream::iter([
                Ok::<_, ()>(bytes::Bytes::from_static(b"abc")),
                Ok(bytes::Bytes::from_static(b"de")),
            ])
        };
        assert_eq!(collect_body(chunks(), 5).await.unwrap(), b"abcde");
        assert!(collect_body(chunks(), 4).await.is_err());
        assert!(
            collect_body(futures::stream::iter([Err::<bytes::Bytes, _>(())]), 5)
                .await
                .is_err()
        );
    }

    fn response(status: u16, length: Option<u64>, body: reqwest::Body) -> reqwest::Response {
        let mut builder = hyper::Response::builder().status(status);
        if let Some(length) = length {
            builder = builder.header("content-length", length);
        }
        builder.body(body).unwrap().into()
    }

    async fn public_dns(host: String, port: u16) -> Result<Vec<SocketAddr>, String> {
        assert_eq!(host, "media.example.test");
        assert_eq!(port, 8443);
        Ok(vec![SocketAddr::new("8.8.8.8".parse().unwrap(), port)])
    }

    #[test]
    fn production_client_builder_contains_the_exact_pinned_addresses() {
        let addresses = ["8.8.8.8:8443".parse().unwrap(), "1.1.1.1:8443".parse().unwrap()];
        let builder = pinned_client("media.example.test", &addresses, Duration::from_secs(1));
        let policy = format!("{builder:?}");
        assert!(
            policy.contains("dns_overrides") && policy.contains("media.example.test"),
            "{policy}"
        );
        assert!(
            policy.contains("8.8.8.8:8443") && policy.contains("1.1.1.1:8443"),
            "{policy}"
        );
        assert!(policy.contains("Policy(None)"), "{policy}");
    }

    #[tokio::test]
    async fn synthetic_fetch_preserves_host_pins_dns_and_disables_redirects_and_proxies() {
        let result = fetch_with(
            &[],
            "https://media.example.test:8443/a",
            5,
            Duration::from_secs(1),
            public_dns,
            |request| async move {
                let (client, request) = request.build_split();
                assert_eq!(request.unwrap().url().as_str(), "https://media.example.test:8443/a");
                let policy = format!("{client:?}");
                assert!(policy.contains("Policy(None)"), "{policy}");
                assert!(!policy.contains("Proxy("), "{policy}");
                Ok(response(200, None, "abc".into()))
            },
        )
        .await
        .unwrap();
        assert_eq!(result.bytes, b"abc");
        let result = fetch_with(
            &[],
            "https://media.example.test:8443/a",
            5,
            Duration::from_secs(1),
            public_dns,
            |_| async {
                Ok(hyper::Response::builder()
                    .status(302)
                    .header("location", "http://127.0.0.1/never-follow")
                    .body(reqwest::Body::from(""))
                    .unwrap()
                    .into())
            },
        )
        .await;
        assert!(result.err().unwrap().contains("302"));
    }

    #[tokio::test]
    async fn synthetic_dns_validation_happens_before_any_request() {
        let result = fetch_with(
            &[],
            "https://media.example.test:8443/a",
            5,
            Duration::from_secs(1),
            |_, port| async move {
                Ok(vec![
                    SocketAddr::new("8.8.8.8".parse().unwrap(), port),
                    SocketAddr::new("127.0.0.1".parse().unwrap(), port),
                ])
            },
            |_| async { panic!("mixed DNS must never reach the connector") },
        )
        .await;
        assert!(result.err().unwrap().contains("exclusively"));
        let result = fetch_with(
            &[],
            "http://127.0.0.1/a",
            5,
            Duration::from_secs(1),
            |_, _| async { panic!("literal must not resolve") },
            |_| async { panic!("private literal must not connect") },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn synthetic_fetch_checks_declared_and_actual_length() {
        for (declared, body) in [(Some(6), "a"), (None, "abcdef")] {
            let result = fetch_with(
                &[],
                "https://media.example.test:8443/a",
                5,
                Duration::from_secs(1),
                public_dns,
                |_| async move { Ok(response(200, declared, body.into())) },
            )
            .await;
            assert!(result.err().unwrap().contains("too large"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn production_deadline_covers_dns_send_and_stalled_body() {
        let deadline = Duration::from_secs(1);
        let result = fetch_with(
            &[],
            "https://media.example.test:8443/a",
            5,
            deadline,
            |_, _| futures::future::pending(),
            |_| async { panic!("DNS never completed") },
        )
        .await;
        assert!(result.err().unwrap().contains("timed out"));
        let result = fetch_with(
            &[],
            "https://media.example.test:8443/a",
            5,
            deadline,
            public_dns,
            |_| futures::future::pending(),
        )
        .await;
        assert!(result.err().unwrap().contains("timed out"));
        let result = fetch_with(
            &[],
            "https://media.example.test:8443/a",
            5,
            deadline,
            public_dns,
            |_| async {
                let body =
                    reqwest::Body::wrap_stream(futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>());
                Ok(response(200, None, body))
            },
        )
        .await;
        assert!(result.err().unwrap().contains("timed out"));
    }
}
