use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderValue, Request, Response, StatusCode, header},
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    sync::{Mutex, Notify},
};
use tracing::{info, warn};

use crate::{
    manifest::Serve,
    resolver::{ResolvedSite, resolve},
};

pub struct Supervisor {
    root: PathBuf,
    client: reqwest::Client,
    services: Arc<Mutex<HashMap<PathBuf, Arc<Service>>>>,
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
}

enum Phase {
    Stopped,
    Starting,
    Running { port: u16, child: Child },
}

impl Supervisor {
    pub fn new(root: PathBuf, client: reqwest::Client) -> Self {
        Self {
            root,
            client,
            services: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn handle(&self, request: Request<Body>) -> anyhow::Result<Response<Body>> {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let Some(site) = resolve(&self.root, host).await? else {
            return Ok(response(StatusCode::NOT_FOUND, "unknown domain\n"));
        };
        match &site.manifest.serve {
            Serve::Static { root, index } => {
                serve_static(&site.directory, root, index, request.uri().path()).await
            }
            Serve::Http { .. } => self.proxy(site, request).await,
            Serve::Stdio { .. } => self.stdio(site, request).await,
        }
    }

    async fn proxy(
        &self,
        site: ResolvedSite,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let service = self.service_for(&site.directory).await;
        let port = ensure_running(service.clone(), &site).await?;
        {
            let mut state = service.state.lock().await;
            state.active_requests += 1;
            state.last_activity = Instant::now();
        }
        let result = self.proxy_to_port(port, request).await;
        {
            let mut state = service.state.lock().await;
            state.active_requests = state.active_requests.saturating_sub(1);
            state.last_activity = Instant::now();
        }
        result
    }

    async fn stdio(
        &self,
        site: ResolvedSite,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let service = self.service_for(&site.directory).await;
        ensure_prepared(service, &site).await?;
        run_stdio(&site, request).await
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
                    }),
                    changed: Notify::new(),
                })
            })
            .clone()
    }

    async fn proxy_to_port(
        &self,
        port: u16,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let (parts, body) = request.into_parts();
        let url = format!(
            "http://127.0.0.1:{port}{}",
            parts
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str())
        );
        let mut upstream = self
            .client
            .request(parts.method, url)
            .headers(parts.headers);
        let body = to_bytes(body, usize::MAX)
            .await
            .context("failed to read request body")?;
        upstream = upstream.body(body);
        let upstream = upstream.send().await.context("upstream request failed")?;
        let status = upstream.status();
        let headers = upstream.headers().clone();
        let bytes = upstream
            .bytes()
            .await
            .context("failed to read upstream response")?;
        let mut response = Response::builder().status(status).body(Body::from(bytes))?;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

async fn ensure_running(service: Arc<Service>, site: &ResolvedSite) -> anyhow::Result<u16> {
    loop {
        let notified = service.changed.notified();
        let mut state = service.state.lock().await;
        state.last_activity = Instant::now();
        match &mut state.phase {
            Phase::Running { port, child } => {
                if child.try_wait()?.is_none() {
                    return Ok(*port);
                }
                warn!(domain = %site.domain, "site process exited; restarting");
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
                    Ok((port, child)) => {
                        state.prepared = true;
                        state.phase = Phase::Running { port, child };
                        state.last_activity = Instant::now();
                        service.changed.notify_waiters();
                        spawn_idle_reaper(service.clone(), site.clone());
                        return Ok(port);
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
        if state.prepared {
            return Ok(());
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
    let body = to_bytes(body, usize::MAX)
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
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(&body)
            .await
            .context("failed to write request body to site")?;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(*timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .context("stdio site timed out")??;
    if !output.status.success() {
        bail!("stdio site exited with {}", output.status);
    }
    parse_cgi_response(&output.stdout)
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

async fn start_process(site: &ResolvedSite) -> anyhow::Result<(u16, Child)> {
    let Serve::Http {
        command,
        environment,
        working_directory,
        port_environment,
        startup_timeout_seconds,
    } = &site.manifest.serve
    else {
        bail!("not an HTTP service");
    };
    let socket = TcpListener::bind("127.0.0.1:0").await?;
    let port = socket.local_addr()?.port();
    drop(socket);
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .current_dir(
            working_directory
                .as_ref()
                .map_or(site.directory.clone(), |dir| site.directory.join(dir)),
        )
        .envs(environment)
        .env(port_environment, port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let child = process
        .spawn()
        .with_context(|| format!("failed to start {}", command[0]))?;
    let timeout = Duration::from_secs(*startup_timeout_seconds);
    tokio::time::timeout(timeout, async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| format!("site did not listen on port {port} within {timeout:?}"))?;
    info!(domain = %site.domain, port, "site process started");
    Ok((port, child))
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
            if let Phase::Running { child, .. } = &mut state.phase {
                info!(domain = %site.domain, "stopping idle site process");
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            state.phase = Phase::Stopped;
            service.changed.notify_waiters();
            return;
        }
    });
}

async fn serve_static(
    site_dir: &Path,
    root: &str,
    indexes: &[String],
    uri_path: &str,
) -> anyhow::Result<Response<Body>> {
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
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(response(StatusCode::NOT_FOUND, "not found\n"));
        }
        Err(error) => return Err(error.into()),
    };
    let content_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type)?);
    Ok(response)
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
}
