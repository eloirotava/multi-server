use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, header},
};
use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpListener,
    process::{Child, Command},
    sync::{Mutex, Notify},
};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

use crate::{
    manifest::{Network, Readiness, Serve},
    resolver::{ResolvedSite, resolve},
};

pub struct Supervisor {
    root: PathBuf,
    client: reqwest::Client,
    services: Arc<Mutex<HashMap<PathBuf, Arc<Service>>>>,
    _watcher: StdMutex<RecommendedWatcher>,
}

struct Service {
    state: Mutex<ServiceState>,
    changed: Notify,
}

struct ServiceState {
    phase: Phase,
    last_activity: Instant,
    active_requests: usize,
    prepared: bool,
    dirty: bool,
}

struct RequestActivity {
    service: Arc<Service>,
    renew_idle: bool,
}

impl RequestActivity {
    async fn begin(service: Arc<Service>, maximum: usize, renew_idle: bool) -> Option<Self> {
        let mut state = service.state.lock().await;
        if state.active_requests >= maximum {
            return None;
        }
        state.active_requests += 1;
        if renew_idle {
            state.last_activity = Instant::now();
        }
        drop(state);
        Some(Self {
            service,
            renew_idle,
        })
    }
}

impl Drop for RequestActivity {
    fn drop(&mut self) {
        let service = self.service.clone();
        let renew_idle = self.renew_idle;
        tokio::spawn(async move {
            let mut state = service.state.lock().await;
            state.active_requests = state.active_requests.saturating_sub(1);
            if renew_idle {
                state.last_activity = Instant::now();
            }
            service.changed.notify_waiters();
        });
    }
}

