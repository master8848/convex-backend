use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    io,
    net::{
        IpAddr,
        Ipv4Addr,
        Ipv6Addr,
        SocketAddr,
    },
    pin::Pin,
    sync::{
        atomic::{
            AtomicU64,
            Ordering,
        },
        Arc,
        LazyLock,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use errors::ErrorMetadata;
use futures::{
    future::BoxFuture,
    Stream,
    StreamExt,
};
use futures_async_stream::try_stream;
use http::StatusCode;
use reqwest::{
    Body,
    Proxy,
    Url,
};
use tokio::select;
use url::Host;

use crate::http::{
    HttpRequestStream,
    HttpResponseStream,
};

/// Http client used for fetch syscall.
#[async_trait]
pub trait FetchClient: Send + Sync {
    async fn fetch(&self, request: HttpRequestStream) -> anyhow::Result<HttpResponseStream>;
}

// Share the underlying TlsConnector between ProxiedFetchClients
static TLS_CONNECTOR: LazyLock<native_tls::TlsConnector> = LazyLock::new(|| {
    let mut tls = native_tls::TlsConnector::builder();
    tls.request_alpns(&["h2", "http/1.1"]);
    tls.build().expect("failed to build TLS connector")
});

/// Creates a reqwest client builder configured with an optional proxy.
/// The client_id is set to the instance name for logging.
/// The redirect_policy dictates how redirects are handled.
fn build_proxied_reqwest_client_builder(
    proxy_url: Option<Url>,
    client_id: String,
    redirect_policy: reqwest::redirect::Policy,
) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder()
        .redirect(redirect_policy)
        .http2_keep_alive_interval(*crate::knobs::HTTP2_CLIENT_KEEPALIVE_INTERVAL)
        .http2_keep_alive_timeout(*crate::knobs::HTTP2_CLIENT_KEEPALIVE_TIMEOUT);
    // It's okay to panic on these errors, as they indicate a serious programming
    // error -- building the reqwest client is expected to be infallible.
    if let Some(proxy_url) = proxy_url {
        let proxy = Proxy::all(proxy_url)
            .expect("Infallible conversion from URL type to URL type")
            .custom_http_auth(
                client_id
                    .try_into()
                    .expect("Backend name is not valid ASCII?"),
            );
        builder = builder.proxy(proxy);
    }
    builder = builder
        .user_agent("Convex/1.0")
        .use_preconfigured_tls(TLS_CONNECTOR.clone());
    builder
}

/// Creates a reqwest client configured with an optional proxy.
/// The client_id is set to the instance name for logging.
/// The redirect_policy dictates how redirects are handled.
///
/// This function is shared between `ProxiedFetchClient` (for fetch syscalls)
/// and `CachedHttpClient` in the `http_client` crate (for OIDC
/// discovery).
pub fn build_proxied_reqwest_client(
    proxy_url: Option<Url>,
    client_id: String,
    redirect_policy: reqwest::redirect::Policy,
) -> reqwest::Client {
    build_proxied_reqwest_client_builder(proxy_url, client_id, redirect_policy)
        .build()
        .expect("Failed to build reqwest client")
}

/// Whether the backend has been explicitly configured to allow UDF `fetch`
/// requests to private IP ranges without a screening proxy.
///
/// The SSRF denylist is on by default when no proxy is configured. Self-hosted
/// deployments that need UDFs to reach internal services (e.g. a service
/// inside the deployment's own network) can opt out with this environment
/// variable, matching the `CONVEX_ALLOW_INSECURE_DEV_SECRET` convention.
fn allow_private_fetch_ips() -> bool {
    let allowed = std::env::var("CONVEX_ALLOW_PRIVATE_FETCH_IPS").is_ok();
    if allowed {
        tracing::warn!(
            "CONVEX_ALLOW_PRIVATE_FETCH_IPS is set -- UDF `fetch` requests to \
             private/loopback/link-local/metadata IP ranges are allowed (SSRF denylist disabled). \
             Only do this if you run a separate screening proxy or trust your UDF code."
        );
    }
    allowed
}

/// Whether an IP address is in a range that UDFs should never be able to
/// reach: private networks, loopback, link-local, cloud metadata, CGNAT,
/// multicast, and reserved ranges.
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

