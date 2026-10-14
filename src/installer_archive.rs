use crate::{
    installer_verify::sha256_hex,
    plugin::{FileRoot, ResolvedPluginFile, path_has_traversal},
};
use anyhow::{Context, Result, anyhow};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, Cursor},
    path::{Path, PathBuf},
    process,
};
use tar::Archive;
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

/// A file (or file from a directory tree) extracted from a release archive
/// that will be installed under one of the configured destination roots.
#[derive(Debug, Clone)]
pub(crate) struct PayloadFile {
    pub(crate) root: FileRoot,
    pub(crate) relative_path: PathBuf,
    pub(crate) contents: Vec<u8>,
    pub(crate) executable: bool,
}

/// Record of an additional installed file, persisted in the installer state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    /// Destination root key (`install_dir`, `data_home`, or `config_home`).
    pub root: String,
    /// Path relative to the destination root.
    pub path: String,
    /// SHA-256 checksum of the file contents as installed.
    pub checksum_sha256: String,
}

#[derive(Debug)]
pub(crate) struct InstallPayload {
    pub(crate) binary_filename: String,
    pub(crate) binary_contents: Vec<u8>,
    pub(crate) man_pages: Vec<(String, Vec<u8>)>,
    pub(crate) files: Vec<PayloadFile>,
}

#[derive(Debug)]
pub(crate) struct InstalledPaths {
    pub(crate) binary_filename: String,
    pub(crate) man_page_filenames: Vec<String>,
    pub(crate) files: Vec<InstalledFile>,
}

/// Destination roots used to resolve `files.to` placeholders.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InstallRoots<'a> {
    pub(crate) install_dir: &'a Path,
    pub(crate) man_dir: &'a Path,
    pub(crate) data_home: &'a Path,
    pub(crate) config_home: &'a Path,
}

impl InstallRoots<'_> {
    /// Resolve the filesystem path for a destination root.
    pub(crate) fn resolve(&self, root: FileRoot) -> &Path {
        match root {
            FileRoot::InstallDir => self.install_dir,
            FileRoot::DataHome => self.data_home,
            FileRoot::ConfigHome => self.config_home,
        }
    }
}

struct StagedFile {
    dest: PathBuf,
    temp: NamedTempFile,
    kind: StagedKind,
}

enum StagedKind {
    Binary,
    ManPage {
        filename: String,
    },
    File {
        root: FileRoot,
        relative_path: PathBuf,
        checksum_sha256: String,
    },
}

pub(crate) fn extract_install_payload(
    asset_name: &str,
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
    plugin_name: &str,
) -> Result<InstallPayload> {
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        extract_from_targz(data, binary_path, man_paths, files)
    } else if asset_name.ends_with(".tar.xz") || asset_name.ends_with(".txz") {
        extract_from_tar_xz(data, binary_path, man_paths, files)
    } else if asset_name.ends_with(".tar.zst") || asset_name.ends_with(".tar.zstd") {
        extract_from_tar_zst(data, binary_path, man_paths, files)
    } else if asset_name.ends_with(".tar.bz2") || asset_name.ends_with(".tbz2") {
        extract_from_tar_bz2(data, binary_path, man_paths, files)
    } else if asset_name.ends_with(".zip") {
        extract_from_zip(data, binary_path, man_paths, files)
    } else {
        require_no_additional_files(asset_name, files, plugin_name)?;
        if asset_name.ends_with(".gz") {
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
                files: Vec::new(),
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
                files: Vec::new(),
            })
        }
    }
}

/// Raw or single-file assets cannot address individual archive paths, so a
/// plugin that declares additional files cannot be installed from them.
fn require_no_additional_files(
    asset_name: &str,
    files: &[ResolvedPluginFile],
    plugin_name: &str,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "Plugin '{plugin_name}' declares additional files, but asset '{asset_name}' is not an archive that can address individual paths"
    ))
}

