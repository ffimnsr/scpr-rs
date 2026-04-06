use crate::{
    github::{GithubClient, ReleaseAsset},
    plugin::Plugin,
    settings::AppSettings,
};
use anyhow::{Context, Result, anyhow};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    io::{self, Cursor},
    ops::Drop,
    path::{Component, Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

const LOCK_RETRY_DELAY_MS: u64 = 100;
const LOCK_RETRY_ATTEMPTS: usize = 100;
const LOCK_STALE_AFTER_SECS: u64 = 60;
const STATE_VERSION: u32 = 1;

/// Record of a single installed package, persisted in `~/.local/share/scpr/state.toml`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    /// Filename of the installed binary (just the name, not the full path).
    pub binary: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub asset_name: Option<String>,
    #[serde(default)]
    pub checksum_sha256: Option<String>,
    #[serde(default)]
    pub man_pages: Vec<String>,
    #[serde(default)]
    pub installed_at_unix: Option<u64>,
    /// When `true`, `update --all` will not upgrade this package.
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default = "default_state_version")]
    version: u32,
    #[serde(default)]
    installed: Vec<InstalledPackage>,
    #[serde(default)]
    history: Vec<HistoryEvent>,
}

#[derive(Debug, Clone, Copy)]
pub enum StateFormat {
    Json,
    Toml,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAction {
    Installed,
    Updated,
    Removed,
    Pinned,
    Unpinned,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HistoryEvent {
    pub package: String,
    pub action: HistoryAction,
    pub timestamp_unix: u64,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub from_version: Option<String>,
    #[serde(default)]
    pub to_version: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Ok,
    Modified,
    Missing,
    Untracked,
}

#[derive(Debug, Serialize, Clone)]
pub struct AuditRecord {
    pub package: String,
    pub binary_path: PathBuf,
    pub status: AuditStatus,
    pub expected_checksum: Option<String>,
    pub actual_checksum: Option<String>,
    pub detail: String,
}

#[derive(Debug)]
struct InstallPayload {
    binary_filename: String,
    binary_contents: Vec<u8>,
    man_pages: Vec<(String, Vec<u8>)>,
}

#[derive(Debug)]
struct InstalledPaths {
    binary_filename: String,
    man_page_filenames: Vec<String>,
}

struct StagedFile {
    dest: PathBuf,
    temp: NamedTempFile,
    is_man_page: bool,
}

#[derive(Debug)]
struct StateLock {
    path: PathBuf,
}

/// Installs and uninstalls GitHub-release binaries into the user's local
/// directories (`~/.local/bin`, `~/.local/share/man`).
#[derive(Clone)]
pub struct Installer {
    /// `~/.local/bin`
    local_bin: PathBuf,
    /// `~/.local/share/man/man1`
    local_man: PathBuf,
    /// `~/.local/share/scpr/state.toml`
    state_file: PathBuf,
}

impl Installer {
    /// Create a new [`Installer`], ensuring all required directories exist.
    pub fn new() -> Result<Self> {
        let settings = AppSettings::load()?;
        Self::from_settings(&settings)
    }

    pub fn from_settings(settings: &AppSettings) -> Result<Self> {
        let local_bin = settings.install_dir().to_path_buf();
        let local_man = settings.man_dir().to_path_buf();
        let state_dir = settings.data_dir().to_path_buf();
        let state_file = state_dir.join("state.toml");

        fs::create_dir_all(&local_bin)
            .with_context(|| format!("Failed to create {}", local_bin.display()))?;
        fs::create_dir_all(&local_man)
            .with_context(|| format!("Failed to create {}", local_man.display()))?;
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("Failed to create {}", state_dir.display()))?;

        Ok(Self {
            local_bin,
            local_man,
            state_file,
        })
    }

    fn load_state(&self) -> Result<State> {
        if !self.state_file.exists() {
            return Ok(State {
                version: STATE_VERSION,
                ..State::default()
            });
        }

        let content =
            fs::read_to_string(&self.state_file).context("Failed to read state file")?;
        let state: State =
            toml::from_str(&content).context("Failed to parse state file")?;
        if state.version != STATE_VERSION {
            return Err(anyhow!(
                "Unsupported state file version {} in {}. Expected version {}.",
                state.version,
                self.state_file.display(),
                STATE_VERSION
            ));
        }
        Ok(state)
    }

    fn save_state(&self, state: &State) -> Result<()> {
        let state = State {
            version: STATE_VERSION,
            installed: state.installed.clone(),
            history: state.history.clone(),
        };
        let content = toml::to_string(&state).context("Failed to serialize state")?;
        let state_dir = self
            .state_file
            .parent()
            .context("State file has no parent directory")?;
        let mut temp = NamedTempFile::new_in(state_dir).with_context(|| {
            format!("Failed to create temp file in {}", state_dir.display())
        })?;
        io::Write::write_all(&mut temp, content.as_bytes())
            .context("Failed to write staged state file")?;
        temp.persist(&self.state_file).map_err(|err| {
            anyhow!(
                "Failed to replace state file {}: {}",
                self.state_file.display(),
                err.error
            )
        })?;
        Ok(())
    }

    /// Return all currently installed packages.
    pub fn list_installed(&self) -> Result<Vec<InstalledPackage>> {
        Ok(self.load_state()?.installed)
    }

    pub fn local_bin_dir(&self) -> &Path {
        &self.local_bin
    }

    pub fn local_man_dir(&self) -> &Path {
        &self.local_man
    }

    pub fn state_file_path(&self) -> &Path {
        &self.state_file
    }

    /// Download and install a release of `plugin` from GitHub.
    ///
    /// If `tag` is `None`, the latest release is installed.
    /// When `dry_run` is `true`, all resolution steps are performed and logged
    /// but nothing is written to disk and nothing is added to the state file.
    pub async fn install(
        &self,
        plugin: &Plugin,
        client: &GithubClient,
        tag: Option<&str>,
        target_override: Option<&str>,
        dry_run: bool,
    ) -> Result<()> {
        let (owner, repo) = plugin.github_repo().ok_or_else(|| {
            anyhow!(
                "Plugin '{}' has an invalid location '{}'; expected 'github:<owner>/<repo>'",
                plugin.name,
                plugin.location
            )
        })?;

        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        info!("Installing {} for {os}/{arch}…", plugin.name);

        let target = match target_override {
            Some(target) => target.to_string(),
            None => plugin.resolve_target(os, arch).ok_or_else(|| {
                let available = plugin.available_target_keys();
                if available.is_empty() {
                    anyhow!(
                        "No target triple defined for {os}/{arch} in plugin '{}'. This plugin has no [plugin.targets] entries. Use --target <triple> to override manually.",
                        plugin.name
                    )
                } else {
                    anyhow!(
                        "No target triple defined for {os}/{arch} in plugin '{}'. Available target keys: {}. Use --target <triple> to override manually.",
                        plugin.name,
                        available.join(", ")
                    )
                }
            })?,
        };
        debug!("Resolved target: {target}");

        let release = match tag {
            Some(tag) => client.get_release_by_tag(owner, repo, tag).await?,
            None => client.get_latest_release(owner, repo).await?,
        };
        let tag = &release.tag_name;
        info!("Using release: {tag}");

        let asset_name = plugin.expand_template(&plugin.asset_pattern, tag, &target);
        let binary_path = plugin.expand_template(&plugin.binary, tag, &target);
        let man_paths: Vec<String> = plugin
            .man_pages
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|template| plugin.expand_template(template, tag, &target))
            .collect();

        debug!("Asset: {asset_name}");
        debug!("Binary path in archive: {binary_path}");

        let asset = release
            .assets
            .iter()
            .find(|candidate| candidate.name == asset_name)
            .ok_or_else(|| {
                let available = release
                    .assets
                    .iter()
                    .map(|candidate| candidate.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow!(
                    "Asset '{asset_name}' not found in release {tag} of {owner}/{repo}.\n\
                     Available assets: {available}"
                )
            })?;

        info!("Downloading {}…", asset.name);
        let data = client
            .download_asset(&asset.browser_download_url, asset.size)
            .await?;
        let checksum_sha256 = self
            .resolve_expected_sha256(plugin, client, &release.assets, asset, tag, &target)
            .await?;
        self.verify_sha256(&data, &checksum_sha256)?;

        let payload = if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
            self.extract_from_targz(&data, &binary_path, &man_paths)?
        } else if asset_name.ends_with(".tar.xz") || asset_name.ends_with(".txz") {
            self.extract_from_tar_xz(&data, &binary_path, &man_paths)?
        } else if asset_name.ends_with(".tar.zst") || asset_name.ends_with(".tar.zstd") {
            self.extract_from_tar_zst(&data, &binary_path, &man_paths)?
        } else if asset_name.ends_with(".tar.bz2") || asset_name.ends_with(".tbz2") {
            self.extract_from_tar_bz2(&data, &binary_path, &man_paths)?
        } else if asset_name.ends_with(".zip") {
            self.extract_from_zip(&data, &binary_path, &man_paths)?
        } else if asset_name.ends_with(".gz") {
            // Single gzip-compressed binary (not a tar archive).
            let mut decoder = GzDecoder::new(Cursor::new(data));
            let mut bytes = Vec::new();
            io::Read::read_to_end(&mut decoder, &mut bytes)
                .context("Failed to decompress .gz binary")?;
            InstallPayload {
                binary_filename: asset_name
                    .strip_suffix(".gz")
                    .and_then(|s| Path::new(s).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or(&plugin.name)
                    .to_string(),
                binary_contents: bytes,
                man_pages: Vec::new(),
            }
        } else {
            // Assume a raw binary (no archive).
            InstallPayload {
                binary_filename: Path::new(&binary_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&plugin.name)
                    .to_string(),
                binary_contents: data,
                man_pages: Vec::new(),
            }
        };

        if dry_run {
            println!(
                "[dry-run] Would install '{}' → {}",
                payload.binary_filename,
                self.local_bin.join(&payload.binary_filename).display()
            );
            return Ok(());
        }

        let _lock = self.acquire_state_lock().await?;
        let installed_paths = self.commit_install(payload)?;

        let mut state = self.load_state()?;
        let previous = state
            .installed
            .iter()
            .find(|p| p.name == plugin.name)
            .cloned();
        let pinned = previous.as_ref().map(|p| p.pinned).unwrap_or(false);
        state
            .installed
            .retain(|package| package.name != plugin.name);
        state.installed.push(InstalledPackage {
            name: plugin.name.clone(),
            version: tag.clone(),
            binary: installed_paths.binary_filename.clone(),
            source: Some(plugin.location.clone()),
            target: Some(target),
            asset_name: Some(asset_name),
            checksum_sha256: Some(checksum_sha256),
            man_pages: installed_paths.man_page_filenames,
            installed_at_unix: Some(current_unix_timestamp()?),
            pinned,
        });
        let action = if let Some(previous) = previous {
            HistoryEvent {
                package: plugin.name.clone(),
                action: HistoryAction::Updated,
                timestamp_unix: current_unix_timestamp()?,
                version: Some(tag.clone()),
                from_version: Some(previous.version),
                to_version: Some(tag.clone()),
                detail: Some(format!(
                    "Installed binary {}",
                    installed_paths.binary_filename
                )),
            }
        } else {
            HistoryEvent {
                package: plugin.name.clone(),
                action: HistoryAction::Installed,
                timestamp_unix: current_unix_timestamp()?,
                version: Some(tag.clone()),
                from_version: None,
                to_version: Some(tag.clone()),
                detail: Some(format!(
                    "Installed binary {}",
                    installed_paths.binary_filename
                )),
            }
        };
        state.history.push(action);
        self.save_state(&state)?;

        println!(
            "✓ Installed '{}' → {}",
            installed_paths.binary_filename,
            self.local_bin
                .join(&installed_paths.binary_filename)
                .display()
        );

        Ok(())
    }

    /// Remove an installed package and its man pages.
    ///
    /// When `dry_run` is `true`, nothing is removed from disk or state.
    pub async fn uninstall(&self, plugin: &Plugin, dry_run: bool) -> Result<()> {
        let _lock = self.acquire_state_lock().await?;
        let state = self.load_state()?;
        let package = state
            .installed
            .iter()
            .find(|installed| installed.name == plugin.name)
            .ok_or_else(|| anyhow!("'{}' is not installed", plugin.name))?
            .clone();

        let binary_dest = self.local_bin.join(&package.binary);
        if dry_run {
            println!("[dry-run] Would remove {}", binary_dest.display());
            for filename in &package.man_pages {
                println!(
                    "[dry-run] Would remove {}",
                    self.local_man.join(filename).display()
                );
            }
            println!("[dry-run] Would uninstall '{}'", plugin.name);
            return Ok(());
        }

        if binary_dest.exists() {
            fs::remove_file(&binary_dest)
                .with_context(|| format!("Failed to remove {}", binary_dest.display()))?;
            println!("Removed {}", binary_dest.display());
        }

        for filename in &package.man_pages {
            let man_dest = self.local_man.join(filename);
            if man_dest.exists() {
                if let Err(err) = fs::remove_file(&man_dest) {
                    warn!("Failed to remove man page {}: {err}", man_dest.display());
                } else {
                    println!("Removed {}", man_dest.display());
                }
            }
        }

        let mut state = self.load_state()?;
        state
            .installed
            .retain(|installed| installed.name != plugin.name);
        let removed_version = package.version.clone();
        state.history.push(HistoryEvent {
            package: plugin.name.clone(),
            action: HistoryAction::Removed,
            timestamp_unix: current_unix_timestamp()?,
            version: Some(removed_version.clone()),
            from_version: Some(removed_version),
            to_version: None,
            detail: Some(format!("Removed binary {}", package.binary)),
        });
        self.save_state(&state)?;

        println!("✓ Uninstalled '{}'", plugin.name);
        Ok(())
    }

    async fn acquire_state_lock(&self) -> Result<StateLock> {
        let lock_path = self.state_file.with_extension("lock");
        for _ in 0..LOCK_RETRY_ATTEMPTS {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => {
                    return Ok(StateLock { path: lock_path });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    if self.clear_stale_lock(&lock_path)? {
                        warn!("Removed stale installer lock {}", lock_path.display());
                        continue;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        LOCK_RETRY_DELAY_MS,
                    ))
                    .await;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("Failed to create lock {}", lock_path.display())
                    });
                }
            }
        }

        Err(anyhow!(
            "Timed out waiting for installer lock {}. If a previous scpr process crashed, remove the lock file or wait for it to become stale.",
            lock_path.display()
        ))
    }

    fn clear_stale_lock(&self, lock_path: &Path) -> Result<bool> {
        let metadata = match fs::metadata(lock_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to inspect lock {}", lock_path.display())
                });
            }
        };
        let modified = metadata.modified().with_context(|| {
            format!("Failed to read lock timestamp {}", lock_path.display())
        })?;
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_secs();
        if age < LOCK_STALE_AFTER_SECS {
            return Ok(false);
        }
        match fs::remove_file(lock_path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).with_context(|| {
                format!("Failed to remove stale lock {}", lock_path.display())
            }),
        }
    }

    fn extract_from_targz(
        &self,
        data: &[u8],
        binary_path: &str,
        man_paths: &[String],
    ) -> Result<InstallPayload> {
        let gz = GzDecoder::new(Cursor::new(data));
        let mut archive = Archive::new(gz);
        self.extract_from_tar(archive.entries()?, binary_path, man_paths)
    }

    fn extract_from_tar_xz(
        &self,
        data: &[u8],
        binary_path: &str,
        man_paths: &[String],
    ) -> Result<InstallPayload> {
        let xz = XzDecoder::new(Cursor::new(data));
        let mut archive = Archive::new(xz);
        self.extract_from_tar(archive.entries()?, binary_path, man_paths)
    }

    fn extract_from_tar_zst(
        &self,
        data: &[u8],
        binary_path: &str,
        man_paths: &[String],
    ) -> Result<InstallPayload> {
        let zst =
            ZstdDecoder::new(Cursor::new(data)).context("Failed to init zstd decoder")?;
        let mut archive = Archive::new(zst);
        self.extract_from_tar(archive.entries()?, binary_path, man_paths)
    }

    fn extract_from_tar_bz2(
        &self,
        data: &[u8],
        binary_path: &str,
        man_paths: &[String],
    ) -> Result<InstallPayload> {
        let bz = BzDecoder::new(Cursor::new(data));
        let mut archive = Archive::new(bz);
        self.extract_from_tar(archive.entries()?, binary_path, man_paths)
    }

    fn extract_from_tar<R: io::Read>(
        &self,
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

            // Reject any entry that would escape the archive root via path traversal.
            if has_path_traversal(&path) {
                warn!("Skipping unsafe tar entry: {path_str}");
                continue;
            }

            if binary_contents.is_none() && path_str == binary_path {
                debug!("Extracting binary: {path_str}");
                let filename = Path::new(&path_str)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("Invalid binary path: {path_str}"))?
                    .to_string();
                let mut bytes = Vec::new();
                io::Read::read_to_end(&mut entry, &mut bytes)
                    .context("Failed to read binary from archive")?;
                binary_filename = Some(filename);
                binary_contents = Some(bytes);
            } else if man_pages_left.contains(path_str.as_str()) {
                let man_filename = Path::new(&path_str)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("Invalid man page path: {path_str}"))?;
                let mut bytes = Vec::new();
                io::Read::read_to_end(&mut entry, &mut bytes)
                    .context("Failed to read man page from archive")?;
                extracted_man_pages.push((man_filename.to_string(), bytes));
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
        &self,
        data: &[u8],
        binary_path: &str,
        man_paths: &[String],
    ) -> Result<InstallPayload> {
        let mut archive = zip::ZipArchive::new(Cursor::new(data))
            .context("Failed to open zip archive")?;

        let binary_filename;
        let binary_contents;

        {
            let mut entry = archive.by_name(binary_path).with_context(|| {
                format!("Binary '{binary_path}' not found in zip archive")
            })?;
            binary_filename = Path::new(binary_path)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("Invalid binary path: {binary_path}"))?
                .to_string();
            let mut bytes = Vec::new();
            io::Read::read_to_end(&mut entry, &mut bytes)
                .context("Failed to read binary from zip archive")?;
            binary_contents = bytes;
        }

        let mut extracted_man_pages = Vec::new();
        for man_path in man_paths {
            match archive.by_name(man_path) {
                Ok(mut entry) => {
                    let man_filename = Path::new(man_path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| anyhow!("Invalid man page path: {man_path}"))?;
                    let mut bytes = Vec::new();
                    io::Read::read_to_end(&mut entry, &mut bytes)
                        .context("Failed to read man page from zip archive")?;
                    extracted_man_pages.push((man_filename.to_string(), bytes));
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

    async fn resolve_expected_sha256(
        &self,
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

        let checksum_pattern =
            plugin.checksum_asset_pattern.as_deref().ok_or_else(|| {
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

    fn verify_sha256(&self, data: &[u8], expected_sha256: &str) -> Result<()> {
        let actual = sha256_hex(data);
        if actual != expected_sha256 {
            return Err(anyhow!(
                "SHA-256 mismatch: expected {expected_sha256}, got {actual}"
            ));
        }
        info!("Verified SHA-256: {actual}");
        Ok(())
    }

    fn commit_install(&self, payload: InstallPayload) -> Result<InstalledPaths> {
        fs::create_dir_all(&self.local_bin)
            .with_context(|| format!("Failed to create {}", self.local_bin.display()))?;
        fs::create_dir_all(&self.local_man)
            .with_context(|| format!("Failed to create {}", self.local_man.display()))?;

        let mut staged = Vec::new();
        let binary_dest = self.local_bin.join(&payload.binary_filename);
        staged.push(stage_file(
            &self.local_bin,
            &binary_dest,
            &payload.binary_contents,
            true,
        )?);

        let mut man_page_filenames = Vec::new();
        for (filename, contents) in payload.man_pages {
            let dest = self.local_man.join(&filename);
            staged.push(stage_file(&self.local_man, &dest, &contents, false)?);
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
                    return Err(anyhow!(
                        "Failed to replace {}: {}",
                        staged_file.dest.display(),
                        err.error
                    ));
                }
            }
        }

        cleanup_backup_files(backups);

        Ok(InstalledPaths {
            binary_filename: payload.binary_filename,
            man_page_filenames,
        })
    }

    /// Mark an installed package as pinned so `update --all` will skip it.
    pub fn pin(&self, name: &str) -> Result<()> {
        self.set_pinned(name, true)
    }

    /// Remove the pin from an installed package.
    pub fn unpin(&self, name: &str) -> Result<()> {
        self.set_pinned(name, false)
    }

    fn set_pinned(&self, name: &str, pinned: bool) -> Result<()> {
        let _lock = self.acquire_state_lock_blocking()?;
        let mut state = self.load_state()?;
        let version = {
            let pkg = state
                .installed
                .iter_mut()
                .find(|p| p.name == name)
                .ok_or_else(|| anyhow!("'{name}' is not installed"))?;
            pkg.pinned = pinned;
            pkg.version.clone()
        };
        state.history.push(HistoryEvent {
            package: name.to_string(),
            action: if pinned {
                HistoryAction::Pinned
            } else {
                HistoryAction::Unpinned
            },
            timestamp_unix: current_unix_timestamp()?,
            version: Some(version),
            from_version: None,
            to_version: None,
            detail: Some(if pinned {
                "Package pinned".to_string()
            } else {
                "Package unpinned".to_string()
            }),
        });
        self.save_state(&state)?;
        if pinned {
            println!("Pinned '{name}' — it will be skipped by `update --all`");
        } else {
            println!("Unpinned '{name}'");
        }
        Ok(())
    }

    pub fn audit(&self) -> Result<Vec<AuditRecord>> {
        let installed = self.load_state()?.installed;
        let mut records = Vec::new();

        for pkg in installed {
            let binary_path = self.local_bin.join(&pkg.binary);
            let Some(expected) = pkg.checksum_sha256.clone() else {
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Untracked,
                    expected_checksum: None,
                    actual_checksum: None,
                    detail: "No stored checksum; cannot verify local changes".to_string(),
                });
                continue;
            };

            if !binary_path.exists() {
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Missing,
                    expected_checksum: Some(expected),
                    actual_checksum: None,
                    detail: "Installed binary is missing".to_string(),
                });
                continue;
            }

            let data = fs::read(&binary_path).with_context(|| {
                format!("Failed to read installed binary {}", binary_path.display())
            })?;
            let actual = sha256_hex(&data);
            if actual == expected {
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Ok,
                    expected_checksum: Some(expected),
                    actual_checksum: Some(actual),
                    detail: "Binary matches the recorded SHA-256 checksum".to_string(),
                });
            } else {
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Modified,
                    expected_checksum: Some(expected),
                    actual_checksum: Some(actual),
                    detail: "Binary contents have changed since installation".to_string(),
                });
            }
        }

        records.sort_by(|left, right| left.package.cmp(&right.package));
        Ok(records)
    }

    pub fn history(&self, package: Option<&str>) -> Result<Vec<HistoryEvent>> {
        let mut events = self.load_state()?.history;
        if let Some(package) = package {
            events.retain(|event| event.package == package);
        }
        events.sort_by_key(|event| event.timestamp_unix);
        Ok(events)
    }

    pub fn history_limited(
        &self,
        package: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<HistoryEvent>> {
        let mut events = self.history(package)?;
        if let Some(limit) = limit
            && events.len() > limit
        {
            events = events.split_off(events.len() - limit);
        }
        Ok(events)
    }

    pub fn clear_history(&self, package: Option<&str>) -> Result<usize> {
        let _lock = self.acquire_state_lock_blocking()?;
        let mut state = self.load_state()?;
        let before = state.history.len();
        if let Some(package) = package {
            state.history.retain(|event| event.package != package);
        } else {
            state.history.clear();
        }
        let removed = before.saturating_sub(state.history.len());
        self.save_state(&state)?;
        Ok(removed)
    }

    pub fn export_state(&self, format: StateFormat) -> Result<String> {
        let state = self.load_state()?;
        match format {
            StateFormat::Json => serde_json::to_string_pretty(&state)
                .context("Failed to serialize state as JSON"),
            StateFormat::Toml => toml::to_string_pretty(&state)
                .context("Failed to serialize state as TOML"),
        }
    }

    pub fn restore_state(&self, contents: &str, format: StateFormat) -> Result<()> {
        let _lock = self.acquire_state_lock_blocking()?;
        let state: State = match format {
            StateFormat::Json => serde_json::from_str(contents)
                .context("Failed to parse JSON state backup")?,
            StateFormat::Toml => {
                toml::from_str(contents).context("Failed to parse TOML state backup")?
            }
        };
        self.save_state(&state)
    }

    fn acquire_state_lock_blocking(&self) -> Result<StateLock> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .context("Failed to initialize runtime for installer lock")?;
        runtime.block_on(self.acquire_state_lock())
    }
}

