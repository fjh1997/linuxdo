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
use hyper_util::server::conn::auto;
use reqwest::Client;
use rustls::SignatureScheme;
use rustls::client::{EchConfig, EchMode};
use rustls::crypto::{aws_lc_rs, ring};
use rustls::pki_types::{EchConfigListBytes, ServerName};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, watch};
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
use crate::paths::AppPaths;
use crate::runtime_log;

struct AppState {
    config: AppConfig,
    paths: AppPaths,
    doh_client: Client,
    upstream_tls_connector: TlsConnector,
    resolve_cache: RwLock<HashMap<ResolveCacheKey, CachedResolvedUpstream>>,
    doh_cache: RwLock<HashMap<DohCacheKey, CachedDohAnswers>>,
    preferred_upstream_addr: RwLock<HashMap<ResolveCacheKey, SocketAddr>>,
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

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct DohCacheKey {
    host: String,
    record_type: String,
}

#[derive(Clone, Debug)]
struct CachedDohAnswers {
    answers: Vec<DohAnswer>,
    expires_at: Instant,
}

struct UpstreamResponse {
    response: Response<Incoming>,
    negotiated_protocol: &'static str,
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
    #[serde(rename = "TTL")]
    ttl: Option<u32>,
    data: String,
}
const FALLBACK_RESOLVE_CACHE_TTL: Duration = Duration::from_secs(300);
const DOH_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const DOH_REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

pub async fn run_proxy(
    config: AppConfig,
    paths: AppPaths,
    bundle: CertificateBundle,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let doh_client = Client::builder()
        .connect_timeout(DOH_CONNECT_TIMEOUT)
        .timeout(DOH_REQUEST_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .context("failed to build DoH client")?;
    let provider = aws_lc_rs::default_provider();
    let mut upstream_tls_config = ClientConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .context("failed to configure upstream TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
        .with_no_client_auth();
    upstream_tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let state = Arc::new(AppState {
        config,
        paths,
        doh_client,
        upstream_tls_connector: TlsConnector::from(Arc::new(upstream_tls_config)),
        resolve_cache: RwLock::new(HashMap::new()),
        doh_cache: RwLock::new(HashMap::new()),
        preferred_upstream_addr: RwLock::new(HashMap::new()),
    });
    let http_state = state.clone();
    let https_state = state.clone();

    tokio::try_join!(
        run_http_redirect(http_state, shutdown_rx.clone()),
        run_https_proxy(https_state, bundle, shutdown_rx)
    )?;

    Ok(())
}

async fn run_http_redirect(
    state: Arc<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let address = format!("{}:{}", state.config.listen_host, state.config.http_port);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {address}"))?;

    println!("http redirect listening on http://{address}");

    loop {
        let (stream, _) = tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            result = listener.accept() => {
                result.context("failed to accept HTTP socket")?
            }
        };
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

    Ok(())
}

async fn run_https_proxy(
    state: Arc<AppState>,
    bundle: CertificateBundle,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let certs = load_certificate_chain(&bundle)?;
    let key = load_private_key(&bundle.server_key_path)?;
    let provider = ring::default_provider();

    let mut tls_config = ServerConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .context("failed to configure rustls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build rustls server config")?;
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let address = format!("{}:{}", state.config.listen_host, state.config.https_port);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind HTTPS listener on {address}"))?;

    println!("https proxy listening on https://{address}");

    loop {
        let (stream, _) = tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            result = listener.accept() => {
                result.context("failed to accept HTTPS socket")?
            }
        };
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
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder.http1().keep_alive(true);
            builder.http2().adaptive_window(true);
            builder.http2().keep_alive_interval(None);
            if let Err(error) = builder
                .serve_connection(TokioIo::new(tls_stream), service)
                .await
            {
                eprintln!("https proxy connection error: {error}");
            }
        });
    }

    Ok(())
}