pub(crate) fn commit_install(
    roots: InstallRoots<'_>,
    payload: InstallPayload,
) -> Result<InstalledPaths> {
    fs::create_dir_all(roots.install_dir)
        .with_context(|| format!("Failed to create {}", roots.install_dir.display()))?;
    fs::create_dir_all(roots.man_dir)
        .with_context(|| format!("Failed to create {}", roots.man_dir.display()))?;

    let mut staged = Vec::new();
    let mut destinations = HashSet::new();
    let binary_dest = roots.install_dir.join(&payload.binary_filename);
    destinations.insert(binary_dest.clone());
    cleanup_orphaned_backups(std::slice::from_ref(&binary_dest));
    staged.push(StagedFile {
        dest: binary_dest.clone(),
        temp: stage_file(
            roots.install_dir,
            &binary_dest,
            &payload.binary_contents,
            true,
        )?,
        kind: StagedKind::Binary,
    });

    for (filename, contents) in payload.man_pages {
        let dest = man_destination(roots.man_dir, &filename);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        cleanup_orphaned_backups(std::slice::from_ref(&dest));
        let staging_dir = dest.parent().unwrap_or(roots.man_dir);
        destinations.insert(dest.clone());
        staged.push(StagedFile {
            dest: dest.clone(),
            temp: stage_file(staging_dir, &dest, &contents, false)?,
            kind: StagedKind::ManPage { filename },
        });
    }

    for file in payload.files {
        let root_path = roots.resolve(file.root);
        let dest = root_path.join(&file.relative_path);
        ensure_unique_destination(&mut destinations, &dest)?;
        let staging_dir = dest.parent().unwrap_or(root_path);
        fs::create_dir_all(staging_dir)
            .with_context(|| format!("Failed to create {}", staging_dir.display()))?;
        cleanup_orphaned_backups(std::slice::from_ref(&dest));
        let checksum_sha256 = sha256_hex(&file.contents);
        staged.push(StagedFile {
            dest: dest.clone(),
            temp: stage_file(staging_dir, &dest, &file.contents, file.executable)?,
            kind: StagedKind::File {
                root: file.root,
                relative_path: file.relative_path,
                checksum_sha256,
            },
        });
    }

    let all_destinations: Vec<PathBuf> =
        staged.iter().map(|file| file.dest.clone()).collect();

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
    let mut man_page_filenames = Vec::new();
    let mut installed_files = Vec::new();
    for staged_file in staged {
        match staged_file.temp.persist(&staged_file.dest) {
            Ok(_) => {
                committed_paths.push(staged_file.dest.clone());
                match staged_file.kind {
                    StagedKind::Binary => {}
                    StagedKind::ManPage { filename } => {
                        info!("Installed man page → {}", staged_file.dest.display());
                        man_page_filenames.push(filename);
                    }
                    StagedKind::File {
                        root,
                        relative_path,
                        checksum_sha256,
                    } => {
                        debug!(
                            "Installed additional file → {}",
                            staged_file.dest.display()
                        );
                        installed_files.push(InstalledFile {
                            root: root.key().to_string(),
                            path: stored_relative_path(&relative_path),
                            checksum_sha256,
                        });
                    }
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
    cleanup_orphaned_backups(&all_destinations);

    Ok(InstalledPaths {
        binary_filename: payload.binary_filename,
        man_page_filenames,
        files: installed_files,
    })
}

fn extract_from_targz(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
) -> Result<InstallPayload> {
    let gz = GzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(gz);
    extract_from_tar(archive.entries()?, binary_path, man_paths, files)
}

fn extract_from_tar_xz(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
) -> Result<InstallPayload> {
    let xz = XzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(xz);
    extract_from_tar(archive.entries()?, binary_path, man_paths, files)
}

fn extract_from_tar_zst(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
) -> Result<InstallPayload> {
    let zst =
        ZstdDecoder::new(Cursor::new(data)).context("Failed to init zstd decoder")?;
    let mut archive = Archive::new(zst);
    extract_from_tar(archive.entries()?, binary_path, man_paths, files)
}

fn extract_from_tar_bz2(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
) -> Result<InstallPayload> {
    let bz = BzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(bz);
    extract_from_tar(archive.entries()?, binary_path, man_paths, files)
}

fn extract_from_tar<R: io::Read>(
    entries: tar::Entries<'_, R>,
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
) -> Result<InstallPayload> {
    let mut binary_filename = None;
    let mut binary_contents = None;
    let mut man_pages_left: HashSet<&str> =
        man_paths.iter().map(String::as_str).collect();
    let mut extracted_man_pages = Vec::new();
    let mut files_seen = vec![false; files.len()];
    let mut extracted_files = Vec::new();

    for entry in entries {
        let mut entry = entry.context("Failed to read tar entry")?;
        let entry_type = entry.header().entry_type();
        let path = entry.path().context("Failed to get tar entry path")?;

        if path_has_traversal(&path) {
            warn!("Skipping unsafe tar entry: {}", path.to_string_lossy());
            continue;
        }
        if !entry_type.is_file() {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        let is_binary = binary_contents.is_none() && path_str == binary_path;
        let man_filename = if man_pages_left.contains(path_str.as_str()) {
            Some(man_install_relpath(&path_str)?)
        } else {
            None
        };
        let file_destinations = matched_file_destinations(files, &path_str);
        if !is_binary && man_filename.is_none() && file_destinations.is_empty() {
            continue;
        }

        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut entry, &mut bytes)
            .context("Failed to read file from archive")?;
        let archive_mode = entry.header().mode().ok().map(|mode| mode & 0o777);

        if is_binary {
            debug!("Extracting binary: {path_str}");
            binary_filename = Some(host_executable_name(
                Path::new(&path_str)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("Invalid binary path: {path_str}"))?,
            ));
            binary_contents = Some(bytes.clone());
        }
        if let Some(man_filename) = man_filename {
            extracted_man_pages.push((man_filename, bytes.clone()));
            man_pages_left.remove(path_str.as_str());
        }
        for (index, relative_path) in file_destinations {
            files_seen[index] = true;
            extracted_files.push(PayloadFile {
                root: files[index].root,
                relative_path,
                contents: bytes.clone(),
                executable: payload_file_executable(&files[index], archive_mode),
            });
        }
    }

    if binary_contents.is_none() {
        return Err(anyhow!("Binary '{binary_path}' not found in archive"));
    }

    for missing in &man_pages_left {
        warn!("Man page '{missing}' not found in archive, skipping");
    }
    ensure_files_found(files, &files_seen)?;

    Ok(InstallPayload {
        binary_filename: binary_filename.expect("binary filename set"),
        binary_contents: binary_contents.expect("binary contents set"),
        man_pages: extracted_man_pages,
        files: extracted_files,
    })
}

fn extract_from_zip(
    data: &[u8],
    binary_path: &str,
    man_paths: &[String],
    files: &[ResolvedPluginFile],
) -> Result<InstallPayload> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(data)).context("Failed to open zip archive")?;

    let mut binary_filename = None;
    let mut binary_contents = None;
    let mut man_pages_left: HashSet<&str> =
        man_paths.iter().map(String::as_str).collect();
    let mut extracted_man_pages = Vec::new();
    let mut files_seen = vec![false; files.len()];
    let mut extracted_files = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("Failed to read zip entry")?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if path_has_traversal(Path::new(&name)) {
            warn!("Skipping unsafe zip entry: {name}");
            continue;
        }

        let is_binary = binary_contents.is_none() && name == binary_path;
        let man_filename = if man_pages_left.contains(name.as_str()) {
            Some(man_install_relpath(&name)?)
        } else {
            None
        };
        let file_destinations = matched_file_destinations(files, &name);
        if !is_binary && man_filename.is_none() && file_destinations.is_empty() {
            continue;
        }

        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut entry, &mut bytes)
            .context("Failed to read file from zip archive")?;
        let archive_mode = entry.unix_mode().map(|mode| mode & 0o777);

        if is_binary {
            debug!("Extracting binary: {name}");
            binary_filename = Some(host_executable_name(
                Path::new(&name)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("Invalid binary path: {name}"))?,
            ));
            binary_contents = Some(bytes.clone());
        }
        if let Some(man_filename) = man_filename {
            extracted_man_pages.push((man_filename, bytes.clone()));
            man_pages_left.remove(name.as_str());
        }
        for (mapping_index, relative_path) in file_destinations {
            files_seen[mapping_index] = true;
            extracted_files.push(PayloadFile {
                root: files[mapping_index].root,
                relative_path,
                contents: bytes.clone(),
                executable: payload_file_executable(&files[mapping_index], archive_mode),
            });
        }
    }

    if binary_contents.is_none() {
        return Err(anyhow!("Binary '{binary_path}' not found in zip archive"));
    }

    for missing in &man_pages_left {
        warn!("Man page '{missing}' not found in archive, skipping");
    }
    ensure_files_found(files, &files_seen)?;

    Ok(InstallPayload {
        binary_filename: binary_filename.expect("binary filename set"),
        binary_contents: binary_contents.expect("binary contents set"),
        man_pages: extracted_man_pages,
        files: extracted_files,
    })
}