/// IPv4 SSRF denylist. Connecting to these ranges from a UDF is almost always
/// an SSRF attempt (e.g. scanning the deployment's VPC or hitting the cloud
/// instance metadata service at 169.254.169.254).
fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    // 0.0.0.0/8 "this network"
    a == 0
        // 10.0.0.0/8 RFC 1918
        || a == 10
        // 100.64.0.0/10 CGNAT (used by cloud NAT, Kubernetes services, etc.)
        || (a == 100 && (64..=127).contains(&b))
        // 127.0.0.0/8 loopback
        || a == 127
        // 169.254.0.0/16 link-local, including 169.254.169.254 (cloud metadata)
        || (a == 169 && b == 254)
        // 172.16.0.0/12 RFC 1918
        || (a == 172 && (16..=31).contains(&b))
        // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 0)
        // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 TEST-NET documentation
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        // 192.168.0.0/16 RFC 1918
        || (a == 192 && b == 168)
        // 198.18.0.0/15 benchmarking
        || (a == 198 && (b == 18 || b == 19))
        // 224.0.0.0/4 multicast, 240.0.0.0/4 reserved, 255.255.255.255/32
        // broadcast
        || a >= 224
}

/// IPv6 SSRF denylist.
fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    // ::/128 unspecified and ::1/128 loopback
    if ip.is_unspecified() || ip.is_loopback() {
        return true;
    }
    // fe80::/10 link-local
    if ip.segments()[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    // fc00::/7 unique local addresses (private)
    if ip.segments()[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    // 64:ff9b::/96 NAT64 well-known prefix
    if ip.segments()[0] == 0x0064
        && ip.segments()[1] == 0xff9b
        && ip.segments()[2] == 0
        && ip.segments()[3] == 0
        && ip.segments()[4] == 0
        && ip.segments()[5] == 0
    {
        return true;
    }
    // ::ffff:0:0/96 IPv4-mapped addresses -- check the embedded IPv4 address
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(v4);
    }
    false
}

/// Returns an error message if the host of a URL is an IP literal in a blocked
/// range. Domain names are resolved through [`SsrfSafeResolver`] instead, so
/// they are never checked here.
fn blocked_host_reason(host: &Host<&str>) -> Option<String> {
    let ip = match host {
        Host::Ipv4(v4) => IpAddr::V4(*v4),
        Host::Ipv6(v6) => IpAddr::V6(*v6),
        Host::Domain(_) => return None,
    };
    if is_blocked_ip(ip) {
        Some(format!(
            "host {ip} is a private, loopback, link-local, or metadata IP address"
        ))
    } else {
        None
    }
}

/// DNS resolver used by the UDF fetch client when no proxy is configured.
///
/// Resolves the hostname once, fails the resolution if *any* resolved address
/// is in a blocked (private/loopback/link-local/metadata) range, and returns
/// only the validated addresses to the connector. hyper's `HttpConnector`
/// connects exclusively to the addresses returned by this resolver and never
/// re-resolves the hostname, so a DNS-rebinding attacker cannot redirect the
/// connection to a private address after validation.
#[derive(Debug)]
struct SsrfSafeResolver;

impl reqwest::dns::Resolve for SsrfSafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = resolve_host(&host).await?;
            Ok::<reqwest::dns::Addrs, Box<dyn std::error::Error + Send + Sync>>(addrs)
        })
    }
}

/// Resolve `host` and validate every resolved address against the SSRF
/// denylist, returning only validated addresses (with port 0, which the
/// connector replaces with the URL's port).
async fn resolve_host(host: &str) -> Result<reqwest::dns::Addrs, io::Error> {
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| io::Error::other(format!("failed to resolve {host}: {e}")))?;
    let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
    if ips.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("{host} resolved to no IP addresses"),
        ));
    }
    if let Some(blocked) = ips.iter().copied().find(|ip| is_blocked_ip(*ip)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to connect to {host}: resolved to a private, loopback, link-local, or \
                 metadata IP address ({blocked})"
            ),
        ));
    }
    Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))))
}

pub struct ProxiedFetchClient {
    http_client:
        LazyLock<reqwest::Client, Box<dyn FnOnce() -> reqwest::Client + Send + Sync + 'static>>,
    /// Whether to enforce the SSRF denylist (private/loopback/link-local/
    /// metadata IP ranges) on this client's requests.
    ssrf_guard: bool,
}