enum Phase {
    Stopped,
    Starting,
    Running {
        upstream: Upstream,
        child: Child,
        namespace: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct Upstream {
    host: String,
    port: u16,
}

const TCP_READINESS: Readiness = Readiness::Tcp;
const HOST_NETWORK: Network = Network::Host;

impl Supervisor {
    pub fn new(root: PathBuf, client: reqwest::Client) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create site root {}", root.display()))?;
        let services = Arc::new(Mutex::new(HashMap::<PathBuf, Arc<Service>>::new()));
        let (changes, mut changed_paths) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            if let Ok(event) = event {
                let _ = changes.send(event);
            }
        })
        .context("failed to create site file watcher")?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;
        let watched_services = services.clone();
        tokio::spawn(async move {
            while let Some(event) = changed_paths.recv().await {
                if !event
                    .paths
                    .iter()
                    .any(|path| path.file_name().is_some_and(|name| name == "site.json"))
                {
                    continue;
                }
                let services = watched_services.lock().await;
                for (directory, service) in services.iter() {
                    if event.paths.iter().any(|path| {
                        path.starts_with(directory)
                            || path
                                .parent()
                                .is_some_and(|parent| directory.starts_with(parent))
                    }) {
                        let mut state = service.state.lock().await;
                        state.dirty = true;
                        service.changed.notify_waiters();
                    }
                }
            }
        });
        Ok(Self {
            root,
            client,
            services,
            _watcher: StdMutex::new(watcher),
        })
    }

    pub async fn handle(&self, request: Request<Body>) -> anyhow::Result<Response<Body>> {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let Some(mut site) = resolve(&self.root, host).await? else {
            return Ok(response(StatusCode::NOT_FOUND, "unknown domain\n"));
        };
        if let Some((index, route)) = site
            .manifest
            .routes
            .iter()
            .enumerate()
            .find(|(_, route)| route.matches(request.uri().path()))
        {
            site.runtime_key = site
                .manifest_directory
                .join(format!(".multi-server-route-{index}"));
            site.manifest.serve = route.serve.clone();
        }
        match &site.manifest.serve {
            Serve::Static { root, index } => {
                serve_static(&site.directory, root, index, request).await
            }
            Serve::Http { .. } => self.proxy(site, request).await,
            Serve::Stdio { .. } => self.stdio(site, request).await,
            Serve::Fastcgi { .. } => self.fastcgi(site, request).await,
        }
    }

    async fn proxy(
        &self,
        site: ResolvedSite,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let service = self.service_for(&site.runtime_key).await;
        let upstream = ensure_running(service.clone(), &site).await?;
        if request_body_too_large(&request, site.manifest.limits.request_body_bytes) {
            return Ok(response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large\n",
            ));
        }
        let renew_idle = path_is_activity(&site, request.uri().path());
        let Some(activity) = RequestActivity::begin(
            service,
            site.manifest.limits.max_concurrent_requests,
            renew_idle,
        )
        .await
        else {
            return Ok(response(
                StatusCode::SERVICE_UNAVAILABLE,
                "site concurrency limit reached\n",
            ));
        };
        if is_upgrade_request(&request) {
            return self.proxy_websocket(&upstream, request, activity).await;
        }
        self.proxy_to_upstream(
            &upstream,
            request,
            activity,
            site.manifest.limits.request_body_bytes,
        )
        .await
    }

    async fn proxy_websocket(
        &self,
        upstream: &Upstream,
        mut request: Request<Body>,
        activity: RequestActivity,
    ) -> anyhow::Result<Response<Body>> {
        let client_upgrade = hyper::upgrade::on(&mut request);
        let stream = tokio::net::TcpStream::connect((upstream.host.as_str(), upstream.port))
            .await
            .context("failed to connect WebSocket upstream")?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.with_upgrades().await {
                warn!(%error, "WebSocket upstream connection failed");
            }
        });
        let mut upstream_response = sender.send_request(request).await?;
        let upstream_upgrade = hyper::upgrade::on(&mut upstream_response);
        tokio::spawn(async move {
            let _activity = activity;
            let (client, upstream) = match tokio::try_join!(client_upgrade, upstream_upgrade) {
                Ok(upgrades) => upgrades,
                Err(error) => {
                    warn!(%error, "WebSocket upgrade failed");
                    return;
                }
            };
            let mut client = TokioIo::new(client);
            let mut upstream = TokioIo::new(upstream);
            if let Err(error) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                warn!(%error, "WebSocket tunnel failed");
            }
        });
        let (parts, body) = upstream_response.into_parts();
        Ok(Response::from_parts(parts, Body::new(body)))
    }

    async fn stdio(
        &self,
        site: ResolvedSite,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let service = self.service_for(&site.runtime_key).await;
        ensure_prepared(service.clone(), &site).await?;
        if request_body_too_large(&request, site.manifest.limits.request_body_bytes) {
            return Ok(response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large\n",
            ));
        }
        let renew_idle = path_is_activity(&site, request.uri().path());
        let Some(_activity) = RequestActivity::begin(
            service,
            site.manifest.limits.max_concurrent_requests,
            renew_idle,
        )
        .await
        else {
            return Ok(response(
                StatusCode::SERVICE_UNAVAILABLE,
                "site concurrency limit reached\n",
            ));
        };
        run_stdio(&site, request).await
    }

    async fn fastcgi(
        &self,
        site: ResolvedSite,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let service = self.service_for(&site.runtime_key).await;
        let upstream = ensure_running(service.clone(), &site).await?;
        if request_body_too_large(&request, site.manifest.limits.request_body_bytes) {
            return Ok(response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large\n",
            ));
        }
        let renew_idle = path_is_activity(&site, request.uri().path());
        let Some(_activity) = RequestActivity::begin(
            service,
            site.manifest.limits.max_concurrent_requests,
            renew_idle,
        )
        .await
        else {
            return Ok(response(
                StatusCode::SERVICE_UNAVAILABLE,
                "site concurrency limit reached\n",
            ));
        };
        forward_fastcgi(&site, &upstream, request).await
    }

    async fn service_for(&self, directory: &Path) -> Arc<Service> {
        let mut services = self.services.lock().await;
        services
            .entry(directory.to_path_buf())
            .or_insert_with(|| {
                Arc::new(Service {
                    state: Mutex::new(ServiceState {
                        phase: Phase::Stopped,
                        last_activity: Instant::now(),
                        active_requests: 0,
                        prepared: false,
                        dirty: false,
                    }),
                    changed: Notify::new(),
                })
            })
            .clone()
    }

    async fn proxy_to_upstream(
        &self,
        upstream: &Upstream,
        request: Request<Body>,
        activity: RequestActivity,
        body_limit: usize,
    ) -> anyhow::Result<Response<Body>> {
        let (parts, body) = request.into_parts();
        let url = format!(
            "http://{}:{}{}",
            upstream.host,
            upstream.port,
            parts
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str())
        );
        let mut request_headers = parts.headers;
        remove_hop_by_hop_headers(&mut request_headers);
        let mut received = 0usize;
        let mut incoming = body.into_data_stream();
        let limited_body = async_stream::stream! {
            while let Some(chunk) = incoming.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err::<axum::body::Bytes, _>(std::io::Error::other(error.to_string()));
                        break;
                    }
                };
                received = received.saturating_add(chunk.len());
                if received > body_limit {
                    yield Err::<axum::body::Bytes, _>(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "request body limit exceeded",
                    ));
                    break;
                }
                yield Ok(chunk);
            }
        };
        let upstream = self
            .client
            .request(parts.method, url)
            .headers(request_headers)
            .body(reqwest::Body::wrap_stream(limited_body));
        let upstream = upstream.send().await.context("upstream request failed")?;
        let status = upstream.status();
        let mut headers = upstream.headers().clone();
        remove_hop_by_hop_headers(&mut headers);
        let mut upstream_body = upstream.bytes_stream();
        let body = async_stream::stream! {
            let _activity = activity;
            while let Some(chunk) = upstream_body.next().await {
                yield chunk;
            }
        };
        let mut response = Response::builder()
            .status(status)
            .body(Body::from_stream(body))?;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let connection_headers = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .filter_map(|name| name.trim().parse().ok())
                .collect::<Vec<HeaderName>>()
        })
        .unwrap_or_default();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn is_upgrade_request(request: &Request<Body>) -> bool {
    request.headers().contains_key(header::UPGRADE)
        && request
            .headers()
            .get(header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn path_is_activity(site: &ResolvedSite, path: &str) -> bool {
    site.manifest
        .lifecycle
        .activity_paths
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn request_body_too_large(request: &Request<Body>, limit: usize) -> bool {
    request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
}

async fn ensure_running(service: Arc<Service>, site: &ResolvedSite) -> anyhow::Result<Upstream> {
    loop {
        let notified = service.changed.notified();
        let mut state = service.state.lock().await;
        state.last_activity = Instant::now();
        if state.dirty && matches!(state.phase, Phase::Running { .. }) {
            if state.active_requests > 0 {
                drop(state);
                notified.await;
                continue;
            }
            if let Phase::Running {
                child, namespace, ..
            } = &mut state.phase
            {
                terminate_process_group(child, site.manifest.lifecycle.shutdown_grace_seconds)
                    .await;
                cleanup_namespace(namespace.take()).await;
            }
            state.phase = Phase::Stopped;
            state.prepared = false;
            continue;
        }
        match &mut state.phase {
            Phase::Running {
                upstream,
                child,
                namespace,
            } => {
                if child.try_wait()?.is_none() {
                    return Ok(upstream.clone());
                }
                warn!(domain = %site.domain, "site process exited; restarting");
                cleanup_namespace(namespace.take()).await;
                state.phase = Phase::Stopped;
            }
            Phase::Starting => {
                drop(state);
                notified.await;
            }
            Phase::Stopped => {
                state.phase = Phase::Starting;
                let needs_prepare = !state.prepared;
                drop(state);
                let started = async {
                    if needs_prepare {
                        prepare_site(site).await?;
                    }
                    start_process(site).await
                }
                .await;
                let mut state = service.state.lock().await;
                match started {
                    Ok((upstream, child, namespace)) => {
                        state.prepared = true;
                        state.dirty = false;
                        state.phase = Phase::Running {
                            upstream: upstream.clone(),
                            child,
                            namespace,
                        };
                        state.last_activity = Instant::now();
                        service.changed.notify_waiters();
                        spawn_idle_reaper(service.clone(), site.clone());
                        return Ok(upstream);
                    }
                    Err(error) => {
                        state.phase = Phase::Stopped;
                        service.changed.notify_waiters();
                        return Err(error);
                    }
                }
            }
        }
    }
}

async fn ensure_prepared(service: Arc<Service>, site: &ResolvedSite) -> anyhow::Result<()> {
    loop {
        let notified = service.changed.notified();
        let mut state = service.state.lock().await;
        if state.dirty {
            state.prepared = false;
        }
        if state.prepared {
            return Ok(());
        }
        if state.active_requests > 0 {
            drop(state);
            notified.await;
            continue;
        }
        match &state.phase {
            Phase::Starting => {
                drop(state);
                notified.await;
            }
            Phase::Stopped => {
                state.phase = Phase::Starting;
                drop(state);
                let prepared = prepare_site(site).await;
                let mut state = service.state.lock().await;
                state.phase = Phase::Stopped;
                if prepared.is_ok() {
                    state.prepared = true;
                    state.dirty = false;
                }
                service.changed.notify_waiters();
                return prepared;
            }
            Phase::Running { .. } => bail!("invalid running state for stdio site"),
        }
    }
}

async fn prepare_site(site: &ResolvedSite) -> anyhow::Result<()> {
    for command in &site.manifest.prepare {
        info!(domain = %site.domain, command = ?command, "preparing site");
        let status = Command::new(&command[0])
            .args(&command[1..])
            .current_dir(&site.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("failed to run prepare command {}", command[0]))?;
        if !status.success() {
            bail!("prepare command {} exited with {status}", command[0]);
        }
    }
    Ok(())
}

async fn run_stdio(site: &ResolvedSite, request: Request<Body>) -> anyhow::Result<Response<Body>> {
    let Serve::Stdio {
        command,
        environment,
        working_directory,
        timeout_seconds,
    } = &site.manifest.serve
    else {
        bail!("not a stdio service");
    };
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, site.manifest.limits.request_body_bytes)
        .await
        .context("failed to read request body")?;
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .current_dir(
            working_directory
                .as_ref()
                .map_or(site.directory.clone(), |dir| site.directory.join(dir)),
        )
        .envs(environment)
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("REQUEST_PATH", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or_default())
        .env("HTTP_HOST", &site.domain)
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    configure_process_group(&mut process);
    for (name, value) in &parts.headers {
        if let Ok(value) = value.to_str() {
            let name = format!(
                "HTTP_{}",
                name.as_str().to_ascii_uppercase().replace('-', "_")
            );
            process.env(name, value);
        }
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to start {}", command[0]))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&body)
            .await
            .context("failed to write request body to site")?;
    }
    let stdout = child.stdout.take().context("stdio site has no stdout")?;
    let output_limit = site.manifest.limits.stdio_output_bytes;
    let output_reader = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout
            .take(output_limit.saturating_add(1) as u64)
            .read_to_end(&mut output)
            .await?;
        Ok::<_, std::io::Error>(output)
    });
    let status =
        match tokio::time::timeout(Duration::from_secs(*timeout_seconds), child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                terminate_process_group(&mut child, 1).await;
                bail!("stdio site timed out");
            }
        };
    let output = output_reader
        .await
        .context("stdio output reader failed")??;
    if !status.success() {
        bail!("stdio site exited with {status}");
    }
    if output.len() > output_limit {
        bail!("stdio site exceeded its output limit");
    }
    parse_cgi_response(&output)
}

