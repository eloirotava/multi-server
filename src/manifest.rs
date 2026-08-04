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
    pub prepare: Vec<Vec<String>>,
    #[serde(default)]
    pub lifecycle: Lifecycle,
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
        #[serde(default = "default_startup_timeout")]
        startup_timeout_seconds: u64,
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
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_seconds: u64,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: default_idle_timeout(),
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
        let command = match &manifest.serve {
            Serve::Http { command, .. } | Serve::Stdio { command, .. } => Some(command),
            Serve::Static { .. } => None,
        };
        if command.is_some_and(Vec::is_empty) {
            bail!("serve.command cannot be empty");
        }
        if manifest.prepare.iter().any(Vec::is_empty) {
            bail!("prepare commands cannot be empty");
        }
        Ok(manifest)
    }
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
const fn default_startup_timeout() -> u64 {
    15
}
const fn default_idle_timeout() -> u64 {
    60
}
const fn default_request_timeout() -> u64 {
    30
}
