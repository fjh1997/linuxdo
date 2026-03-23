use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::header::{HOST, LOCATION};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1 as client_http1;
use hyper::client::conn::http2 as client_http2;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use reqwest::Client;
use rustls::SignatureScheme;
use rustls::client::{EchConfig, EchMode};
use rustls::crypto::{aws_lc_rs, ring};
use rustls::pki_types::{EchConfigListBytes, ServerName};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::client::danger::ServerCertVerifier;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError};

use crate::certs::CertificateBundle;
use crate::config::AppConfig;

struct AppState {
    config: AppConfig,
    doh_client: Client,
    resolve_cache: RwLock<HashMap<ResolveCacheKey, CachedResolvedUpstream>>,
}

#[derive(Clone, Debug)]
struct ResolvedUpstream {
    addrs: Vec<SocketAddr>,
    ech_config: Option<EchConfig>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ResolveCacheKey {
    host: String,
    port: u16,
}

#[derive(Clone)]
struct CachedResolvedUpstream {
    upstream: ResolvedUpstream,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct HttpsServiceBinding {
    priority: u16,
    target_name: Option<String>,
    ech_config_list: Option<Vec<u8>>,
    ech_public_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DohJsonResponse {
    #[serde(rename = "Status")]
    status: Option<u32>,
    #[serde(rename = "Answer")]
    answers: Option<Vec<DohAnswer>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    record_type: u16,
    data: String,
}

const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(300);

pub async fn run_proxy(config: AppConfig, bundle: CertificateBundle) -> Result<()> {
    let doh_client = Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build DoH client")?;
    let state = Arc::new(AppState {
        config,
        doh_client,
        resolve_cache: RwLock::new(HashMap::new()),
    });
    let http_state = state.clone();
    let https_state = state.clone();

    tokio::try_join!(
        run_http_redirect(http_state),
        run_https_proxy(https_state, bundle)
    )?;

    Ok(())
}

async fn run_http_redirect(state: Arc<AppState>) -> Result<()> {
    let address = format!("{}:{}", state.config.listen_host, state.config.http_port);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {address}"))?;

    println!("http redirect listening on http://{address}");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept HTTP socket")?;
        let state = state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |request| redirect_handler(request, state.clone()));
            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("http redirect connection error: {error}");
            }
        });
    }
}

async fn run_https_proxy(state: Arc<AppState>, bundle: CertificateBundle) -> Result<()> {
    let certs = load_certificate_chain(&bundle)?;
    let key = load_private_key(&bundle.server_key_path)?;
    let provider = ring::default_provider();

    let mut tls_config = ServerConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .context("failed to configure rustls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build rustls server config")?;
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let address = format!("{}:{}", state.config.listen_host, state.config.https_port);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind HTTPS listener on {address}"))?;

    println!("https proxy listening on https://{address}");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept HTTPS socket")?;
        let acceptor = acceptor.clone();
        let state = state.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("tls accept error: {error}");
                    return;
                }
            };

            let service = service_fn(move |request| proxy_handler(request, state.clone()));
            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .await
            {
                eprintln!("https proxy connection error: {error}");
            }
        });
    }
}

async fn redirect_handler(
    request: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let host = extract_host(request.headers(), &state.config)
        .unwrap_or_else(|| state.config.server_common_name.clone());
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");

    let port = if state.config.https_port == 443 {
        String::new()
    } else {
        format!(":{}", state.config.https_port)
    };
    let location = format!("https://{host}{port}{path}");

    Ok(Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(LOCATION, location)
        .body(Full::new(Bytes::new()))
        .unwrap())
}

async fn proxy_handler(
    request: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let host = match extract_host(request.headers(), &state.config) {
        Some(host) => host,
        None => {
            return Ok(simple_response(
                StatusCode::BAD_REQUEST,
                "missing Host header",
            ));
        }
    };

    if !state.config.matches_proxy_host(&host) {
        return Ok(simple_response(
            StatusCode::BAD_GATEWAY,
            "host is not managed by linuxdo-accelerator",
        ));
    }

    match forward_request(request, &state, &host).await {
        Ok(response) => Ok(response),
        Err(error) => Ok(simple_response(
            StatusCode::BAD_GATEWAY,
            &format!("upstream request failed: {error:#}"),
        )),
    }
}