async fn redirect_handler(
    request: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let host = extract_host(request.headers(), request.uri(), &state.config)
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
    let host = match extract_host(request.headers(), request.uri(), &state.config) {
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

    let status = upstream_response.response.status();
    let headers = upstream_response.response.headers().clone();
    let body = upstream_response
        .response
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
) -> Result<UpstreamResponse> {
    let upstream = resolve_upstream(state, request_host, upstream_port).await?;
    let ech_config = upstream
        .ech_config
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ECH 强制模式：{request_host} 未提供可用的 ECH 配置"))?;

    let mut last_error = None;
    for addr in upstream.addrs.iter().copied() {
        log_upstream_debug(
            state,
            request_host,
            path_and_query,
            &format!(
                "attempt addr={addr} scheme={upstream_scheme} ech={} edge_node={}",
                if upstream.ech_config.is_some() {
                    "yes"
                } else {
                    "no"
                },
                state
                    .config
                    .edge_node_override()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("-")
            ),
        );
        match send_once(
            state,
            upstream_scheme,
            request_host,
            None,
            Some(ech_config.clone()),
            addr,
            method.clone(),
            headers.clone(),
            body.clone(),
            path_and_query,
        )
        .await
        {
            Ok(response) => {
                remember_successful_upstream(state, request_host, upstream_port, addr).await;
                let status = response.response.status();
                let cf_ray = response
                    .response
                    .headers()
                    .get("cf-ray")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("-");
                let content_type = response
                    .response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("-");
                log_upstream_debug(
                    state,
                    request_host,
                    path_and_query,
                    &format!(
                        "success addr={addr} protocol={} status={} cf-ray={} content-type={}",
                        response.negotiated_protocol, status, cf_ray, content_type
                    ),
                );
                return Ok(response);
            }
            Err(error) => {
                log_upstream_debug(
                    state,
                    request_host,
                    path_and_query,
                    &format!("failure addr={addr} error={error:#}"),
                );
                eprintln!("ech upstream attempt failed for {request_host} via {addr}: {error:#}");
                last_error = Some(error);
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("ECH 强制模式：{request_host} 所有上游地址都握手失败")))
}

async fn send_once(
    state: &AppState,
    upstream_scheme: &str,
    request_host: &str,
    outer_sni: Option<&str>,
    ech_config: Option<EchConfig>,
    addr: SocketAddr,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
    path_and_query: &str,
) -> Result<UpstreamResponse> {
    let request = build_upstream_request(request_host, method, headers, body, path_and_query)?;

    if upstream_scheme.eq_ignore_ascii_case("http") {
        let stream = connect_tcp(addr).await?;
        return Ok(UpstreamResponse {
            response: send_over_io(TokioIo::new(stream), request).await?,
            negotiated_protocol: "http/1.1",
        });
    }

    let tls_stream = connect_tls(state, request_host, outer_sni, ech_config, addr).await?;
    let negotiated_h2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");
    if negotiated_h2 {
        return Ok(UpstreamResponse {
            response: send_over_io_http2(TokioIo::new(tls_stream), request).await?,
            negotiated_protocol: "h2",
        });
    }
    Ok(UpstreamResponse {
        response: send_over_io(TokioIo::new(tls_stream), request).await?,
        negotiated_protocol: "http/1.1",
    })
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
    builder = builder.header(HOST, request_host);
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
    let mut builder = client_http2::Builder::new(TokioExecutor::new());
    builder.adaptive_window(true);
    let (mut sender, connection) = builder
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
    state: &AppState,
    request_host: &str,
    outer_sni: Option<&str>,
    ech_config: Option<EchConfig>,
    addr: SocketAddr,
) -> Result<TlsStream<tokio::net::TcpStream>> {
    let tcp = connect_tcp(addr).await?;

    let connector = if let Some(ech_config) = ech_config {
        let provider = aws_lc_rs::default_provider();
        let mut tls_config = ClientConfig::builder_with_provider(provider.into())
            .with_ech(EchMode::Enable(ech_config))
            .context("failed to configure upstream ECH mode")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth();
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        TlsConnector::from(Arc::new(tls_config))
    } else {
        state.upstream_tls_connector.clone()
    };

    let server_name = ServerName::try_from(outer_sni.unwrap_or(request_host).to_string())
        .context("failed to construct upstream SNI name")?;
    connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("failed TLS handshake with upstream {addr}"))
}

async fn connect_tcp(addr: SocketAddr) -> Result<tokio::net::TcpStream> {
    let stream = tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .with_context(|| format!("timed out connecting upstream {addr}"))?
    .with_context(|| format!("failed to connect upstream {addr}"))?;
    stream
        .set_nodelay(true)
        .with_context(|| format!("failed to enable TCP_NODELAY for upstream {addr}"))?;
    Ok(stream)
}

async fn resolve_upstream(state: &AppState, host: &str, port: u16) -> Result<ResolvedUpstream> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ResolvedUpstream {
            addrs: vec![SocketAddr::new(ip, port)],
            ech_config: None,
        });
    }

    let override_host = state.config.find_dns_host_override(host).map(str::to_owned);
    let edge_override = state
        .config
        .edge_node_override()
        .map(parse_dns_host_override)
        .transpose()?;
    let cache_key = ResolveCacheKey {
        host: host.to_ascii_lowercase(),
        port,
    };
    if let Some(cached) = read_cached_upstream(state, &cache_key).await {
        let mut cached = cached;
        prioritize_preferred_upstream(state, &cache_key, &mut cached.addrs).await;
        log_upstream_debug(
            state,
            host,
            "/",
            &format!(
                "resolve cache-hit addrs={} ech={}",
                format_socket_addrs(&cached.addrs),
                if cached.ech_config.is_some() {
                    "yes"
                } else {
                    "no"
                }
            ),
        );
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
        write_cached_upstream(
            state,
            cache_key,
            upstream.clone(),
            FALLBACK_RESOLVE_CACHE_TTL,
        )
        .await;
        return Ok(upstream);
    }

    let binding_host = override_target
        .as_ref()
        .and_then(|target| match target {
            DnsHostOverride::Alias(host) => Some(host.as_str()),
            DnsHostOverride::Addresses(_) => None,
        })
        .unwrap_or(host);

    let (binding, binding_ttl) = resolve_https_binding(state, binding_host).await?;
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
        Ok((Vec::new(), None))
    };
    let ((mut ips, addr_ttl), (extra_ips, extra_ttl)) = if let Some(edge_override) = edge_override {
        let override_lookup = async {
            match edge_override {
                DnsHostOverride::Addresses(ips) => Ok((ips, None)),
                DnsHostOverride::Alias(alias) => doh_lookup_ip_addrs(state, &alias).await,
            }
        };
        tokio::try_join!(override_lookup, public_lookup)?
    } else {
        tokio::try_join!(doh_lookup_ip_addrs(state, &target_host), public_lookup)?
    };
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
    let mut upstream = ResolvedUpstream { addrs, ech_config };
    prioritize_preferred_upstream(state, &cache_key, &mut upstream.addrs).await;
    log_upstream_debug(
        state,
        host,
        "/",
        &format!(
            "resolve binding_host={binding_host} target_host={target_host} addrs={} ech={} edge_node={}",
            format_socket_addrs(&upstream.addrs),
            if upstream.ech_config.is_some() {
                "yes"
            } else {
                "no"
            },
            state
                .config
                .edge_node_override()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("-")
        ),
    );
    let resolve_ttl = min_duration_options(binding_ttl, min_duration_options(addr_ttl, extra_ttl))
        .unwrap_or(FALLBACK_RESOLVE_CACHE_TTL);
    write_cached_upstream(state, cache_key, upstream.clone(), resolve_ttl).await;
    Ok(upstream)
}

