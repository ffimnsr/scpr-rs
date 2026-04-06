use crate::{
    github::{GithubClient, ReleaseAsset},
    plugin::Plugin,
};
use anyhow::{Context, Result, anyhow};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{self, Cursor},
    path::{Component, Path, PathBuf},
    process,
};
use tar::Archive;
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Debug)]
pub(crate) struct InstallPayload {
    pub(crate) binary_filename: String,
    pub(crate) binary_contents: Vec<u8>,
    pub(crate) man_pages: Vec<(String, Vec<u8>)>,
}

#[derive(Debug)]
pub(crate) struct InstalledPaths {
    pub(crate) binary_filename: String,
    pub(crate) man_page_filenames: Vec<String>,
}

struct StagedFile {
    dest: PathBuf,
    temp: NamedTempFile,
    is_man_page: bool,
}

pub(crate) fn extract_install_payload(
    asset_name: &str,
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    plugin_name: &str,
) -> Result<InstallPayload> {
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        extract_from_targz(data, binary_path, man_paths)
    } else if asset_name.ends_with(".tar.xz") || asset_name.ends_with(".txz") {
        extract_from_tar_xz(data, binary_path, man_paths)
    } else if asset_name.ends_with(".tar.zst") || asset_name.ends_with(".tar.zstd") {
        extract_from_tar_zst(data, binary_path, man_paths)
    } else if asset_name.ends_with(".tar.bz2") || asset_name.ends_with(".tbz2") {
        extract_from_tar_bz2(data, binary_path, man_paths)
    } else if asset_name.ends_with(".zip") {
        extract_from_zip(data, binary_path, man_paths)
    } else if asset_name.ends_with(".gz") {
        let mut decoder = GzDecoder::new(Cursor::new(data));
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut decoder, &mut bytes)
            .context("Failed to decompress .gz binary")?;
        Ok(InstallPayload {
            binary_filename: host_executable_name(
                asset_name
                    .strip_suffix(".gz")
                    .and_then(|s| Path::new(s).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or(plugin_name),
            ),
            binary_contents: bytes,
            man_pages: Vec::new(),
        })
    } else {
        Ok(InstallPayload {
            binary_filename: host_executable_name(
                Path::new(binary_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(plugin_name),
            ),
            binary_contents: data.to_vec(),
            man_pages: Vec::new(),
        })
    }
}