impl ProxiedFetchClient {
    pub fn new(
        proxy_url: Option<Url>,
        client_id: String,
        redirect_policy: reqwest::redirect::Policy,
    ) -> Self {
        // SSRF screening is delegated to the proxy when one is configured: the
        // proxy resolves the destination and answers 407 for blocked targets
        // (see the PROXY_AUTHENTICATION_REQUIRED handling below). Our
        // client-side denylist only applies when we resolve DNS ourselves (no
        // proxy), and it is on by default so UDFs cannot reach
        // private/loopback/link-local/metadata ranges unless the deployment
        // explicitly opts out.
        let ssrf_guard = proxy_url.is_none() && !allow_private_fetch_ips();
        Self {
            http_client: LazyLock::new(Box::new(move || {
                let mut builder =
                    build_proxied_reqwest_client_builder(proxy_url, client_id, redirect_policy);
                if ssrf_guard {
                    builder = builder.dns_resolver(Arc::new(SsrfSafeResolver));
                }
                builder.build().expect("Failed to build reqwest client")
            })),
            ssrf_guard,
        }
    }
}

#[async_trait]
impl FetchClient for ProxiedFetchClient {
    async fn fetch(&self, mut request: HttpRequestStream) -> anyhow::Result<HttpResponseStream> {
        if self.ssrf_guard
            && let Some(host) = request.url.host()
            && let Some(reason) = blocked_host_reason(&host)
        {
            // IP literal hosts never reach the DNS resolver (hyper connects to
            // them directly), so they must be checked here. Domain hosts are
            // validated by SsrfSafeResolver at resolution time.
            anyhow::bail!("Request to {} forbidden: {reason}", request.url);
        }
        let mut request_builder = self
            .http_client
            .request(request.method, request.url.as_str());
        let request_size = Arc::new(AtomicU64::new(0));
        // Only attach a body when the request has one. `Body::wrap_stream` (used
        // by `streaming_body`) reports `is_end_stream() == false`, so hyper omits
        // END_STREAM from the HTTP/2 HEADERS frame and closes the stream with a
        // trailing empty DATA frame -- which strict servers reject for a body-less
        // GET (https://github.com/get-convex/convex-backend/issues/497). Omitting
        // the body uses `Body::empty()` (`is_end_stream() == true`), so hyper sets
        // END_STREAM on HEADERS and sends no DATA frame.
        if let Some(body) = request.body {
            request_builder = request_builder.body(streaming_body(body, request_size.clone()));
        }
        for (name, value) in &request.headers {
            request_builder = request_builder.header(name.as_str(), value.as_bytes());
        }
        let raw_request = request_builder.build()?;
        let raw_response = select! {
            response = self.http_client.execute(raw_request) => {
                response?
            },
            _ = &mut request.signal => {
                // TODO: This should turn into a DOMException with name "AbortError"
                anyhow::bail!(ErrorMetadata::bad_request("RequestAborted", "AbortError"));
            },
        };
        if raw_response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
            // SSRF mitigated -- our proxy blocked this request because it was
            // directed at a non-public IP range. Don't send back the raw HTTP response as
            // it leaks internal implementation details in the response headers.
            anyhow::bail!("Request to {} forbidden", request.url);
        }
        let status = raw_response.status();
        let headers = raw_response.headers().to_owned();
        let response = HttpResponseStream {
            status,
            headers,
            url: Some(request.url),
            body: Some(cancelable_body_stream(
                raw_response.bytes_stream(),
                request.signal,
            )),
            request_size,
        };
        Ok(response)
    }
}

/// Wraps a request body stream into a reqwest [`Body`], counting the bytes that
/// flow through it into `request_size`.
///
/// This is intentionally a lazy, non-awaiting wrap: the body stream may be a
/// full-duplex body whose first chunk is only produced after the response
/// headers arrive, so we must not poll it before the request is sent.
fn streaming_body(
    body: Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Sync + Send>>,
    request_size: Arc<AtomicU64>,
) -> Body {
    Body::wrap_stream(body.inspect(move |b| {
        if let Ok(b) = b {
            request_size.fetch_add(b.len() as u64, Ordering::Relaxed);
        }
    }))
}

#[try_stream(boxed, ok = Bytes, error = anyhow::Error)]
async fn cancelable_body_stream<E: Into<anyhow::Error>>(
    stream: impl futures::stream::Stream<Item = Result<Bytes, E>> + Send + 'static,
    mut signal: BoxFuture<'static, ()>,
) {
    let mut stream = Box::pin(stream);
    loop {
        let result = async {
            select! {
                item = stream.next() => {
                    item.transpose().map_err(Into::<anyhow::Error>::into)
                },
                _ = &mut signal => {
                    // TODO: This should turn into a DOMException with name "AbortError"
                    Err(anyhow::anyhow!(ErrorMetadata::bad_request("RequestAborted", "AbortError")))
                },
            }
        };
        match result.await? {
            Some(item) => {
                yield item;
            },
            None => {
                break;
            },
        }
    }
}

