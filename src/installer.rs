use crate::{
    github::{GithubClient, ReleaseAsset},
    installer_archive::{self, InstallPayload, InstalledPaths},
    plugin::Plugin,
    settings::AppSettings,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    io::ErrorKind,
    ops::Drop,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};

const LOCK_RETRY_DELAY_MS: u64 = 100;
const LOCK_RETRY_ATTEMPTS: usize = 100;
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct LegacyStateV0 {
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
    lock_stale_after_secs: u64,
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
            lock_stale_after_secs: settings.lock_stale_after_secs(),
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
        let value: toml::Value =
            toml::from_str(&content).context("Failed to parse state file")?;
        migrate_state_value(value, &self.state_file)
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
                    "Asset pattern '{}' resolved to '{}' but no matching asset was found in release {tag} of {owner}/{repo}.\n\
                     Binary path template: '{}'\n\
                     Available assets: {available}",
                    plugin.asset_pattern,
                    asset_name,
                    plugin.binary,
                )
            })?;

        info!("Downloading {}…", asset.name);
        let data = client
            .download_asset(&asset.browser_download_url, asset.size)
            .await?;
        let checksum_sha256 = self
            .resolve_expected_sha256(plugin, client, &release.assets, asset, tag, &target)
            .await?;
        installer_archive::verify_signature_if_configured(
            plugin,
            client,
            &release.assets,
            asset,
            &data,
            tag,
            &target,
        )
        .await?;
        if let Some(expected_sha256) = checksum_sha256.as_deref() {
            self.verify_sha256(&data, expected_sha256)?;
        }

        let payload = installer_archive::extract_install_payload(
            &asset_name,
            &data,
            &binary_path,
            &man_paths,
            &plugin.name,
        )?;

        if dry_run {
            println!(
                "[dry-run] Would install '{}' → {}",
                payload.binary_filename,
                self.local_bin.join(&payload.binary_filename).display()
            );
            if checksum_sha256.is_none() {
                println!(
                    "[dry-run] Warning: '{}' would be installed without SHA-256 verification",
                    plugin.name
                );
            }
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
            checksum_sha256,
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
        self.run_post_install_hooks(plugin, &installed_paths.binary_filename, dry_run)?;

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
        if age < self.lock_stale_after_secs {
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

    async fn resolve_expected_sha256(
        &self,
        plugin: &Plugin,
        client: &GithubClient,
        assets: &[ReleaseAsset],
        asset: &ReleaseAsset,
        tag: &str,
        target: &str,
    ) -> Result<Option<String>> {
        installer_archive::resolve_expected_sha256(
            plugin, client, assets, asset, tag, target,
        )
        .await
    }

    fn verify_sha256(&self, data: &[u8], expected_sha256: &str) -> Result<()> {
        installer_archive::verify_sha256(data, expected_sha256)
    }

    fn commit_install(&self, payload: InstallPayload) -> Result<InstalledPaths> {
        installer_archive::commit_install(&self.local_bin, &self.local_man, payload)
    }

    fn run_post_install_hooks(
        &self,
        plugin: &Plugin,
        binary_filename: &str,
        dry_run: bool,
    ) -> Result<()> {
        let Some(hooks) = plugin.post_install.as_deref() else {
            return Ok(());
        };
        let binary_path = self.local_bin.join(binary_filename);
        for hook in hooks {
            let command = hook
                .replace("{binary_path}", &binary_path.display().to_string())
                .replace("{binary_name}", binary_filename)
                .replace("{plugin}", &plugin.name);
            if dry_run {
                println!("[dry-run] Would run post-install hook: {command}");
                continue;
            }
            let status = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .status()
                .with_context(|| {
                    format!(
                        "Failed to execute post-install hook for '{}': {}",
                        plugin.name, command
                    )
                })?;
            if !status.success() {
                anyhow::bail!(
                    "Post-install hook failed for '{}': {}",
                    plugin.name,
                    command
                );
            }
        }
        Ok(())
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
            let actual = installer_archive::sha256_hex(&data);
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

    pub fn rollback_version(&self, package: &str) -> Result<String> {
        let state = self.load_state()?;
        let current = state
            .installed
            .iter()
            .find(|installed| installed.name == package)
            .ok_or_else(|| anyhow!("'{package}' is not installed"))?;

        state
            .history
            .iter()
            .rev()
            .find_map(|event| {
                if event.package == package
                    && matches!(event.action, HistoryAction::Updated)
                    && event.to_version.as_deref() == Some(current.version.as_str())
                {
                    event.from_version.clone()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                anyhow!(
                    "No previous version is recorded for '{}'; rollback is only available after an update",
                    package
                )
            })
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
        self.back_up_state_file()?;
        self.save_state(&state)
    }

    fn acquire_state_lock_blocking(&self) -> Result<StateLock> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .context("Failed to initialize runtime for installer lock")?;
        runtime.block_on(self.acquire_state_lock())
    }

    fn back_up_state_file(&self) -> Result<()> {
        if !self.state_file.exists() {
            return Ok(());
        }
        let backup_path = self.state_file.with_extension("toml.bak");
        fs::copy(&self.state_file, &backup_path).with_context(|| {
            format!(
                "Failed to back up state file from {} to {}",
                self.state_file.display(),
                backup_path.display()
            )
        })?;
        Ok(())
    }
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

fn migrate_state_value(value: toml::Value, path: &Path) -> Result<State> {
    let version = value
        .get("version")
        .and_then(toml::Value::as_integer)
        .unwrap_or(0);

    match version {
        0 => migrate_state_v0(value),
        1 => toml::Value::try_into(value).context("Failed to parse state file"),
        other => Err(anyhow!(
            "Unsupported state file version {} in {}. Supported versions: 0, {}.",
            other,
            path.display(),
            STATE_VERSION
        )),
    }
}

fn migrate_state_v0(value: toml::Value) -> Result<State> {
    let legacy: LegacyStateV0 =
        toml::Value::try_into(value).context("Failed to parse legacy v0 state file")?;
    Ok(State {
        version: STATE_VERSION,
        installed: legacy.installed,
        history: legacy.history,
    })
}

impl Drop for StateLock {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "installer_tests.rs"]
mod tests;