pub(crate) async fn resolve_expected_sha256(
    plugin: &Plugin,
    client: &GithubClient,
    assets: &[ReleaseAsset],
    asset: &ReleaseAsset,
    tag: &str,
    target: &str,
) -> Result<String> {
    if let Some(digest) = asset.digest.as_deref() {
        return parse_sha256_digest(digest);
    }

    let checksum_pattern = plugin.checksum_asset_pattern.as_deref().ok_or_else(|| {
        anyhow!(
            "No SHA-256 metadata configured for plugin '{}'",
            plugin.name
        )
    })?;
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
    parse_sha256_checksum_file(&checksum_text, &asset.name)
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

pub(crate) fn commit_install(
    local_bin: &Path,
    local_man: &Path,
    payload: InstallPayload,
) -> Result<InstalledPaths> {
    fs::create_dir_all(local_bin)
        .with_context(|| format!("Failed to create {}", local_bin.display()))?;
    fs::create_dir_all(local_man)
        .with_context(|| format!("Failed to create {}", local_man.display()))?;

    let mut staged = Vec::new();
    let binary_dest = local_bin.join(&payload.binary_filename);
    cleanup_orphaned_backups(std::slice::from_ref(&binary_dest));
    staged.push(stage_file(
        local_bin,
        &binary_dest,
        &payload.binary_contents,
        true,
    )?);

    let mut man_page_filenames = Vec::new();
    for (filename, contents) in payload.man_pages {
        let dest = man_destination(local_man, &filename);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        cleanup_orphaned_backups(std::slice::from_ref(&dest));
        let staging_dir = dest.parent().unwrap_or(local_man);
        staged.push(stage_file(staging_dir, &dest, &contents, false)?);
        man_page_filenames.push(filename);
    }

    let mut backups = Vec::new();
    for staged_file in &staged {
        if staged_file.dest.exists() {
            let backup = unique_backup_path(&staged_file.dest);
            fs::rename(&staged_file.dest, &backup).with_context(|| {
                format!(
                    "Failed to move {} to backup location {}",
                    staged_file.dest.display(),
                    backup.display()
                )
            })?;
            backups.push((staged_file.dest.clone(), backup));
        }
    }

    let mut committed_paths = Vec::new();
    for staged_file in staged {
        match staged_file.temp.persist(&staged_file.dest) {
            Ok(_) => {
                committed_paths.push(staged_file.dest.clone());
                if staged_file.is_man_page {
                    info!("Installed man page → {}", staged_file.dest.display());
                }
            }
            Err(err) => {
                restore_backups(&backups)?;
                cleanup_paths(&committed_paths);
                cleanup_orphaned_backups(
                    &backups
                        .iter()
                        .map(|(dest, _)| dest.clone())
                        .collect::<Vec<_>>(),
                );
                return Err(anyhow!(
                    "Failed to replace {}: {}",
                    staged_file.dest.display(),
                    err.error
                ));
            }
        }
    }

    cleanup_backup_files(backups);
    cleanup_orphaned_backups(
        &std::iter::once(binary_dest)
            .chain(man_page_filenames.iter().map(|name| local_man.join(name)))
            .collect::<Vec<_>>(),
    );

    Ok(InstalledPaths {
        binary_filename: payload.binary_filename,
        man_page_filenames,
    })
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

fn extract_from_targz(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
) -> Result<InstallPayload> {
    let gz = GzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(gz);
    extract_from_tar(archive.entries()?, binary_path, man_paths)
}

fn extract_from_tar_xz(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
) -> Result<InstallPayload> {
    let xz = XzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(xz);
    extract_from_tar(archive.entries()?, binary_path, man_paths)
}

fn extract_from_tar_zst(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
) -> Result<InstallPayload> {
    let zst =
        ZstdDecoder::new(Cursor::new(data)).context("Failed to init zstd decoder")?;
    let mut archive = Archive::new(zst);
    extract_from_tar(archive.entries()?, binary_path, man_paths)
}

fn extract_from_tar_bz2(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
) -> Result<InstallPayload> {
    let bz = BzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(bz);
    extract_from_tar(archive.entries()?, binary_path, man_paths)
}

fn extract_from_tar<R: io::Read>(
    entries: tar::Entries<'_, R>,
    binary_path: &str,
    man_paths: &[String],
) -> Result<InstallPayload> {
    let mut binary_filename = None;
    let mut binary_contents = None;
    let mut man_pages_left: HashSet<&str> =
        man_paths.iter().map(String::as_str).collect();
    let mut extracted_man_pages = Vec::new();

    for entry in entries {
        let mut entry = entry.context("Failed to read tar entry")?;
        let path = entry.path().context("Failed to get tar entry path")?;
        let path_str = path.to_string_lossy().to_string();

        if has_path_traversal(&path) {
            warn!("Skipping unsafe tar entry: {path_str}");
            continue;
        }

        if binary_contents.is_none() && path_str == binary_path {
            debug!("Extracting binary: {path_str}");
            let filename = host_executable_name(
                Path::new(&path_str)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("Invalid binary path: {path_str}"))?,
            );
            let mut bytes = Vec::new();
            io::Read::read_to_end(&mut entry, &mut bytes)
                .context("Failed to read binary from archive")?;
            binary_filename = Some(filename);
            binary_contents = Some(bytes);
        } else if man_pages_left.contains(path_str.as_str()) {
            let man_filename = man_install_relpath(&path_str)?;
            let mut bytes = Vec::new();
            io::Read::read_to_end(&mut entry, &mut bytes)
                .context("Failed to read man page from archive")?;
            extracted_man_pages.push((man_filename, bytes));
            man_pages_left.remove(path_str.as_str());
        }

        if binary_contents.is_some() && man_pages_left.is_empty() {
            break;
        }
    }

    if binary_contents.is_none() {
        return Err(anyhow!("Binary '{binary_path}' not found in archive"));
    }

    for missing in &man_pages_left {
        warn!("Man page '{missing}' not found in archive, skipping");
    }

    Ok(InstallPayload {
        binary_filename: binary_filename.expect("binary filename set"),
        binary_contents: binary_contents.expect("binary contents set"),
        man_pages: extracted_man_pages,
    })
}

fn extract_from_zip(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
) -> Result<InstallPayload> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(data)).context("Failed to open zip archive")?;

    let binary_filename;
    let binary_contents;

    {
        let mut entry = archive.by_name(binary_path).with_context(|| {
            format!("Binary '{binary_path}' not found in zip archive")
        })?;
        binary_filename = host_executable_name(
            Path::new(binary_path)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("Invalid binary path: {binary_path}"))?,
        );
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut entry, &mut bytes)
            .context("Failed to read binary from zip archive")?;
        binary_contents = bytes;
    }

    let mut extracted_man_pages = Vec::new();
    for man_path in man_paths {
        match archive.by_name(man_path) {
            Ok(mut entry) => {
                let man_filename = man_install_relpath(man_path)?;
                let mut bytes = Vec::new();
                io::Read::read_to_end(&mut entry, &mut bytes)
                    .context("Failed to read man page from zip archive")?;
                extracted_man_pages.push((man_filename, bytes));
            }
            Err(_) => warn!("Man page '{man_path}' not found in archive, skipping"),
        }
    }

    Ok(InstallPayload {
        binary_filename,
        binary_contents,
        man_pages: extracted_man_pages,
    })
}