type HandlerFn = Box<
    dyn Fn(HttpRequestStream) -> BoxFuture<'static, anyhow::Result<HttpResponseStream>>
        + Send
        + Sync
        + 'static,
>;

pub struct StaticFetchClient {
    router: BTreeMap<url::Url, HashMap<http::Method, HandlerFn>>,
    num_calls: AtomicU64,
}

impl StaticFetchClient {
    pub fn new() -> Self {
        Self {
            router: BTreeMap::new(),
            num_calls: AtomicU64::new(0),
        }
    }

    pub fn register_http_route<F>(&mut self, url: url::Url, method: http::Method, handler: F)
    where
        F: Fn(HttpRequestStream) -> BoxFuture<'static, anyhow::Result<HttpResponseStream>>
            + Send
            + Sync
            + 'static,
    {
        self.router
            .entry(url)
            .or_default()
            .insert(method, Box::new(handler));
    }

    /// Returns how many times a fetch client has been called
    pub fn num_calls(&self) -> u64 {
        self.num_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl FetchClient for StaticFetchClient {
    async fn fetch(&self, request: HttpRequestStream) -> anyhow::Result<HttpResponseStream> {
        self.num_calls.fetch_add(1, Ordering::Relaxed);
        let handler = self
            .router
            .get(&request.url)
            .and_then(|methods| methods.get(&request.method))
            .unwrap_or_else(|| {
                panic!(
                    "could not find route {} with method {}",
                    request.url, request.method
                )
            });
        handler(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use url::Url;

    use super::{
        blocked_host_reason,
        is_blocked_ip,
    };

    fn blocked(ip: &str) -> bool {
        is_blocked_ip(ip.parse::<IpAddr>().unwrap())
    }

    #[test]
    fn blocks_private_ipv4_ranges() {
        for ip in [
            "0.0.0.0",
            "0.255.255.255",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.255",
            "169.254.0.1",
            "169.254.169.254",
            "169.254.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.0.1",
            "192.168.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(blocked(ip), "{ip} should be blocked");
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "9.9.9.9",
            "23.0.0.1",
            "52.0.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "169.253.255.255",
            "169.255.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.3.1",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
        ] {
            assert!(!blocked(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn blocks_private_ipv6_ranges() {
        for ip in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "64:ff9b::1",
            "64:ff9b::c000:201",
            "fc00::1",
            "fd00::1",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe80::1",
            "fe9f::1",
            "febf::1",
        ] {
            assert!(blocked(ip), "{ip} should be blocked");
        }
    }

    #[test]
    fn allows_public_ipv6() {
        for ip in [
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2a00:1450:4001:810::200e",
            "2400:cb00::1",
            "fbff::1",
            "f800::1",
        ] {
            assert!(!blocked(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn blocks_ip_literal_hosts_in_urls() {
        for url in [
            "http://10.0.0.1/",
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:169.254.169.254]/",
        ] {
            let parsed = Url::parse(url).unwrap();
            let host = parsed.host().unwrap();
            assert!(
                blocked_host_reason(&host).is_some(),
                "host {host} in {url} should be blocked"
            );
        }
    }

    #[test]
    fn allows_public_ip_literal_and_domain_hosts() {
        for url in [
            "https://8.8.8.8/",
            "http://[2606:4700:4700::1111]/",
            "https://example.com/",
            "http://localhost/",
        ] {
            let parsed = Url::parse(url).unwrap();
            let host = parsed.host().unwrap();
            assert!(
                blocked_host_reason(&host).is_none(),
                "host {host} in {url} should not be blocked by the URL check (domains are \
                 validated at DNS resolution time)"
            );
        }
    }

    #[test]
    fn boundary_ranges_are_correct() {
        // Just inside each private range: blocked. Just outside: allowed.
        assert!(blocked("172.16.0.0"));
        assert!(!blocked("172.15.255.255"));
        assert!(blocked("172.31.255.255"));
        assert!(!blocked("172.32.0.0"));
        assert!(blocked("100.64.0.0"));
        assert!(!blocked("100.63.255.255"));
        assert!(blocked("100.127.255.255"));
        assert!(!blocked("100.128.0.0"));
        assert!(blocked("169.254.0.0"));
        assert!(!blocked("169.253.255.255"));
        assert!(blocked("192.168.0.0"));
        assert!(!blocked("192.169.0.0"));
        assert!(blocked("198.18.0.0"));
        assert!(!blocked("198.17.255.255"));
        assert!(blocked("198.19.255.255"));
        assert!(!blocked("198.20.0.0"));
        assert!(blocked("224.0.0.0"));
        assert!(!blocked("223.255.255.255"));
    }
}