async fn forward_request(
    request: Request<Incoming>,
    state: &AppState,
    request_host: &str,
) -> Result<Response<Full<Bytes>>> {
    let upstream_url = reqwest::Url::parse(&state.config.upstream)
        .with_context(|| format!("failed to parse upstream URL {}", state.config.upstream))?;
    let upstream_scheme = upstream_url.scheme().to_string();
    let upstream_port = upstream_url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("upstream URL is missing port"))?;
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let method: Method = request.method().clone();
    let headers = request.headers().clone();
    let body = request
        .into_body()
        .collect()
        .await
        .context("failed to read inbound request body")?
        .to_bytes();

    let upstream_response = dispatch_upstream_request(
        state,
        &upstream_scheme,
        request_host,
        upstream_port,
        method,
        headers,
        body,
        &path_and_query,
    )
    .await?;

    let status = upstream_response.status();
    let headers = upstream_response.headers().clone();
    let body = upstream_response
        .into_body()
        .collect()
        .await
        .context("failed to read upstream response body")?
        .to_bytes();

    let mut response_builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if should_skip_response_header(name.as_str()) {
            continue;
        }
        response_builder = response_builder.header(name, value);
    }

    Ok(response_builder
        .body(Full::new(body))
        .unwrap_or_else(|_| simple_response(StatusCode::BAD_GATEWAY, "failed to build response")))
}

async fn dispatch_upstream_request(
    state: &AppState,
    upstream_scheme: &str,
    request_host: &str,
    upstream_port: u16,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    path_and_query: &str,
) -> Result<Response<Incoming>> {
    let upstream = resolve_upstream(state, request_host, upstream_port).await?;
    let mut last_error = None;

    if upstream.ech_config.is_some() {
        for addr in upstream.addrs.iter().copied() {
            match send_once(
                upstream_scheme,
                request_host,
                None,
                upstream.ech_config.clone(),
                addr,
                method.clone(),
                headers.clone(),
                body.clone(),
                path_and_query,
            )
            .await
            {
                Ok(response) => return Ok(response),
                Err(error) => {
                    eprintln!(
                        "ech upstream attempt failed for {request_host} via {addr}: {error:#}"
                    );
                    last_error = Some(error);
                }
            }
        }
    }

    let sni_candidates = effective_fake_sni_candidates(&state.config);
    for sni_name in sni_candidates {
        for addr in upstream.addrs.iter().copied() {
            match send_once(
                upstream_scheme,
                request_host,
                Some(&sni_name),
                None,
                addr,
                method.clone(),
                headers.clone(),
                body.clone(),
                path_and_query,
            )
            .await
            {
                Ok(response) => {
                    if should_retry_front_status(response.status()) {
                        last_error = Some(anyhow::anyhow!(
                            "front domain {sni_name} returned retryable status {}",
                            response.status()
                        ));
                        continue;
                    }
                    return Ok(response);
                }
                Err(error) => last_error = Some(error),
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no usable upstream address")))
}

async fn send_once(
    upstream_scheme: &str,
    request_host: &str,
    outer_sni: Option<&str>,
    ech_config: Option<EchConfig>,
    addr: SocketAddr,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    path_and_query: &str,
) -> Result<Response<Incoming>> {
    let request = build_upstream_request(request_host, method, headers, body, path_and_query)?;

    if upstream_scheme.eq_ignore_ascii_case("http") {
        let stream = connect_tcp(addr).await?;
        return send_over_io(TokioIo::new(stream), request).await;
    }

    let tls_stream = connect_tls(request_host, outer_sni, ech_config, addr).await?;
    let negotiated_h2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");
    if negotiated_h2 {
        return send_over_io_http2(TokioIo::new(tls_stream), request).await;
    }
    send_over_io(TokioIo::new(tls_stream), request).await
}

fn build_upstream_request(
    request_host: &str,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    path_and_query: &str,
) -> Result<Request<Full<Bytes>>> {
    let mut builder = Request::builder().method(method).uri(path_and_query);
    for (name, value) in headers.iter() {
        if should_skip_request_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder
        .header(HOST, request_host);
    builder
        .body(Full::new(body))
        .context("failed to build upstream request")
}

async fn send_over_io<T>(
    io: TokioIo<T>,
    request: Request<Full<Bytes>>,
) -> Result<Response<Incoming>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = client_http1::handshake(io)
        .await
        .context("failed to initialize upstream HTTP/1.1 client")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .context("failed to contact upstream")
}

async fn send_over_io_http2<T>(
    io: TokioIo<T>,
    request: Request<Full<Bytes>>,
) -> Result<Response<Incoming>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = client_http2::Builder::new(TokioExecutor::new())
        .handshake(io)
        .await
        .context("failed to initialize upstream HTTP/2 client")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .context("failed to contact upstream")
}

async fn connect_tls(
    request_host: &str,
    outer_sni: Option<&str>,
    ech_config: Option<EchConfig>,
    addr: SocketAddr,
) -> Result<TlsStream<tokio::net::TcpStream>> {
    let tcp = connect_tcp(addr).await?;

    let provider = aws_lc_rs::default_provider();
    let mut tls_config = if let Some(ech_config) = ech_config {
        ClientConfig::builder_with_provider(provider.into())
            .with_ech(EchMode::Enable(ech_config))
            .context("failed to configure upstream ECH mode")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth()
    } else {
        ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .context("failed to configure upstream TLS versions")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth()
    };
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let server_name = ServerName::try_from(outer_sni.unwrap_or(request_host).to_string())
        .context("failed to construct upstream SNI name")?;
    let connector = TlsConnector::from(Arc::new(tls_config));
    connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("failed TLS handshake with upstream {addr}"))
}

async fn connect_tcp(addr: SocketAddr) -> Result<tokio::net::TcpStream> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .with_context(|| format!("timed out connecting upstream {addr}"))?
    .with_context(|| format!("failed to connect upstream {addr}"))
}