async fn forward_fastcgi(
    site: &ResolvedSite,
    upstream: &Upstream,
    request: Request<Body>,
) -> anyhow::Result<Response<Body>> {
    let Serve::Fastcgi {
        document_root,
        front_controller,
        ..
    } = &site.manifest.serve
    else {
        bail!("not a FastCGI service");
    };
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, site.manifest.limits.request_body_bytes)
        .await
        .context("failed to read FastCGI request body")?;
    let root = site.directory.join(document_root);
    let requested = parts.uri.path().trim_start_matches('/');
    let requested_path = Path::new(requested);
    let safe_requested = !requested_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)));
    let requested_script = root.join(requested_path);
    let direct_script = safe_requested
        && requested.ends_with(".php")
        && tokio::fs::metadata(&requested_script)
            .await
            .is_ok_and(|metadata| metadata.is_file());
    let (script_filename, script_name) = if direct_script {
        (requested_script, format!("/{requested}"))
    } else {
        (root.join(front_controller), format!("/{front_controller}"))
    };
    if !tokio::fs::try_exists(&script_filename).await? {
        return Ok(response(
            StatusCode::NOT_FOUND,
            "FastCGI script not found\n",
        ));
    }
    let mut params = vec![
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(), "multi-server".into()),
        ("SERVER_PROTOCOL".into(), format!("{:?}", parts.version)),
        ("REQUEST_METHOD".into(), parts.method.to_string()),
        ("REQUEST_URI".into(), parts.uri.to_string()),
        ("DOCUMENT_URI".into(), parts.uri.path().into()),
        (
            "QUERY_STRING".into(),
            parts.uri.query().unwrap_or_default().into(),
        ),
        ("SCRIPT_NAME".into(), script_name),
        (
            "SCRIPT_FILENAME".into(),
            script_filename.to_string_lossy().into_owned(),
        ),
        ("DOCUMENT_ROOT".into(), root.to_string_lossy().into_owned()),
        ("SERVER_NAME".into(), site.domain.clone()),
        ("SERVER_PORT".into(), "80".into()),
        ("REMOTE_ADDR".into(), "127.0.0.1".into()),
        ("CONTENT_LENGTH".into(), body.len().to_string()),
        ("REDIRECT_STATUS".into(), "200".into()),
    ];
    for (name, value) in &parts.headers {
        let Ok(value) = value.to_str() else { continue };
        if name == header::CONTENT_TYPE {
            params.push(("CONTENT_TYPE".into(), value.into()));
        } else if name != header::CONTENT_LENGTH {
            params.push((
                format!(
                    "HTTP_{}",
                    name.as_str().to_ascii_uppercase().replace('-', "_")
                ),
                value.into(),
            ));
        }
    }
    if parts
        .headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
    {
        params.push(("HTTPS".into(), "on".into()));
        params.push(("SERVER_PORT".into(), "443".into()));
    }
    let mut stream = tokio::net::TcpStream::connect((upstream.host.as_str(), upstream.port))
        .await
        .context("failed to connect FastCGI upstream")?;
    write_fastcgi_record(&mut stream, 1, &[0, 1, 0, 0, 0, 0, 0, 0]).await?;
    let encoded_params = encode_fastcgi_params(&params);
    for chunk in encoded_params.chunks(u16::MAX as usize) {
        write_fastcgi_record(&mut stream, 4, chunk).await?;
    }
    write_fastcgi_record(&mut stream, 4, &[]).await?;
    for chunk in body.chunks(u16::MAX as usize) {
        write_fastcgi_record(&mut stream, 5, chunk).await?;
    }
    write_fastcgi_record(&mut stream, 5, &[]).await?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let mut header = [0_u8; 8];
        stream.read_exact(&mut header).await?;
        if header[0] != 1 || u16::from_be_bytes([header[2], header[3]]) != 1 {
            bail!("invalid FastCGI response");
        }
        let content_length = u16::from_be_bytes([header[4], header[5]]) as usize;
        let padding_length = header[6] as usize;
        let mut content = vec![0; content_length];
        stream.read_exact(&mut content).await?;
        if padding_length > 0 {
            let mut padding = vec![0; padding_length];
            stream.read_exact(&mut padding).await?;
        }
        match header[1] {
            3 => break,
            6 => stdout.extend_from_slice(&content),
            7 => stderr.extend_from_slice(&content),
            _ => {}
        }
        if stdout.len() > site.manifest.limits.stdio_output_bytes {
            bail!("FastCGI response exceeded its output limit");
        }
    }
    if !stderr.is_empty() {
        warn!(domain = %site.domain, message = %String::from_utf8_lossy(&stderr), "FastCGI stderr");
    }
    parse_cgi_response(&stdout)
}

