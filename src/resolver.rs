use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::manifest::Manifest;

#[derive(Clone, Debug)]
pub struct ResolvedSite {
    pub domain: String,
    pub manifest_directory: PathBuf,
    pub directory: PathBuf,
    pub runtime_key: PathBuf,
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
    if let Some(site) = resolve_alias(root, &domain).await? {
        return Ok(Some(site));
    }
    Ok(None)
}

async fn resolve_alias(root: &Path, domain: &str) -> anyhow::Result<Option<ResolvedSite>> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("site.json");
        if !tokio::fs::try_exists(&manifest_path).await? {
            continue;
        }
        let Ok(manifest) = Manifest::load(&manifest_path).await else {
            continue;
        };
        if manifest
            .domains
            .aliases
            .iter()
            .any(|alias| alias.trim_end_matches('.').eq_ignore_ascii_case(domain))
        {
            return Ok(Some(resolved_site(domain, entry.path(), manifest)));
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
    Ok(Some(resolved_site(domain, directory, manifest)))
}

fn resolved_site(domain: &str, manifest_directory: PathBuf, manifest: Manifest) -> ResolvedSite {
    let configured_root = Path::new(&manifest.site_root);
    let directory = if configured_root.is_absolute() {
        configured_root.to_path_buf()
    } else {
        manifest_directory.join(configured_root)
    };
    ResolvedSite {
        domain: domain.into(),
        runtime_key: manifest_directory.clone(),
        manifest_directory,
        directory,
        manifest,
    }
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

    #[tokio::test]
    async fn resolves_aliases_and_external_site_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest_directory = temporary.path().join("canonical.test");
        let shared = temporary.path().join("shared");
        tokio::fs::create_dir_all(&manifest_directory)
            .await
            .unwrap();
        tokio::fs::create_dir_all(&shared).await.unwrap();
        tokio::fs::write(
            manifest_directory.join("site.json"),
            r#"{
                "version": 1,
                "site_root": "../shared",
                "domains": {"aliases": ["alias.test"]},
                "serve": {"mode": "static", "root": "public"}
            }"#,
        )
        .await
        .unwrap();

        let site = resolve(temporary.path(), "alias.test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(site.manifest_directory, manifest_directory);
        assert_eq!(
            site.directory,
            temporary.path().join("canonical.test/../shared")
        );
        assert_eq!(site.domain, "alias.test");
    }
}