async fn resolve_upstream(state: &AppState, host: &str, port: u16) -> Result<ResolvedUpstream> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ResolvedUpstream {
            addrs: vec![SocketAddr::new(ip, port)],
            ech_config: None,
        });
    }

    let override_host = state.config.find_dns_host_override(host).map(str::to_owned);
    let cache_key = ResolveCacheKey {
        host: host.to_ascii_lowercase(),
        port,
    };
    if let Some(cached) = read_cached_upstream(state, &cache_key).await {
        return Ok(cached);
    }

    let override_target = override_host
        .as_deref()
        .map(parse_dns_host_override)
        .transpose()?;

    if let Some(DnsHostOverride::Addresses(ips)) = override_target.as_ref() {
        let upstream = ResolvedUpstream {
            addrs: ips
                .iter()
                .copied()
                .map(|ip| SocketAddr::new(ip, port))
                .collect::<Vec<_>>(),
            ech_config: None,
        };
        write_cached_upstream(state, cache_key, upstream.clone()).await;
        return Ok(upstream);
    }

    let binding_host = override_target
        .as_ref()
        .and_then(|target| match target {
            DnsHostOverride::Alias(host) => Some(host.as_str()),
            DnsHostOverride::Addresses(_) => None,
        })
        .unwrap_or(host);

    let binding = resolve_https_binding(state, binding_host).await?;
    let target_host = binding
        .target_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| binding_host.to_string());

    let public_name = binding
        .ech_public_name
        .as_ref()
        .filter(|name| *name != &target_host)
        .cloned();
    let public_lookup = async {
        if let Some(public_name) = public_name.as_deref() {
            return doh_lookup_ip_addrs(state, public_name).await;
        }
        Ok(Vec::new())
    };
    let (mut ips, extra_ips) =
        tokio::try_join!(doh_lookup_ip_addrs(state, &target_host), public_lookup)?;
    ips.extend(extra_ips);

    if ips.is_empty() {
        anyhow::bail!("upstream host {target_host} resolved to no addresses");
    }

    let mut ordered_ips = Vec::with_capacity(ips.len());
    for ip in ips.drain(..) {
        if !ordered_ips.contains(&ip) {
            ordered_ips.push(ip);
        }
    }
    let ips = ordered_ips;

    let ech_config = if let Some(ech_bytes) = binding.ech_config_list {
        match EchConfig::new(
            EchConfigListBytes::from(ech_bytes),
            aws_lc_rs::hpke::ALL_SUPPORTED_SUITES,
        ) {
            Ok(config) => Some(config),
            Err(error) => {
                eprintln!("failed to build ECH config for {host}: {error}");
                None
            }
        }
    } else {
        None
    };

    let addrs = ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect::<Vec<_>>();
    let upstream = ResolvedUpstream { addrs, ech_config };
    write_cached_upstream(state, cache_key, upstream.clone()).await;
    Ok(upstream)
}