async fn write_fastcgi_record(
    stream: &mut tokio::net::TcpStream,
    record_type: u8,
    content: &[u8],
) -> std::io::Result<()> {
    let padding = (8 - content.len() % 8) % 8;
    let length = content.len() as u16;
    stream
        .write_all(&[
            1,
            record_type,
            0,
            1,
            (length >> 8) as u8,
            length as u8,
            padding as u8,
            0,
        ])
        .await?;
    stream.write_all(content).await?;
    if padding > 0 {
        stream.write_all(&[0; 8][..padding]).await?;
    }
    Ok(())
}

fn encode_fastcgi_params(params: &[(String, String)]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in params {
        encode_fastcgi_length(name.len(), &mut encoded);
        encode_fastcgi_length(value.len(), &mut encoded);
        encoded.extend_from_slice(name.as_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    encoded
}

fn encode_fastcgi_length(length: usize, output: &mut Vec<u8>) {
    if length < 128 {
        output.push(length as u8);
    } else {
        output.extend_from_slice(&((length as u32) | 0x8000_0000).to_be_bytes());
    }
}

fn parse_cgi_response(output: &[u8]) -> anyhow::Result<Response<Body>> {
    let split = output
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| (at, 4))
        .or_else(|| {
            output
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|at| (at, 2))
        });
    let Some((header_end, separator_len)) = split else {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(output.to_vec()))?);
    };
    let headers = std::str::from_utf8(&output[..header_end])
        .context("stdio response headers are not UTF-8")?;
    let mut builder = Response::builder().status(StatusCode::OK);
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            bail!("invalid stdio response header");
        };
        if name.eq_ignore_ascii_case("status") {
            let status = value
                .split_whitespace()
                .next()
                .context("empty Status header")?
                .parse::<u16>()?;
            builder = builder.status(status);
        } else {
            builder = builder.header(name.trim(), value.trim());
        }
    }
    Ok(builder.body(Body::from(output[header_end + separator_len..].to_vec()))?)
}