fn has_path_traversal(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir
        )
    })
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

fn stage_file(
    dest_dir: &Path,
    dest: &Path,
    contents: &[u8],
    executable: bool,
) -> Result<StagedFile> {
    let mut temp = NamedTempFile::new_in(dest_dir).with_context(|| {
        format!("Failed to create temp file in {}", dest_dir.display())
    })?;
    io::Write::write_all(&mut temp, contents)
        .with_context(|| format!("Failed to write staged file for {}", dest.display()))?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = temp
            .as_file()
            .metadata()
            .with_context(|| {
                format!("Failed to stat staged file for {}", dest.display())
            })?
            .permissions();
        perms.set_mode(0o755);
        temp.as_file().set_permissions(perms).with_context(|| {
            format!("Failed to set permissions on {}", dest.display())
        })?;
    }

    Ok(StagedFile {
        dest: dest.to_path_buf(),
        temp,
        is_man_page: !executable,
    })
}

fn host_executable_name(filename: &str) -> String {
    #[cfg(windows)]
    {
        if filename.ends_with(".exe") {
            filename.to_string()
        } else {
            format!("{filename}.exe")
        }
    }
    #[cfg(not(windows))]
    {
        filename.to_string()
    }
}

fn unique_backup_path(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scpr-backup");

    for counter in 0..1000 {
        let candidate =
            parent.join(format!("{filename}.scpr-old.{}.{}", process::id(), counter));
        if !candidate.exists() {
            return candidate;
        }
    }

    parent.join(format!("{filename}.scpr-old.{}", process::id()))
}

fn cleanup_backup_files(backups: Vec<(PathBuf, PathBuf)>) {
    for (_, backup) in backups {
        if backup.exists() {
            let _ = fs::remove_file(backup);
        }
    }
}

fn restore_backups(backups: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (dest, backup) in backups.iter().rev() {
        if backup.exists() {
            fs::rename(backup, dest).with_context(|| {
                format!(
                    "Failed to restore backup from {} to {}",
                    backup.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}

fn cleanup_paths(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn man_install_relpath(path: &str) -> Result<String> {
    let path = Path::new(path);
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("Invalid man page path: {}", path.display()))?;
    for ancestor in path.ancestors() {
        let Some(component) = ancestor.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if component.len() >= 4
            && component.starts_with("man")
            && component[3..].chars().all(|ch| ch.is_ascii_digit())
        {
            return Ok(format!("{component}/{filename}"));
        }
    }
    Ok(filename.to_string())
}

fn man_destination(local_man: &Path, relative: &str) -> PathBuf {
    if relative.contains('/')
        && local_man
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("man") && name[3..].chars().all(|ch| ch.is_ascii_digit())
            })
        && let Some(parent) = local_man.parent()
    {
        return parent.join(relative);
    }
    local_man.join(relative)
}

fn cleanup_orphaned_backups(paths: &[PathBuf]) {
    for path in paths {
        let Some(parent) = path.parent() else {
            continue;
        };
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let prefix = format!("{filename}.scpr-old.");
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let matches = entry_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with(&prefix))
                .unwrap_or(false);
            if matches {
                let _ = fs::remove_file(entry_path);
            }
        }
    }
}
