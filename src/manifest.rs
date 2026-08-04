use std::{collections::HashMap, path::Path};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default = "manifest_version")]
    pub version: u32,
    #[serde(default)]
    pub domains: Domains,
    pub serve: Serve,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Domains {
    #[serde(default)]
    pub subdomains: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Serve {
    Static {
        #[serde(default = "default_public")]
        root: String,
        #[serde(default = "default_indexes")]
        index: Vec<String>,
    },
    Http {
        command: Vec<String>,
        #[serde(default)]
        environment: HashMap<String, String>,
        #[serde(default)]
        working_directory: Option<String>,
        #[serde(default = "default_port_environment")]
        port_environment: String,
        /// Use a predetermined host port instead of allocating one dynamically.
        #[serde(default)]
        port: Option<u16>,
        #[serde(default = "default_upstream_host")]
        upstream_host: String,
        #[serde(default = "default_startup_timeout")]
        startup_timeout_seconds: u64,
        #[serde(default)]
        readiness: Readiness,
        #[serde(default)]
        network: Network,
    },
    Stdio {
        command: Vec<String>,
        #[serde(default)]
        environment: HashMap<String, String>,
        #[serde(default)]
        working_directory: Option<String>,
        #[serde(default = "default_request_timeout")]
        timeout_seconds: u64,
    },
    Fastcgi {
        command: Vec<String>,
        #[serde(default)]
        environment: HashMap<String, String>,
        #[serde(default)]
        working_directory: Option<String>,
        #[serde(default = "default_port_environment")]
        port_environment: String,
        #[serde(default = "default_upstream_host")]
        upstream_host: String,
        #[serde(default = "default_startup_timeout")]
        startup_timeout_seconds: u64,
        #[serde(default = "default_public")]
        document_root: String,
        #[serde(default = "default_front_controller")]
        front_controller: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub serve: Serve,
}

impl Route {
    pub fn matches(&self, path: &str) -> bool {
        self.path_prefix
            .as_ref()
            .is_some_and(|prefix| path.starts_with(prefix))
            || self.extensions.iter().any(|extension| {
                path.rsplit_once('.').is_some_and(|(_, actual)| {
                    actual.eq_ignore_ascii_case(extension.trim_start_matches('.'))
                })
            })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_seconds: u64,
    #[serde(default = "default_shutdown_grace")]
    pub shutdown_grace_seconds: u64,
    #[serde(default = "default_activity_paths")]
    pub activity_paths: Vec<String>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: default_idle_timeout(),
            shutdown_grace_seconds: default_shutdown_grace(),
            activity_paths: default_activity_paths(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Readiness {
    Tcp,
    Http {
        #[serde(default = "default_health_path")]
        path: String,
        #[serde(default = "default_health_status")]
        status: u16,
    },
}

impl Default for Readiness {
    fn default() -> Self {
        Self::Tcp
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Network {
    #[default]
    Host,
    Namespace,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_request_body_limit")]
    pub request_body_bytes: usize,
    #[serde(default = "default_stdio_output_limit")]
    pub stdio_output_bytes: usize,
    #[serde(default = "default_concurrency")]
    pub max_concurrent_requests: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_body_bytes: default_request_body_limit(),
            stdio_output_bytes: default_stdio_output_limit(),
            max_concurrent_requests: default_concurrency(),
        }
    }
}

impl Manifest {
    pub async fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let manifest: Self = serde_json::from_slice(&contents)
            .with_context(|| format!("invalid manifest {}", path.display()))?;
        if manifest.version != 1 {
            bail!("unsupported manifest version {}", manifest.version);
        }
        validate_serve(&manifest.serve)?;
        for route in &manifest.routes {
            if route.path_prefix.is_none() && route.extensions.is_empty() {
                bail!("routes require path_prefix, extensions, or both");
            }
            if route
                .path_prefix
                .as_ref()
                .is_some_and(|path| !path.starts_with('/'))
            {
                bail!("route path_prefix must start with /");
            }
            validate_serve(&route.serve)?;
        }
        if manifest.prepare.iter().any(Vec::is_empty) {
            bail!("prepare commands cannot be empty");
        }
        if let Serve::Http { port: Some(0), .. } = &manifest.serve {
            bail!("serve.port must be between 1 and 65535");
        }
        if let Serve::Http { upstream_host, .. } = &manifest.serve {
            if upstream_host.trim().is_empty() {
                bail!("serve.upstream_host cannot be empty");
            }
        }
        if let Serve::Http {
            readiness: Readiness::Http { path, status },
            ..
        } = &manifest.serve
        {
            if !path.starts_with('/') || !(100..=599).contains(status) {
                bail!("HTTP readiness requires an absolute path and status between 100 and 599");
            }
        }
        if manifest.limits.request_body_bytes == 0
            || manifest.limits.stdio_output_bytes == 0
            || manifest.limits.max_concurrent_requests == 0
        {
            bail!("limit values must be greater than zero");
        }
        if manifest
            .lifecycle
            .activity_paths
            .iter()
            .any(|path| !path.starts_with('/'))
        {
            bail!("lifecycle.activity_paths entries must start with /");
        }
        Ok(manifest)
    }
}

fn validate_serve(serve: &Serve) -> anyhow::Result<()> {
    let command = match serve {
        Serve::Http { command, .. }
        | Serve::Stdio { command, .. }
        | Serve::Fastcgi { command, .. } => Some(command),
        Serve::Static { .. } => None,
    };
    if command.is_some_and(Vec::is_empty) {
        bail!("serve.command cannot be empty");
    }
    if let Serve::Fastcgi {
        document_root,
        front_controller,
        ..
    } = serve
    {
        if document_root.is_empty() || front_controller.is_empty() {
            bail!("FastCGI document_root and front_controller cannot be empty");
        }
    }
    Ok(())
}

const fn manifest_version() -> u32 {
    1
}
fn default_public() -> String {
    "public".into()
}
fn default_indexes() -> Vec<String> {
    vec!["index.html".into()]
}
fn default_port_environment() -> String {
    "PORT".into()
}
fn default_upstream_host() -> String {
    "127.0.0.1".into()
}
fn default_front_controller() -> String {
    "index.php".into()
}
const fn default_startup_timeout() -> u64 {
    15
}
const fn default_idle_timeout() -> u64 {
    60
}
const fn default_request_timeout() -> u64 {
    30
}
const fn default_shutdown_grace() -> u64 {
    10
}
fn default_activity_paths() -> Vec<String> {
    vec!["/".into()]
}
fn default_health_path() -> String {
    "/health".into()
}
const fn default_health_status() -> u16 {
    200
}
const fn default_request_body_limit() -> usize {
    64 * 1024 * 1024
}
const fn default_stdio_output_limit() -> usize {
    16 * 1024 * 1024
}
const fn default_concurrency() -> usize {
    64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extended_generic_http_policy() {
        let manifest: Manifest = serde_json::from_str(
            r#"{
                "version": 1,
                "serve": {
                    "mode": "http",
                    "command": ["./server"],
                    "port": 8080,
                    "readiness": {"mode": "http", "path": "/health", "status": 204},
                    "network": {"mode": "namespace"}
                },
                "lifecycle": {
                    "idle_timeout_seconds": 90,
                    "shutdown_grace_seconds": 5,
                    "activity_paths": ["/hls/"]
                },
                "limits": {
                    "request_body_bytes": 1024,
                    "stdio_output_bytes": 2048,
                    "max_concurrent_requests": 3
                }
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.lifecycle.activity_paths, ["/hls/"]);
        assert_eq!(manifest.limits.max_concurrent_requests, 3);
        assert!(matches!(
            manifest.serve,
            Serve::Http {
                readiness: Readiness::Http { status: 204, .. },
                network: Network::Namespace,
                ..
            }
        ));
    }

    #[test]
    fn route_matches_prefixes_and_extensions() {
        let route = Route {
            path_prefix: Some("/assets/".into()),
            extensions: vec!["css".into(), ".JS".into()],
            serve: Serve::Static {
                root: "public".into(),
                index: vec!["index.html".into()],
            },
        };
        assert!(route.matches("/assets/no-extension"));
        assert!(route.matches("/theme.CSS"));
        assert!(route.matches("/app.js"));
        assert!(!route.matches("/blog/post"));
    }
}