async fn resolve_https_binding(state: &AppState, host: &str) -> Result<HttpsServiceBinding> {
    let mut bindings = doh_lookup_https_bindings(state, host).await?;
    bindings.sort_by_key(|binding| binding.priority);
    Ok(bindings.into_iter().next().unwrap_or_default())
}

async fn doh_lookup_ip_addrs(state: &AppState, host: &str) -> Result<Vec<IpAddr>> {
    let mut addrs = Vec::new();

    let (ipv6_answers, ipv4_answers) =
        tokio::try_join!(doh_query(state, host, "AAAA"), doh_query(state, host, "A"))?;

    for answer in ipv6_answers {
        if answer.record_type == 28
            && let Ok(ip) = answer.data.parse::<std::net::Ipv6Addr>()
        {
            addrs.push(IpAddr::V6(ip));
        }
    }

    for answer in ipv4_answers {
        if answer.record_type == 1
            && let Ok(ip) = answer.data.parse::<std::net::Ipv4Addr>()
        {
            addrs.push(IpAddr::V4(ip));
        }
    }

    Ok(addrs)
}

async fn doh_lookup_https_bindings(
    state: &AppState,
    host: &str,
) -> Result<Vec<HttpsServiceBinding>> {
    let mut bindings = Vec::new();

    for answer in doh_query(state, host, "HTTPS").await? {
        if answer.record_type != 65 {
            continue;
        }

        match parse_https_answer(&answer.data) {
            Ok(binding) => bindings.push(binding),
            Err(error) => eprintln!("failed to parse HTTPS RR for {host}: {error:#}"),
        }
    }

    Ok(bindings)
}