/// Destination paths a single archive entry maps to, one per matching `files`
/// declaration. Nested declarations can both match, so a whole tree and a
/// subtree can be installed to different roots from one archive.
fn matched_file_destinations(
    files: &[ResolvedPluginFile],
    entry_path: &str,
) -> Vec<(usize, PathBuf)> {
    let mut matches = Vec::new();
    for (index, mapping) in files.iter().enumerate() {
        if entry_path == mapping.from {
            matches.push((index, mapping.relative_path.clone()));
            continue;
        }
        let prefix = format!("{}/", mapping.from);
        if let Some(rest) = entry_path.strip_prefix(&prefix)
            && !rest.is_empty()
        {
            matches.push((index, mapping.relative_path.join(rest)));
        }
    }
    matches
}

fn payload_file_executable(
    mapping: &ResolvedPluginFile,
    archive_mode: Option<u32>,
) -> bool {
    mapping.executable || archive_mode.is_some_and(|mode| mode & 0o111 != 0)
}

fn ensure_files_found(files: &[ResolvedPluginFile], files_seen: &[bool]) -> Result<()> {
    for (index, mapping) in files.iter().enumerate() {
        if !files_seen.get(index).copied().unwrap_or(false) {
            return Err(anyhow!(
                "Additional file '{}' was not found in the release archive",
                mapping.from
            ));
        }
    }
    Ok(())
}