async fn start_process(site: &ResolvedSite) -> anyhow::Result<(Upstream, Child, Option<String>)> {
    let (
        command,
        environment,
        working_directory,
        port,
        port_environment,
        upstream_host,
        startup_timeout_seconds,
        readiness,
        network,
    ) = match &site.manifest.serve {
        Serve::Http {
            command,
            environment,
            working_directory,
            port_environment,
            port,
            upstream_host,
            startup_timeout_seconds,
            readiness,
            network,
        } => (
            command,
            environment,
            working_directory,
            port.as_ref(),
            port_environment,
            upstream_host,
            startup_timeout_seconds,
            readiness,
            network,
        ),
        Serve::Fastcgi {
            command,
            environment,
            working_directory,
            port_environment,
            upstream_host,
            startup_timeout_seconds,
            ..
        } => (
            command,
            environment,
            working_directory,
            None,
            port_environment,
            upstream_host,
            startup_timeout_seconds,
            &TCP_READINESS,
            &HOST_NETWORK,
        ),
        _ => bail!("serve mode does not start a persistent process"),
    };
    let selected_port = match port {
        Some(port) => *port,
        None => {
            let socket = TcpListener::bind("127.0.0.1:0").await?;
            let port = socket.local_addr()?.port();
            drop(socket);
            port
        }
    };
    let variables = RuntimeVariables {
        port: selected_port,
        domain: &site.domain,
        site_root: &site.directory,
    };
    let expanded_command = command
        .iter()
        .map(|value| expand_runtime_variables(value, &variables))
        .collect::<Vec<_>>();
    let namespace = match network {
        Network::Host => None,
        Network::Namespace => Some(create_namespace(&site.directory).await?),
    };
    let mut process = if let Some(namespace) = &namespace {
        let mut process = Command::new("ip");
        process.args(["netns", "exec", namespace, &expanded_command[0]]);
        process
    } else {
        Command::new(&expanded_command[0])
    };
    process
        .args(&expanded_command[1..])
        .current_dir(
            working_directory
                .as_ref()
                .map_or(site.directory.clone(), |dir| site.directory.join(dir)),
        )
        .envs(
            environment
                .iter()
                .map(|(name, value)| (name, expand_runtime_variables(value, &variables))),
        )
        .env(port_environment, selected_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    configure_process_group(&mut process);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            cleanup_namespace(namespace.clone()).await;
            return Err(error).with_context(|| format!("failed to start {}", command[0]));
        }
    };
    let upstream = Upstream {
        host: if let Some(namespace) = &namespace {
            namespace_address(namespace)
        } else {
            expand_runtime_variables(upstream_host, &variables)
        },
        port: selected_port,
    };
    let timeout = Duration::from_secs(*startup_timeout_seconds);
    let readiness_client = reqwest::Client::new();
    let ready = tokio::time::timeout(timeout, async {
        loop {
            if readiness_ready(&readiness_client, &upstream, readiness).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if ready.is_err() {
        terminate_process_group(&mut child, site.manifest.lifecycle.shutdown_grace_seconds).await;
        cleanup_namespace(namespace.clone()).await;
        bail!(
            "site did not become ready on {}:{} within {timeout:?}",
            upstream.host,
            upstream.port
        );
    }
    info!(domain = %site.domain, host = %upstream.host, port = upstream.port, "site process started");
    Ok((upstream, child, namespace))
}

async fn readiness_ready(
    client: &reqwest::Client,
    upstream: &Upstream,
    readiness: &Readiness,
) -> bool {
    match readiness {
        Readiness::Tcp => tokio::net::TcpStream::connect((upstream.host.as_str(), upstream.port))
            .await
            .is_ok(),
        Readiness::Http { path, status } => {
            let url = format!("http://{}:{}{}", upstream.host, upstream.port, path);
            client
                .get(url)
                .send()
                .await
                .is_ok_and(|response| response.status().as_u16() == *status)
        }
    }
}

async fn create_namespace(site_directory: &Path) -> anyhow::Result<String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    site_directory.hash(&mut hasher);
    let id = hasher.finish() as u32;
    let namespace = format!("ms{id:08x}");
    let host_interface = format!("mh{id:08x}");
    let namespace_interface = format!("mn{id:08x}");
    let (host_address, guest_address) = namespace_addresses(id);
    cleanup_namespace(Some(namespace.clone())).await;
    let commands = [
        vec!["netns", "add", &namespace],
        vec![
            "link",
            "add",
            &host_interface,
            "type",
            "veth",
            "peer",
            "name",
            &namespace_interface,
        ],
        vec!["link", "set", &namespace_interface, "netns", &namespace],
        vec!["addr", "add", &host_address, "dev", &host_interface],
        vec!["link", "set", &host_interface, "up"],
        vec!["netns", "exec", &namespace, "ip", "link", "set", "lo", "up"],
        vec![
            "netns",
            "exec",
            &namespace,
            "ip",
            "addr",
            "add",
            &guest_address,
            "dev",
            &namespace_interface,
        ],
        vec![
            "netns",
            "exec",
            &namespace,
            "ip",
            "link",
            "set",
            &namespace_interface,
            "up",
        ],
    ];
    for args in commands {
        let status = Command::new("ip").args(args).status().await?;
        if !status.success() {
            cleanup_namespace(Some(namespace.clone())).await;
            bail!("failed to configure network namespace {namespace}");
        }
    }
    Ok(namespace)
}

fn namespace_addresses(id: u32) -> (String, String) {
    let third = ((id >> 8) & 0xff) as u8;
    let base = ((id & 0x3f) * 4) as u8;
    (
        format!("10.203.{third}.{}/30", base + 1),
        format!("10.203.{third}.{}/30", base + 2),
    )
}

fn namespace_address(namespace: &str) -> String {
    let id = u32::from_str_radix(namespace.trim_start_matches("ms"), 16).unwrap_or_default();
    namespace_addresses(id)
        .1
        .split('/')
        .next()
        .unwrap_or("127.0.0.1")
        .to_owned()
}

async fn cleanup_namespace(namespace: Option<String>) {
    if let Some(namespace) = namespace {
        let _ = Command::new("ip")
            .args(["netns", "del", &namespace])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

struct RuntimeVariables<'a> {
    port: u16,
    domain: &'a str,
    site_root: &'a Path,
}

fn expand_runtime_variables(value: &str, variables: &RuntimeVariables<'_>) -> String {
    value
        .replace("${PORT}", &variables.port.to_string())
        .replace("${DOMAIN}", variables.domain)
        .replace("${SITE_ROOT}", &variables.site_root.to_string_lossy())
}

fn spawn_idle_reaper(service: Arc<Service>, site: ResolvedSite) {
    tokio::spawn(async move {
        let idle = Duration::from_secs(site.manifest.lifecycle.idle_timeout_seconds);
        loop {
            tokio::time::sleep(idle.max(Duration::from_secs(1))).await;
            let mut state = service.state.lock().await;
            if !matches!(state.phase, Phase::Running { .. }) {
                return;
            }
            if state.active_requests > 0 || state.last_activity.elapsed() < idle {
                continue;
            }
            if let Phase::Running {
                child, namespace, ..
            } = &mut state.phase
            {
                info!(domain = %site.domain, "stopping idle site process");
                terminate_process_group(child, site.manifest.lifecycle.shutdown_grace_seconds)
                    .await;
                cleanup_namespace(namespace.take()).await;
            }
            state.phase = Phase::Stopped;
            service.changed.notify_waiters();
            return;
        }
    });
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

async fn terminate_process_group(child: &mut Child, grace_seconds: u64) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        if tokio::time::timeout(Duration::from_secs(grace_seconds), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        let _ = child.wait().await;
        return;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn serve_static(
    site_dir: &Path,
    root: &str,
    indexes: &[String],
    request: Request<Body>,
) -> anyhow::Result<Response<Body>> {
    let uri_path = request.uri().path();
    let relative = uri_path.trim_start_matches('/');
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
        && !relative.as_os_str().is_empty()
    {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid path\n"));
    }
    let root = site_dir.join(root);
    let mut path = root.join(relative);
    if tokio::fs::metadata(&path)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
    {
        if let Some(index) = indexes
            .iter()
            .map(|index| path.join(index))
            .find(|path| path.is_file())
        {
            path = index;
        }
    }
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(response(StatusCode::NOT_FOUND, "not found\n"));
        }
        Ok(_) => return Ok(response(StatusCode::NOT_FOUND, "not found\n")),
        Err(error) => return Err(error.into()),
    };
    let modified = metadata.modified().ok();
    let etag = format!(
        "\"{:x}-{:x}\"",
        metadata.len(),
        modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos())
    );
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|candidate| candidate.trim() == etag.as_str())
        })
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(Body::empty())?);
    }
    if let (Some(modified), Some(since)) = (
        modified,
        request
            .headers()
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| httpdate::parse_http_date(value).ok()),
    ) {
        if modified <= since + Duration::from_secs(1) {
            return Ok(Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag)
                .body(Body::empty())?);
        }
    }
    let (start, end, status) = match request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => match parse_byte_range(value, metadata.len()) {
            Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", metadata.len()))
                    .body(Body::empty())?);
            }
        },
        None => (0, metadata.len().saturating_sub(1), StatusCode::OK),
    };
    let length = if metadata.len() == 0 {
        0
    } else {
        end - start + 1
    };
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let stream = ReaderStream::new(file.take(length));
    let content_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, HeaderValue::from_str(&content_type)?)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag);
    if let Some(modified) = modified {
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(modified));
    }
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", metadata.len()),
        );
    }
    let body = if request.method() == axum::http::Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(stream)
    };
    Ok(builder.body(body)?)
}

