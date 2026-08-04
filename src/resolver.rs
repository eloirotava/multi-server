use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::manifest::Manifest;

#[derive(Clone, Debug)]
pub struct ResolvedSite {
    pub domain: String,
    pub directory: PathBuf,
    pub manifest: Manifest,
}

pub async fn resolve(root: &Path, host: &str) -> anyhow::Result<Option<ResolvedSite>> {
    let domain = normalize_host(host)?;
    let exact = root.join(&domain);
    if let Some(site) = load_site(&domain, exact, false).await? {
        return Ok(Some(site));
    }

    let labels: Vec<_> = domain.split('.').collect();
    for offset in 1..labels.len().saturating_sub(1) {
        let parent = labels[offset..].join(".");
        if let Some(site) = load_site(&domain, root.join(parent), true).await? {
            return Ok(Some(site));
        }
    }
    Ok(None)
}

async fn load_site(
    domain: &str,
    directory: PathBuf,
    require_subdomains: bool,
) -> anyhow::Result<Option<ResolvedSite>> {
    let path = directory.join("site.json");
    if !tokio::fs::try_exists(&path)
        .await
        .context("failed to inspect site directory")?
    {
        return Ok(None);
    }
    let manifest = Manifest::load(&path).await?;
    if require_subdomains && !manifest.domains.subdomains {
        return Ok(None);
    }
    Ok(Some(ResolvedSite {
        domain: domain.into(),
        directory,
        manifest,
    }))
}

fn normalize_host(host: &str) -> anyhow::Result<String> {
    let host = host
        .trim()
        .trim_end_matches('.')
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        || host
            .split('.')
            .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
    {
        bail!("invalid Host header");
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_normalization_rejects_paths() {
        assert!(normalize_host("example.com/../../etc").is_err());
        assert_eq!(
            normalize_host("ELOI.ROTAVA.COM:8080").unwrap(),
            "eloi.rotava.com"
        );
    }
}