async fn doh_query(state: &AppState, host: &str, record_type: &str) -> Result<Vec<DohAnswer>> {
    let mut last_error = None;
    for endpoint in &state.config.doh_endpoints {
        match doh_query_once(&state.doh_client, endpoint, host, record_type).await {
            Ok(answers) => return Ok(answers),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no DoH endpoint available")))
}

async fn read_cached_upstream(state: &AppState, key: &ResolveCacheKey) -> Option<ResolvedUpstream> {
    let cache = state.resolve_cache.read().await;
    cache.get(key).and_then(|entry| {
        if Instant::now() < entry.expires_at {
            Some(entry.upstream.clone())
        } else {
            None
        }
    })
}

async fn write_cached_upstream(state: &AppState, key: ResolveCacheKey, upstream: ResolvedUpstream) {
    let mut cache = state.resolve_cache.write().await;
    cache.retain(|_, entry| Instant::now() < entry.expires_at);
    cache.insert(
        key,
        CachedResolvedUpstream {
            upstream,
            expires_at: Instant::now() + RESOLVE_CACHE_TTL,
        },
    );
}

async fn doh_query_once(
    client: &Client,
    endpoint: &str,
    host: &str,
    record_type: &str,
) -> Result<Vec<DohAnswer>> {
    let response = client
        .get(endpoint)
        .query(&[("name", host), ("type", record_type)])
        .header("accept", "application/dns-json")
        .send()
        .await
        .with_context(|| format!("failed DoH request to {endpoint}"))?
        .error_for_status()
        .with_context(|| format!("DoH server {endpoint} returned failure status"))?;

    let payload = response
        .text()
        .await
        .with_context(|| format!("failed to read DoH JSON payload from {endpoint}"))?;
    let payload: DohJsonResponse = serde_json::from_str(&payload)
        .with_context(|| format!("failed to decode DoH JSON from {endpoint}"))?;

    if payload.status.unwrap_or(0) != 0 {
        anyhow::bail!(
            "DoH query {record_type} {host} via {endpoint} returned status {}",
            payload.status.unwrap_or(0)
        );
    }

    Ok(payload.answers.unwrap_or_default())
}

enum DnsHostOverride {
    Alias(String),
    Addresses(Vec<IpAddr>),
}

fn parse_dns_host_override(raw: &str) -> Result<DnsHostOverride> {
    let value = raw.trim();
    if value.is_empty() {
        anyhow::bail!("dns_hosts override cannot be empty");
    }

    if let Some(alias) = value.strip_prefix("domain:") {
        let alias = alias.trim();
        if alias.is_empty() {
            anyhow::bail!("dns_hosts domain override cannot be empty");
        }
        return Ok(DnsHostOverride::Alias(alias.to_string()));
    }

    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(DnsHostOverride::Addresses(vec![ip]));
    }

    Ok(DnsHostOverride::Alias(value.to_string()))
}

fn parse_https_answer(raw_rdata: &str) -> Result<HttpsServiceBinding> {
    let bytes = parse_dns_json_hex_rdata(raw_rdata)?;
    if bytes.len() < 3 {
        anyhow::bail!("HTTPS RR data is too short");
    }

    let priority = u16::from_be_bytes([bytes[0], bytes[1]]);
    let (target_name, consumed) = parse_dns_name(&bytes[2..])?;
    let mut binding = HttpsServiceBinding {
        priority,
        ..Default::default()
    };
    if !target_name.is_empty() {
        binding.target_name = Some(target_name);
    }

    let mut offset = 2 + consumed;
    while offset < bytes.len() {
        if offset + 4 > bytes.len() {
            anyhow::bail!("truncated HTTPS RR service parameter header");
        }

        let key = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let value_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset += 4;

        if offset + value_len > bytes.len() {
            anyhow::bail!("truncated HTTPS RR service parameter value");
        }

        let value = &bytes[offset..offset + value_len];
        offset += value_len;

        if key == 5 {
            let ech_config_list = parse_ech_config_param(value)?;
            binding.ech_public_name = parse_ech_public_name(&ech_config_list).ok();
            binding.ech_config_list = Some(ech_config_list);
        }
    }

    Ok(binding)
}

fn parse_dns_json_hex_rdata(raw_rdata: &str) -> Result<Vec<u8>> {
    let raw_rdata = raw_rdata.trim();
    let encoded = raw_rdata
        .strip_prefix("\\#")
        .with_context(|| format!("unexpected HTTPS RR format: {raw_rdata}"))?
        .trim();
    let mut parts = encoded.split_whitespace();
    let expected_len: usize = parts
        .next()
        .context("missing HTTPS RR byte length")?
        .parse()
        .context("invalid HTTPS RR byte length")?;

    let mut bytes = Vec::with_capacity(expected_len);
    for part in parts {
        let byte = u8::from_str_radix(part, 16)
            .with_context(|| format!("invalid HTTPS RR hex byte {part}"))?;
        bytes.push(byte);
    }

    if bytes.len() != expected_len {
        anyhow::bail!(
            "HTTPS RR length mismatch, expected {expected_len} bytes, got {}",
            bytes.len()
        );
    }

    Ok(bytes)
}

fn parse_dns_name(data: &[u8]) -> Result<(String, usize)> {
    let mut labels = Vec::new();
    let mut offset = 0usize;

    loop {
        let Some(&label_len) = data.get(offset) else {
            anyhow::bail!("truncated DNS name in HTTPS RR");
        };
        offset += 1;

        if label_len == 0 {
            break;
        }

        if label_len & 0b1100_0000 != 0 {
            anyhow::bail!("compressed DNS names are unsupported in HTTPS RR parser");
        }

        let label_len = label_len as usize;
        if offset + label_len > data.len() {
            anyhow::bail!("truncated DNS label in HTTPS RR");
        }

        let label = std::str::from_utf8(&data[offset..offset + label_len])
            .context("invalid UTF-8 label in HTTPS RR")?;
        labels.push(label.to_string());
        offset += label_len;
    }

    Ok((labels.join("."), offset))
}

fn parse_ech_config_param(value: &[u8]) -> Result<Vec<u8>> {
    if value.len() < 2 {
        anyhow::bail!("ECH service parameter is too short");
    }

    let declared_len = u16::from_be_bytes([value[0], value[1]]) as usize;
    if value.len() != declared_len + 2 {
        anyhow::bail!(
            "ECH config length mismatch, declared {declared_len}, actual {}",
            value.len().saturating_sub(2)
        );
    }

    Ok(value.to_vec())
}

fn parse_ech_public_name(ech_config_list: &[u8]) -> Result<String> {
    let list_len = read_u16(ech_config_list, 0)? as usize;
    if ech_config_list.len() < 2 + list_len || list_len < 4 {
        anyhow::bail!("ECH config list is truncated");
    }

    let config = &ech_config_list[2..2 + list_len];
    let version = read_u16(config, 0)?;
    if version != 0xfe0d {
        anyhow::bail!("unsupported ECH version {version:#06x}");
    }

    let contents_len = read_u16(config, 2)? as usize;
    if config.len() < 4 + contents_len {
        anyhow::bail!("ECH config contents are truncated");
    }

    let mut offset = 4usize;
    offset += 1; // config_id
    offset += 2; // kem_id

    let public_key_len = read_u16(config, offset)? as usize;
    offset += 2 + public_key_len;

    let suites_len = read_u16(config, offset)? as usize;
    offset += 2 + suites_len;

    offset += 1; // maximum_name_length

    let public_name_len = *config
        .get(offset)
        .context("ECH public_name length is missing")? as usize;
    offset += 1;

    let public_name_bytes = config
        .get(offset..offset + public_name_len)
        .context("ECH public_name bytes are truncated")?;
    let public_name = std::str::from_utf8(public_name_bytes)
        .context("ECH public_name is not valid UTF-8")?
        .trim()
        .to_string();

    if public_name.is_empty() {
        anyhow::bail!("ECH public_name is empty");
    }

    Ok(public_name)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .context("truncated u16 field while parsing ECH config")?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn effective_fake_sni_candidates(config: &AppConfig) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(fake_sni) = config.fake_sni.as_ref()
        && !fake_sni.trim().is_empty()
    {
        candidates.push(fake_sni.trim().to_string());
    }

    for candidate in [
        "www.cloudflare.com".to_string(),
        "cdnjs.cloudflare.com".to_string(),
        "developers.cloudflare.com".to_string(),
        "dash.cloudflare.com".to_string(),
        "one.one.one.one".to_string(),
    ] {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    candidates
}

fn should_retry_front_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::FORBIDDEN
            | StatusCode::MISDIRECTED_REQUEST
            | StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    )
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn extract_host(headers: &HeaderMap, config: &AppConfig) -> Option<String> {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(':')
                .next()
                .unwrap_or(value)
                .to_ascii_lowercase()
        })
        .or_else(|| config.proxy_domains.first().cloned())
}

fn should_skip_request_header(header_name: &str) -> bool {
    matches!(
        header_name.to_ascii_lowercase().as_str(),
        "connection"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn should_skip_response_header(header_name: &str) -> bool {
    matches!(
        header_name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn simple_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(message.to_string())))
        .unwrap()
}

fn load_certificates(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to load certificate chain")
}

fn load_certificate_chain(bundle: &CertificateBundle) -> Result<Vec<CertificateDer<'static>>> {
    load_certificates(&bundle.server_cert_path)
}

fn load_private_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .context("failed to parse private key")?
        .ok_or_else(|| anyhow::anyhow!("private key not found in {}", path.display()))
}
