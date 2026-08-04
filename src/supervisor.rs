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
        }
    }

    async fn proxy(
        &self,
        site: ResolvedSite,
        request: Request<Body>,
    ) -> anyhow::Result<Response<Body>> {
        let service = {
            let mut services = self.services.lock().await;
            services
                .entry(site.directory.clone())
                .or_insert_with(|| {
                    Arc::new(Service {
                        state: Mutex::new(ServiceState {
                            phase: Phase::Stopped,
                            last_activity: Instant::now(),
                            active_requests: 0,
                        }),
                        changed: Notify::new(),
                    })
                })
                .clone()
        };
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
                service.changed.notified().await;
            }
            Phase::Stopped => {
                state.phase = Phase::Starting;
                drop(state);
                let started = start_process(site).await;
                let mut state = service.state.lock().await;
                match started {
                    Ok((port, child)) => {
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