/// Reject a destination that is already claimed by another installed file so a
/// plugin cannot silently clobber the binary, a man page, or an earlier entry.
fn ensure_unique_destination(
    destinations: &mut HashSet<PathBuf>,
    dest: &Path,
) -> Result<()> {
    if !destinations.insert(dest.to_path_buf()) {
        return Err(anyhow!(
            "Plugin installs more than one file to '{}'",
            dest.display()
        ));
    }
    Ok(())
}

/// Render a destination path for the state file. On Windows the separator is
/// normalized so records stay readable and portable across platforms.
fn stored_relative_path(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw
    }
}

fn stage_file(
    dest_dir: &Path,
    dest: &Path,
    contents: &[u8],
    executable: bool,
) -> Result<NamedTempFile> {
    let mut temp = NamedTempFile::new_in(dest_dir).with_context(|| {
        format!("Failed to create temp file in {}", dest_dir.display())
    })?;
    io::Write::write_all(&mut temp, contents)
        .with_context(|| format!("Failed to write staged file for {}", dest.display()))?;

    // NamedTempFile creates files with mode 0600; always set an explicit mode so
    // installed files keep working permissions instead of inheriting that.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        let mut perms = temp
            .as_file()
            .metadata()
            .with_context(|| {
                format!("Failed to stat staged file for {}", dest.display())
            })?
            .permissions();
        perms.set_mode(mode);
        temp.as_file().set_permissions(perms).with_context(|| {
            format!("Failed to set permissions on {}", dest.display())
        })?;
    }

    Ok(temp)
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

#[cfg(test)]
#[path = "installer_archive_tests.rs"]
mod tests;