async fn resolve_https_binding(
    state: &AppState,
    host: &str,
) -> Result<(HttpsServiceBinding, Option<Duration>)> {
    let (mut bindings, ttl) = doh_lookup_https_bindings(state, host).await?;
    bindings.sort_by_key(|binding| binding.priority);
    Ok((bindings.into_iter().next().unwrap_or_default(), ttl))
}

async fn doh_lookup_ip_addrs(
    state: &AppState,
    host: &str,
) -> Result<(Vec<IpAddr>, Option<Duration>)> {
    let mut addrs = Vec::new();

    let (ipv4_answers, ipv6_answers) =
        tokio::try_join!(doh_query(state, host, "A"), doh_query(state, host, "AAAA"))?;

    if state.config.managed_prefer_ipv6 {
        for answer in &ipv6_answers {
            if answer.record_type == 28
                && let Ok(ip) = answer.data.parse::<std::net::Ipv6Addr>()
            {
                addrs.push(IpAddr::V6(ip));
            }
        }
        for answer in &ipv4_answers {
            if answer.record_type == 1
                && let Ok(ip) = answer.data.parse::<std::net::Ipv4Addr>()
            {
                addrs.push(IpAddr::V4(ip));
            }
        }
    } else {
        for answer in &ipv4_answers {
            if answer.record_type == 1
                && let Ok(ip) = answer.data.parse::<std::net::Ipv4Addr>()
            {
                addrs.push(IpAddr::V4(ip));
            }
        }
        for answer in &ipv6_answers {
            if answer.record_type == 28
                && let Ok(ip) = answer.data.parse::<std::net::Ipv6Addr>()
            {
                addrs.push(IpAddr::V6(ip));
            }
        }
    }

    let ttl = min_duration_options(
        min_ttl_duration(&ipv6_answers),
        min_ttl_duration(&ipv4_answers),
    );

    Ok((addrs, ttl))
}