fn parse_byte_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let range = value.strip_prefix("bytes=")?;
    if range.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(length);
        return (suffix > 0).then_some((length - suffix, length - 1));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= length {
        return None;
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().ok()?.min(length - 1)
    };
    (start <= end).then_some((start, end))
}

fn response(status: StatusCode, body: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .expect("valid response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cgi_headers_and_status() {
        let response = parse_cgi_response(
            b"Status: 201 Created\r\nContent-Type: text/plain\r\nX-Site: test\r\n\r\ncreated",
        )
        .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
        assert_eq!(response.headers()["x-site"], "test");
    }

    #[test]
    fn treats_headerless_output_as_html() {
        let response = parse_cgi_response(b"<h1>Hello</h1>").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn removes_standard_and_connection_named_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            "keep-alive, x-internal".parse().unwrap(),
        );
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("x-internal", "secret".parse().unwrap());
        headers.insert("x-forwarded-test", "kept".parse().unwrap());

        remove_hop_by_hop_headers(&mut headers);

        assert!(!headers.contains_key(header::CONNECTION));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("x-internal"));
        assert_eq!(headers["x-forwarded-test"], "kept");
    }

    #[test]
    fn expands_generic_runtime_variables() {
        let root = Path::new("/raiz/example.com");
        let variables = RuntimeVariables {
            port: 32123,
            domain: "example.com",
            site_root: root,
        };
        assert_eq!(
            expand_runtime_variables(
                "${SITE_ROOT}/server --domain=${DOMAIN} --port=${PORT}",
                &variables
            ),
            "/raiz/example.com/server --domain=example.com --port=32123"
        );
    }

    #[test]
    fn parses_single_byte_ranges() {
        assert_eq!(parse_byte_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_byte_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_byte_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_byte_range("bytes=100-", 100), None);
        assert_eq!(parse_byte_range("bytes=0-1,3-4", 100), None);
    }

    #[test]
    fn encodes_fastcgi_parameter_lengths() {
        let encoded = encode_fastcgi_params(&[("A".into(), "B".repeat(130))]);
        assert_eq!(encoded[0], 1);
        assert_eq!(&encoded[1..5], &[0x80, 0, 0, 130]);
        assert_eq!(encoded[5], b'A');
        assert_eq!(encoded.len(), 5 + 1 + 130);
    }
}
