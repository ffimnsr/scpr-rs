use crate::{
    github::{GithubClient, ReleaseAsset},
    plugin::Plugin,
};
use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::{fs, process};
use tracing::{info, warn};

pub(crate) async fn resolve_expected_sha256(
    plugin: &Plugin,
    client: &GithubClient,
    assets: &[ReleaseAsset],
    asset: &ReleaseAsset,
    tag: &str,
    target: &str,
) -> Result<Option<String>> {
    if let Some(digest) = asset.digest.as_deref() {
        return parse_sha256_digest(digest).map(Some);
    }

    let Some(checksum_pattern) = plugin.checksum_asset_pattern.as_deref() else {
        if plugin.allow_insecure_no_checksum {
            warn!(
                "Skipping SHA-256 verification for plugin '{}' because the plugin explicitly allows installs without published checksum metadata",
                plugin.name
            );
            return Ok(None);
        }
        return Err(anyhow!(
            "No SHA-256 metadata configured for plugin '{}'. If this upstream does not publish checksums, set `allow_insecure_no_checksum = true` in the plugin to opt in explicitly.",
            plugin.name
        ));
    };
    let checksum_name = plugin.expand_template(checksum_pattern, tag, target);
    let checksum_asset = assets
        .iter()
        .find(|candidate| candidate.name == checksum_name)
        .ok_or_else(|| {
            anyhow!(
                "Checksum asset '{checksum_name}' not found for plugin '{}'",
                plugin.name
            )
        })?;

    info!("Downloading checksum {}…", checksum_asset.name);
    let checksum_data = client
        .download_asset(&checksum_asset.browser_download_url, checksum_asset.size)
        .await?;
    let checksum_text = String::from_utf8(checksum_data)
        .context("Checksum asset is not valid UTF-8 text")?;
    parse_sha256_checksum_file(&checksum_text, &asset.name).map(Some)
}

pub(crate) async fn verify_signature_if_configured(
    plugin: &Plugin,
    client: &GithubClient,
    assets: &[ReleaseAsset],
    asset: &ReleaseAsset,
    data: &[u8],
    tag: &str,
    target: &str,
) -> Result<()> {
    let Some(signature_pattern) = plugin.signature_asset_pattern.as_deref() else {
        return Ok(());
    };
    let signature_format = plugin.signature_format_name().ok_or_else(|| {
        anyhow!(
            "Plugin '{}' configures a signature asset but no signature format",
            plugin.name
        )
    })?;

    let signature_name = plugin.expand_template(signature_pattern, tag, target);
    let signature_asset = assets
        .iter()
        .find(|candidate| candidate.name == signature_name)
        .ok_or_else(|| {
            anyhow!(
                "Signature asset '{signature_name}' not found for plugin '{}'",
                plugin.name
            )
        })?;

    info!("Downloading signature {}…", signature_asset.name);
    let signature_data = client
        .download_asset(&signature_asset.browser_download_url, signature_asset.size)
        .await?;
    verify_signature(
        signature_format,
        plugin.signature_key.as_deref(),
        &asset.name,
        data,
        &signature_asset.name,
        &signature_data,
    )
}

pub(crate) fn verify_sha256(data: &[u8], expected_sha256: &str) -> Result<()> {
    let actual = sha256_hex(data);
    if actual != expected_sha256 {
        return Err(anyhow!(
            "SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        ));
    }
    info!("Verified SHA-256: {actual}");
    Ok(())
}

pub(crate) fn parse_sha256_digest(digest: &str) -> Result<String> {
    let normalized = digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .trim()
        .to_ascii_lowercase();
    validate_sha256_hex(&normalized)?;
    Ok(normalized)
}

pub(crate) fn parse_sha256_checksum_file(
    contents: &str,
    asset_name: &str,
) -> Result<String> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(digest) = parse_bsd_checksum_line(line, asset_name)? {
            return Ok(digest);
        }

        if !line.contains(char::is_whitespace) {
            return parse_sha256_digest(line);
        }

        let mut parts = line.split_whitespace();
        let checksum = parts.next().unwrap_or_default();
        let filename = parts.next().unwrap_or_default().trim_start_matches('*');
        if filename == asset_name {
            return parse_sha256_digest(checksum);
        }
    }

    Err(anyhow!(
        "Checksum file does not contain an entry for asset '{asset_name}'"
    ))
}

fn parse_bsd_checksum_line(line: &str, asset_name: &str) -> Result<Option<String>> {
    let Some((left, right)) = line.split_once('=') else {
        return Ok(None);
    };
    let left = left.trim();
    let right = right.trim();
    let Some(inner) = left
        .strip_prefix("SHA256 (")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return Ok(None);
    };
    if inner == asset_name {
        return parse_sha256_digest(right).map(Some);
    }
    Ok(None)
}

fn verify_signature(
    signature_format: &str,
    signature_key: Option<&str>,
    asset_name: &str,
    asset_data: &[u8],
    signature_name: &str,
    signature_data: &[u8],
) -> Result<()> {
    let temp_dir = tempfile::tempdir()
        .context("Failed to create temp dir for signature verification")?;
    let asset_path = temp_dir.path().join(asset_name);
    let signature_path = temp_dir.path().join(signature_name);
    fs::write(&asset_path, asset_data).with_context(|| {
        format!(
            "Failed to stage asset for signature verification at {}",
            asset_path.display()
        )
    })?;
    fs::write(&signature_path, signature_data).with_context(|| {
        format!(
            "Failed to stage signature for verification at {}",
            signature_path.display()
        )
    })?;

    match signature_format {
        "gpg" => verify_gpg_signature(&asset_path, &signature_path),
        "minisign" => {
            verify_minisign_signature(signature_key, &asset_path, &signature_path)
        }
        other => Err(anyhow!(
            "Unsupported signature format '{other}'. Use 'gpg' or 'minisign'."
        )),
    }
}

fn verify_gpg_signature(asset_path: &Path, signature_path: &Path) -> Result<()> {
    let output = process::Command::new("gpg")
        .arg("--verify")
        .arg(signature_path)
        .arg(asset_path)
        .output()
        .context("Failed to execute gpg for signature verification")?;
    if !output.status.success() {
        return Err(anyhow!(
            "GPG signature verification failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    info!("Verified GPG signature");
    Ok(())
}

fn verify_minisign_signature(
    signature_key: Option<&str>,
    asset_path: &Path,
    signature_path: &Path,
) -> Result<()> {
    let public_key = signature_key.ok_or_else(|| {
        anyhow!(
            "minisign verification requires plugin.signature_key to contain the trusted public key"
        )
    })?;
    let output = process::Command::new("minisign")
        .arg("-V")
        .arg("-q")
        .arg("-P")
        .arg(public_key)
        .arg("-m")
        .arg(asset_path)
        .arg("-x")
        .arg(signature_path)
        .output()
        .context("Failed to execute minisign for signature verification")?;
    if !output.status.success() {
        return Err(anyhow!(
            "minisign verification failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    info!("Verified minisign signature");
    Ok(())
}

fn validate_sha256_hex(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("Invalid SHA-256 digest: {value}"));
    }
    Ok(())
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}