async fn doh_lookup_https_bindings(
    state: &AppState,
    host: &str,
) -> Result<(Vec<HttpsServiceBinding>, Option<Duration>)> {
    let mut bindings = Vec::new();

    let answers = doh_query(state, host, "HTTPS").await?;
    for answer in answers.iter() {
        if answer.record_type != 65 {
            continue;
        }

        match parse_https_answer(&answer.data) {
            Ok(binding) => bindings.push(binding),
            Err(error) => eprintln!("failed to parse HTTPS RR for {host}: {error:#}"),
        }
    }

    Ok((bindings, min_ttl_duration(&answers)))
}

async fn doh_query(state: &AppState, host: &str, record_type: &str) -> Result<Vec<DohAnswer>> {
    let cache_key = DohCacheKey {
        host: host.to_ascii_lowercase(),
        record_type: record_type.to_string(),
    };
    if let Some(cached) = read_cached_doh_answers(state, &cache_key).await {
        return Ok(cached);
    }

    if state.config.doh_endpoints.is_empty() {
        anyhow::bail!("DoH 不可用，请在配置中自行更换 DoH：未配置 DoH 端点");
    }

    let mut last_error = None;
    for endpoint in &state.config.doh_endpoints {
        match doh_query_once(&state.doh_client, endpoint, host, record_type).await {
            Ok(answers) => {
                write_cached_doh_answers(state, cache_key, answers.clone()).await;
                return Ok(answers);
            }
            Err(error) => {
                eprintln!("DoH query {record_type} {host} via {endpoint} failed: {error:#}");
                last_error = Some(anyhow::anyhow!(
                    "DoH 不可用，请在配置中自行更换 DoH：{endpoint}: {error:#}"
                ));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("DoH 不可用：所有 DoH 端点都失败了")))
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

async fn prioritize_preferred_upstream(
    state: &AppState,
    key: &ResolveCacheKey,
    addrs: &mut Vec<SocketAddr>,
) {
    let preferred = {
        let preferred = state.preferred_upstream_addr.read().await;
        preferred.get(key).copied()
    };

    if let Some(preferred) = preferred
        && let Some(index) = addrs.iter().position(|addr| *addr == preferred)
        && index > 0
    {
        addrs.swap(0, index);
    }
}

async fn remember_successful_upstream(state: &AppState, host: &str, port: u16, addr: SocketAddr) {
    let key = ResolveCacheKey {
        host: host.to_ascii_lowercase(),
        port,
    };
    let mut preferred = state.preferred_upstream_addr.write().await;
    preferred.insert(key, addr);
}

async fn write_cached_upstream(
    state: &AppState,
    key: ResolveCacheKey,
    upstream: ResolvedUpstream,
    ttl: Duration,
) {
    if ttl.is_zero() {
        return;
    }
    let mut cache = state.resolve_cache.write().await;
    cache.retain(|_, entry| Instant::now() < entry.expires_at);
    cache.insert(
        key,
        CachedResolvedUpstream {
            upstream,
            expires_at: Instant::now() + ttl,
        },
    );
}

async fn read_cached_doh_answers(state: &AppState, key: &DohCacheKey) -> Option<Vec<DohAnswer>> {
    let cache = state.doh_cache.read().await;
    cache.get(key).and_then(|entry| {
        if Instant::now() < entry.expires_at {
            Some(entry.answers.clone())
        } else {
            None
        }
    })
}

async fn write_cached_doh_answers(state: &AppState, key: DohCacheKey, answers: Vec<DohAnswer>) {
    let Some(ttl) = min_ttl_duration(&answers) else {
        return;
    };
    if ttl.is_zero() {
        return;
    }

    let mut cache = state.doh_cache.write().await;
    cache.retain(|_, entry| Instant::now() < entry.expires_at);
    cache.insert(
        key,
        CachedDohAnswers {
            answers,
            expires_at: Instant::now() + ttl,
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

fn min_ttl_duration(answers: &[DohAnswer]) -> Option<Duration> {
    answers
        .iter()
        .filter_map(|answer| answer.ttl)
        .min()
        .map(|ttl| Duration::from_secs(ttl as u64))
}

fn min_duration_options(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
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

fn should_trace_upstream(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "cdn3.linux.do" | "linux.do"
    )
}

fn log_upstream_debug(state: &AppState, host: &str, path_and_query: &str, message: &str) {
    if !should_trace_upstream(host) {
        return;
    }

    let _ = runtime_log::append(
        &state.paths,
        "INFO",
        "proxy-upstream",
        &format!("host={host} path={path_and_query} {message}"),
    );
}

fn format_socket_addrs(addrs: &[SocketAddr]) -> String {
    addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
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

fn extract_host(headers: &HeaderMap, uri: &http::Uri, config: &AppConfig) -> Option<String> {
    uri.authority()
        .map(|authority| authority.host().to_ascii_lowercase())
        .or_else(|| {
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