/// Return `true` if `path` contains any component that would escape the
/// archive root (absolute prefix, `..`, etc.).
fn has_path_traversal(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir
        )
    })
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_secs())
}

fn default_state_version() -> u32 {
    STATE_VERSION
}

fn parse_sha256_digest(digest: &str) -> Result<String> {
    let normalized = digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .trim()
        .to_ascii_lowercase();
    validate_sha256_hex(&normalized)?;
    Ok(normalized)
}

fn parse_sha256_checksum_file(contents: &str, asset_name: &str) -> Result<String> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
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

fn validate_sha256_hex(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("Invalid SHA-256 digest: {value}"));
    }
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
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

impl Drop for StateLock {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuditStatus, HistoryAction, InstallPayload, InstalledPackage, Installer,
        STATE_VERSION, State, StateFormat, parse_sha256_checksum_file,
        parse_sha256_digest,
    };
    use crate::plugin::Plugin;
    use std::path::PathBuf;

    fn temp_installer() -> Installer {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.keep();
        let local_bin = root.join("bin");
        let local_man = root.join("man");
        let state_dir = root.join("state");
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::create_dir_all(&local_man).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        Installer {
            local_bin,
            local_man,
            state_file: state_dir.join("state.toml"),
        }
    }

    fn sample_plugin() -> Plugin {
        Plugin {
            name: "ripgrep".to_string(),
            alias: vec!["rg".to_string()],
            description: Some("sample".to_string()),
            location: "github:BurntSushi/ripgrep".to_string(),
            asset_pattern: "{name}-{version}-{target}.tar.gz".to_string(),
            checksum_asset_pattern: Some(
                "{name}-{version}-{target}.tar.gz.sha256".to_string(),
            ),
            binary: "{name}-{version}-{target}/rg".to_string(),
            man_pages: Some(vec!["{name}-{version}-{target}/doc/rg.1".to_string()]),
            targets: None,
        }
    }

    #[test]
    fn test_parse_sha256_digest_accepts_prefixed_value() {
        let value = parse_sha256_digest(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert_eq!(
            value,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn test_parse_sha256_checksum_file_matches_asset_name() {
        let checksum = parse_sha256_checksum_file(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ripgrep.tar.gz",
            "ripgrep.tar.gz",
        )
        .unwrap();
        assert_eq!(
            checksum,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn test_parse_sha256_checksum_file_accepts_single_value() {
        let checksum = parse_sha256_checksum_file(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "ignored",
        )
        .unwrap();
        assert_eq!(
            checksum,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[tokio::test]
    async fn test_acquire_state_lock_blocks_when_lock_exists() {
        let installer = temp_installer();
        let lock_path = installer.state_file_path().with_extension("lock");
        std::fs::write(&lock_path, b"busy").unwrap();

        let error = installer.acquire_state_lock().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Timed out waiting for installer lock")
        );

        std::fs::remove_file(lock_path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_acquire_state_lock_clears_stale_lock() {
        let installer = temp_installer();
        let lock_path = installer.state_file_path().with_extension("lock");
        std::fs::write(&lock_path, b"stale").unwrap();
        std::process::Command::new("touch")
            .arg("-d")
            .arg("2 minutes ago")
            .arg(&lock_path)
            .status()
            .unwrap();

        let _lock = installer.acquire_state_lock().await.unwrap();
        assert!(lock_path.exists());
    }

    #[tokio::test]
    async fn test_state_lock_removed_on_drop() {
        let installer = temp_installer();
        let lock_path: PathBuf = installer.state_file_path().with_extension("lock");

        {
            let _lock = installer.acquire_state_lock().await.unwrap();
            assert!(lock_path.exists());
        }

        assert!(!lock_path.exists());
    }

    #[test]
    fn test_commit_install_writes_binary_and_man_page() {
        let installer = temp_installer();
        let payload = InstallPayload {
            binary_filename: "rg".to_string(),
            binary_contents: b"binary".to_vec(),
            man_pages: vec![("rg.1".to_string(), b"manual".to_vec())],
        };

        let installed = installer.commit_install(payload).unwrap();

        assert_eq!(installed.binary_filename, "rg");
        assert_eq!(installed.man_page_filenames, vec!["rg.1".to_string()]);
        assert_eq!(
            std::fs::read(installer.local_bin_dir().join("rg")).unwrap(),
            b"binary"
        );
        assert_eq!(
            std::fs::read(installer.local_man_dir().join("rg.1")).unwrap(),
            b"manual"
        );
    }

    #[test]
    fn test_uninstall_removes_tracked_files_and_state() {
        let installer = temp_installer();
        let plugin = sample_plugin();
        let binary_path = installer.local_bin_dir().join("rg");
        let man_path = installer.local_man_dir().join("rg.1");

        std::fs::write(&binary_path, b"binary").unwrap();
        std::fs::write(&man_path, b"manual").unwrap();
        installer
            .save_state(&State {
                version: STATE_VERSION,
                installed: vec![InstalledPackage {
                    name: "ripgrep".to_string(),
                    version: "v15.1.0".to_string(),
                    binary: "rg".to_string(),
                    source: Some("github:BurntSushi/ripgrep".to_string()),
                    target: Some("x86_64-unknown-linux-musl".to_string()),
                    asset_name: Some(
                        "ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz".to_string(),
                    ),
                    checksum_sha256: Some("a".repeat(64)),
                    man_pages: vec!["rg.1".to_string()],
                    installed_at_unix: Some(1),
                    pinned: false,
                }],
                history: Vec::new(),
            })
            .unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(installer.uninstall(&plugin, false))
            .unwrap();

        assert!(!binary_path.exists());
        assert!(!man_path.exists());
        assert!(installer.list_installed().unwrap().is_empty());
        let history = installer.history(Some("ripgrep")).unwrap();
        assert!(matches!(
            history.last().unwrap().action,
            HistoryAction::Removed
        ));
    }

    #[test]
    fn test_audit_detects_modified_binary() {
        let installer = temp_installer();
        let binary_path = installer.local_bin_dir().join("rg");
        std::fs::write(&binary_path, b"modified").unwrap();
        installer
            .save_state(&State {
                version: STATE_VERSION,
                installed: vec![InstalledPackage {
                    name: "ripgrep".to_string(),
                    version: "v15.1.0".to_string(),
                    binary: "rg".to_string(),
                    source: None,
                    target: None,
                    asset_name: None,
                    checksum_sha256: Some("a".repeat(64)),
                    man_pages: Vec::new(),
                    installed_at_unix: Some(1),
                    pinned: false,
                }],
                history: Vec::new(),
            })
            .unwrap();

        let audit = installer.audit().unwrap();
        assert_eq!(audit.len(), 1);
        assert!(matches!(audit[0].status, AuditStatus::Modified));
    }

    #[test]
    fn test_pin_records_history() {
        let installer = temp_installer();
        installer
            .save_state(&State {
                version: STATE_VERSION,
                installed: vec![InstalledPackage {
                    name: "ripgrep".to_string(),
                    version: "v15.1.0".to_string(),
                    binary: "rg".to_string(),
                    source: None,
                    target: None,
                    asset_name: None,
                    checksum_sha256: Some("a".repeat(64)),
                    man_pages: Vec::new(),
                    installed_at_unix: Some(1),
                    pinned: false,
                }],
                history: Vec::new(),
            })
            .unwrap();

        installer.pin("ripgrep").unwrap();
        let history = installer.history(Some("ripgrep")).unwrap();
        assert!(matches!(
            history.last().unwrap().action,
            HistoryAction::Pinned
        ));
    }

    #[test]
    fn test_history_limited_returns_most_recent_events() {
        let installer = temp_installer();
        installer
            .save_state(&State {
                version: STATE_VERSION,
                installed: Vec::new(),
                history: vec![
                    super::HistoryEvent {
                        package: "ripgrep".to_string(),
                        action: HistoryAction::Installed,
                        timestamp_unix: 1,
                        version: Some("v1".to_string()),
                        from_version: None,
                        to_version: Some("v1".to_string()),
                        detail: None,
                    },
                    super::HistoryEvent {
                        package: "ripgrep".to_string(),
                        action: HistoryAction::Updated,
                        timestamp_unix: 2,
                        version: Some("v2".to_string()),
                        from_version: Some("v1".to_string()),
                        to_version: Some("v2".to_string()),
                        detail: None,
                    },
                    super::HistoryEvent {
                        package: "ripgrep".to_string(),
                        action: HistoryAction::Removed,
                        timestamp_unix: 3,
                        version: Some("v2".to_string()),
                        from_version: Some("v2".to_string()),
                        to_version: None,
                        detail: None,
                    },
                ],
            })
            .unwrap();

        let history = installer.history_limited(Some("ripgrep"), Some(2)).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].timestamp_unix, 2);
        assert_eq!(history[1].timestamp_unix, 3);
    }

    #[test]
    fn test_export_and_restore_state_json_round_trip() {
        let installer = temp_installer();
        installer
            .save_state(&State {
                version: STATE_VERSION,
                installed: vec![InstalledPackage {
                    name: "ripgrep".to_string(),
                    version: "v15.1.0".to_string(),
                    binary: "rg".to_string(),
                    source: None,
                    target: None,
                    asset_name: None,
                    checksum_sha256: Some("a".repeat(64)),
                    man_pages: Vec::new(),
                    installed_at_unix: Some(1),
                    pinned: false,
                }],
                history: Vec::new(),
            })
            .unwrap();

        let exported = installer.export_state(StateFormat::Json).unwrap();

        let restored = temp_installer();
        restored
            .restore_state(&exported, StateFormat::Json)
            .unwrap();
        let installed = restored.list_installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "ripgrep");
    }

    #[test]
    fn test_exported_state_includes_schema_version() {
        let installer = temp_installer();
        let exported = installer.export_state(StateFormat::Toml).unwrap();
        assert!(exported.contains(&format!("version = {}", STATE_VERSION)));
    }

    #[test]
    fn test_load_state_rejects_unsupported_schema_version() {
        let installer = temp_installer();
        std::fs::write(
            installer.state_file_path(),
            "version = 99\ninstalled = []\nhistory = []\n",
        )
        .unwrap();

        let error = installer.list_installed().unwrap_err();
        assert!(error.to_string().contains("Unsupported state file version"));
    }

    #[test]
    fn test_clear_history_removes_matching_events() {
        let installer = temp_installer();
        installer
            .save_state(&State {
                version: STATE_VERSION,
                installed: Vec::new(),
                history: vec![
                    super::HistoryEvent {
                        package: "ripgrep".to_string(),
                        action: HistoryAction::Installed,
                        timestamp_unix: 1,
                        version: Some("v1".to_string()),
                        from_version: None,
                        to_version: Some("v1".to_string()),
                        detail: None,
                    },
                    super::HistoryEvent {
                        package: "fd".to_string(),
                        action: HistoryAction::Installed,
                        timestamp_unix: 2,
                        version: Some("v1".to_string()),
                        from_version: None,
                        to_version: Some("v1".to_string()),
                        detail: None,
                    },
                ],
            })
            .unwrap();

        let removed = installer.clear_history(Some("ripgrep")).unwrap();
        assert_eq!(removed, 1);
        let history = installer.history(None).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].package, "fd");
    }
}
