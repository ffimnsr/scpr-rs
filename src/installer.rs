use crate::{
    github::{GithubClient, ReleaseAsset},
    installer_archive::{self, InstallPayload, InstalledPaths},
    plugin::Plugin,
    settings::AppSettings,
};
use anyhow::{Context, Result, anyhow};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    io::ErrorKind,
    io::{BufRead, IsTerminal, Write},
    ops::Drop,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};

const LOCK_RETRY_DELAY_MS: u64 = 100;
const LOCK_RETRY_ATTEMPTS: usize = 100;
const STATE_VERSION: u32 = 1;

#[derive(Clone, Copy)]
enum ManagedCompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl ManagedCompletionShell {
    fn from_env(shell: &str) -> Option<Self> {
        let shell_name = Path::new(shell).file_name()?.to_str()?;
        match shell_name {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    fn profile_path(self, home: &Path) -> Option<PathBuf> {
        match self {
            Self::Bash => Some(home.join(".bashrc")),
            Self::Zsh => Some(home.join(".zshrc")),
            Self::Fish => None,
        }
    }

    fn managed_file_path(self, home: &Path, plugin_name: &str) -> PathBuf {
        match self {
            Self::Bash => home.join(".bashrc.d").join(format!("{plugin_name}.sh")),
            Self::Zsh => home.join(".zshrc.d").join(format!("{plugin_name}.sh")),
            Self::Fish => home
                .join(".config")
                .join("fish")
                .join("completions")
                .join(format!("{plugin_name}.fish")),
        }
    }

    fn source_line(self, plugin_name: &str) -> Option<String> {
        match self {
            Self::Bash => Some(format!(
                "[ -f \"$HOME/.bashrc.d/{plugin_name}.sh\" ] && . \"$HOME/.bashrc.d/{plugin_name}.sh\""
            )),
            Self::Zsh => Some(format!(
                "[ -f \"$HOME/.zshrc.d/{plugin_name}.sh\" ] && . \"$HOME/.zshrc.d/{plugin_name}.sh\""
            )),
            Self::Fish => None,
        }
    }

    fn profile_loads_drop_in(self, contents: &str) -> bool {
        match self {
            Self::Bash => contents.contains(".bashrc.d"),
            Self::Zsh => contents.contains(".zshrc.d"),
            Self::Fish => true,
        }
    }
}

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
    /// SHA-256 checksum of the downloaded release asset, when available.
    #[serde(default)]
    pub checksum_sha256: Option<String>,
    /// SHA-256 checksum of the installed binary contents.
    #[serde(default)]
    pub binary_checksum_sha256: Option<String>,
    #[serde(default)]
    pub man_pages: Vec<String>,
    #[serde(default)]
    pub installed_at_unix: Option<u64>,
    #[serde(default)]
    pub binary_mode: Option<u32>,
    #[serde(default)]
    pub binary_owner_uid: Option<u32>,
    #[serde(default)]
    pub binary_owner_gid: Option<u32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub describe: Option<AuditDescription>,
}

#[derive(Debug, Serialize, Clone)]
pub struct AuditDescription {
    pub fingerprint: AuditAspect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<AuditAspect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<AuditAspect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct AuditAspect {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct AuditBaseline {
    mode: Option<u32>,
    owner_uid: Option<u32>,
    owner_gid: Option<u32>,
}

#[derive(Debug)]
struct StateLock {
    path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct InstallOptions<'a> {
    pub tag: Option<&'a str>,
    pub target_override: Option<&'a str>,
    pub dry_run: bool,
    pub force: bool,
    pub build: bool,
}

#[derive(Debug, Clone, Copy)]
struct BuildRequest<'a> {
    owner: &'a str,
    repo: &'a str,
    source_ref: &'a str,
    template_tag: &'a str,
    target_override: Option<&'a str>,
    dry_run: bool,
}

#[derive(Debug, Clone, Copy)]
struct BuildExecution<'a> {
    plugin: &'a Plugin,
    template_tag: &'a str,
    target: Option<&'a str>,
    repo_dir: &'a Path,
    binary_path: &'a Path,
    dry_run: bool,
}

struct BuildRefResolution {
    source_ref: String,
    template_tag: String,
    version_label: String,
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
        options: InstallOptions<'_>,
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

        info!(
            "Installing {} for {os}/{arch}{}…",
            plugin.name,
            if options.build { " from source" } else { "" }
        );

        let (payload, target, asset_name, checksum_sha256, action_verb, version) = if options.build
        {
            let build_ref = resolve_build_ref(plugin, client, owner, repo, options.tag).await?;
            info!("Using source ref: {}", build_ref.source_ref);

            if !options.force
                && self.same_version_already_installed(
                    &plugin.name,
                    &build_ref.version_label,
                )?
            {
                println!(
                    "Skipping '{}': version '{}' is already installed. Use --force to reinstall.",
                    plugin.name, build_ref.version_label
                );
                return Ok(());
            }

            let (payload, target) = self
                .build_install_payload(
                    plugin,
                    client,
                    BuildRequest {
                        owner,
                        repo,
                        source_ref: &build_ref.source_ref,
                        template_tag: &build_ref.template_tag,
                        target_override: options.target_override,
                        dry_run: options.dry_run,
                    },
                )
                .await?;
            (payload, target, None, None, "Built and installed", build_ref.version_label)
        } else {
            let release = match options.tag {
                Some(tag) => client.get_release_by_tag(owner, repo, tag).await?,
                None => client.get_latest_release(owner, repo).await?,
            };
            let tag = release.tag_name;
            info!("Using release: {tag}");

            if !options.force && self.same_version_already_installed(&plugin.name, &tag)? {
                println!(
                    "Skipping '{}': version '{}' is already installed. Use --force to reinstall.",
                    plugin.name, tag
                );
                return Ok(());
            }
            if plugin.asset_pattern.trim().is_empty() {
                anyhow::bail!(
                    "Plugin '{}' does not define a release asset pattern. Re-run this command with --build.",
                    plugin.name
                );
            }
            let target =
                resolve_release_target(plugin, os, arch, options.target_override)?;
            debug!("Resolved target: {target}");

            let asset_name = plugin.expand_template(&plugin.asset_pattern, &tag, &target);
            let binary_path = plugin.expand_template(&plugin.binary, &tag, &target);
            let man_paths: Vec<String> = plugin
                .man_pages
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|template| plugin.expand_template(template, &tag, &target))
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
                .resolve_expected_sha256(
                    plugin,
                    client,
                    &release.assets,
                    asset,
                    &tag,
                    &target,
                )
                .await?;
            installer_archive::verify_signature_if_configured(
                plugin,
                client,
                &release.assets,
                asset,
                &data,
                &tag,
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

            (
                payload,
                Some(target),
                Some(asset_name),
                checksum_sha256,
                "Installed",
                tag,
            )
        };

        self.warn_if_binary_on_path_is_external(plugin, &payload.binary_filename)?;
        let binary_checksum_sha256 =
            installer_archive::sha256_hex(&payload.binary_contents);

        if options.dry_run {
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
        if !options.force
            && self.same_version_already_installed(&plugin.name, &version)?
        {
            println!(
                "Skipping '{}': version '{}' is already installed. Use --force to reinstall.",
                plugin.name, version
            );
            return Ok(());
        }
        let installed_paths = self.commit_install(payload)?;
        let binary_audit = self.capture_binary_audit_baseline(
            &self.local_bin.join(&installed_paths.binary_filename),
        )?;

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
            version: version.clone(),
            binary: installed_paths.binary_filename.clone(),
            source: Some(plugin.location.clone()),
            target,
            asset_name,
            checksum_sha256,
            binary_checksum_sha256: Some(binary_checksum_sha256),
            man_pages: installed_paths.man_page_filenames,
            installed_at_unix: Some(current_unix_timestamp()?),
            binary_mode: binary_audit.mode,
            binary_owner_uid: binary_audit.owner_uid,
            binary_owner_gid: binary_audit.owner_gid,
            pinned,
        });
        let action = if let Some(previous) = previous {
            HistoryEvent {
                package: plugin.name.clone(),
                action: HistoryAction::Updated,
                timestamp_unix: current_unix_timestamp()?,
                version: Some(version.clone()),
                from_version: Some(previous.version),
                to_version: Some(version.clone()),
                detail: Some(format!(
                    "{action_verb} binary {}",
                    installed_paths.binary_filename
                )),
            }
        } else {
            HistoryEvent {
                package: plugin.name.clone(),
                action: HistoryAction::Installed,
                timestamp_unix: current_unix_timestamp()?,
                version: Some(version.clone()),
                from_version: None,
                to_version: Some(version.clone()),
                detail: Some(format!(
                    "{action_verb} binary {}",
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
        self.run_managed_completion_install(
            plugin,
            &installed_paths.binary_filename,
            options.dry_run,
        )?;
        self.run_post_install_hooks(
            plugin,
            &installed_paths.binary_filename,
            options.dry_run,
        )?;

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
            self.run_managed_completion_uninstall(plugin, true)?;
            self.run_post_uninstall_hooks(plugin, &package.binary, true)?;
            self.run_cleanup_hooks(plugin, &package.binary, true)?;
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
        self.run_managed_completion_uninstall(plugin, false)?;
        self.run_post_uninstall_hooks(plugin, &package.binary, false)?;
        self.run_cleanup_hooks(plugin, &package.binary, false)?;
        Ok(())
    }

    async fn build_install_payload(
        &self,
        plugin: &Plugin,
        client: &GithubClient,
        request: BuildRequest<'_>,
    ) -> Result<(InstallPayload, Option<String>)> {
        info!(
            "Downloading source archive for {}/{}@{}…",
            request.owner, request.repo, request.source_ref
        );
        let archive = client
            .download_asset(
                &format!(
                    "https://api.github.com/repos/{}/{}/tarball/{}",
                    request.owner, request.repo, request.source_ref
                ),
                0,
            )
            .await?;
        let temp_dir = tempfile::tempdir()
            .context("Failed to create temporary directory for source build")?;
        let repo_dir = extract_github_tarball(&archive, temp_dir.path())?;
        let target = resolve_optional_target(
            plugin,
            std::env::consts::OS,
            std::env::consts::ARCH,
            request.target_override,
        );
        let binary_relative = expand_build_template(
            plugin,
            &plugin.binary,
            request.template_tag,
            target.as_deref(),
        )?;
        let binary_path = repo_dir.join(&binary_relative);
        let execution = BuildExecution {
            plugin,
            template_tag: request.template_tag,
            target: target.as_deref(),
            repo_dir: &repo_dir,
            binary_path: &binary_path,
            dry_run: request.dry_run,
        };

        let build_commands = match plugin.build_script.as_deref() {
            Some(commands) => commands.to_vec(),
            None => infer_build_commands(&repo_dir, target.as_deref())?,
        };
        self.run_build_commands(&build_commands, execution, "build")?;
        self.run_build_commands(
            plugin.post_build.as_deref().unwrap_or_default(),
            execution,
            "post-build",
        )?;

        if request.dry_run {
            let binary_filename = binary_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    anyhow!(
                        "Built binary path '{}' has no file name",
                        binary_path.display()
                    )
                })?
                .to_string();
            return Ok((
                InstallPayload {
                    binary_filename,
                    binary_contents: Vec::new(),
                    man_pages: Vec::new(),
                },
                target,
            ));
        }

        let binary_contents = fs::read(&binary_path).with_context(|| {
            format!(
                "Expected built binary for '{}' at {}",
                plugin.name,
                binary_path.display()
            )
        })?;
        let binary_filename = binary_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "Built binary path '{}' has no file name",
                    binary_path.display()
                )
            })?
            .to_string();
        let man_pages = plugin
            .man_pages
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|path| {
                let relative =
                    expand_build_template(
                        plugin,
                        path,
                        request.template_tag,
                        target.as_deref(),
                    )?;
                let full_path = repo_dir.join(&relative);
                let filename = full_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        anyhow!(
                            "Built man page path '{}' has no file name",
                            full_path.display()
                        )
                    })?
                    .to_string();
                let contents = fs::read(&full_path).with_context(|| {
                    format!(
                        "Expected built man page for '{}' at {}",
                        plugin.name,
                        full_path.display()
                    )
                })?;
                Ok((filename, contents))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok((
            InstallPayload {
                binary_filename,
                binary_contents,
                man_pages,
            },
            target,
        ))
    }

    fn run_build_commands(
        &self,
        commands: &[String],
        execution: BuildExecution<'_>,
        hook_kind: &str,
    ) -> Result<()> {
        for command in commands {
            let command = expand_build_command(
                execution.plugin,
                command,
                execution.template_tag,
                execution.target,
                execution.repo_dir,
                execution.binary_path,
            )?;
            if execution.dry_run {
                println!("[dry-run] Would run {hook_kind} step: {command}");
                continue;
            }
            let status = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(execution.repo_dir)
                .status()
                .with_context(|| {
                    format!(
                        "Failed to execute {hook_kind} step for '{}': {}",
                        execution.plugin.name, command
                    )
                })?;
            if !status.success() {
                anyhow::bail!(
                    "{} step failed for '{}': {}",
                    hook_kind,
                    execution.plugin.name,
                    command
                );
            }
        }
        Ok(())
    }

    fn warn_if_binary_on_path_is_external(
        &self,
        plugin: &Plugin,
        binary_filename: &str,
    ) -> Result<()> {
        let Some(resolved_path) = first_binary_on_path(binary_filename) else {
            return Ok(());
        };
        let managed_path = self.local_bin.join(binary_filename);
        if resolved_path == managed_path {
            return Ok(());
        }

        let tracked = self.load_state()?.installed.iter().any(|package| {
            package.name == plugin.name && package.binary == binary_filename
        });
        if tracked || managed_path.exists() {
            eprintln!(
                "warning: '{}' currently resolves to '{}' instead of the scpr-managed path '{}'. Installing or updating '{}' may not change the binary your shell runs until PATH is reordered.",
                binary_filename,
                resolved_path.display(),
                managed_path.display(),
                plugin.name
            );
        } else {
            eprintln!(
                "warning: '{}' already exists on PATH at '{}' and is not managed by scpr. Installing '{}' into '{}' may not change the binary your shell runs until PATH is reordered.",
                binary_filename,
                resolved_path.display(),
                plugin.name,
                managed_path.display()
            );
        }
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
        self.run_hook_commands(
            plugin.post_install.as_deref(),
            plugin,
            binary_filename,
            dry_run,
            "post-install",
        )
    }

    fn run_managed_completion_install(
        &self,
        plugin: &Plugin,
        binary_filename: &str,
        dry_run: bool,
    ) -> Result<()> {
        let Some(completions) = plugin.completions.as_ref() else {
            return Ok(());
        };

        let shell = env::var("SHELL")
            .ok()
            .and_then(|value| ManagedCompletionShell::from_env(&value));
        let Some(shell) = shell else {
            return Ok(());
        };

        let home = dirs::home_dir().context(
            "Failed to determine home directory for managed completion installation",
        )?;
        let managed_file = shell.managed_file_path(&home, &plugin.name);
        let command = self.expand_command_template(
            &completions.command,
            plugin,
            binary_filename,
            Some(shell.as_str()),
        );

        if dry_run {
            println!(
                "[dry-run] Would generate {} completion for '{}' with: {}",
                shell.as_str(),
                plugin.name,
                command
            );
            println!(
                "[dry-run] Would write managed completion file {}",
                managed_file.display()
            );
            if let (Some(profile_path), Some(source_line)) =
                (shell.profile_path(&home), shell.source_line(&plugin.name))
            {
                let should_append = match fs::read_to_string(&profile_path) {
                    Ok(contents) => {
                        !shell.profile_loads_drop_in(&contents)
                            && !contents.lines().any(|line| line == source_line)
                    }
                    Err(err) if err.kind() == ErrorKind::NotFound => true,
                    Err(_) => true,
                };
                if should_append {
                    println!(
                        "[dry-run] Would ensure {} sources managed completion file",
                        profile_path.display()
                    );
                }
            }
            return Ok(());
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .with_context(|| {
                format!(
                    "Failed to generate {} completion for '{}': {}",
                    shell.as_str(),
                    plugin.name,
                    command
                )
            })?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to generate {} completion for '{}': {}",
                shell.as_str(),
                plugin.name,
                command
            );
        }

        self.install_managed_completion(shell, plugin, &home, &output.stdout)
    }

    fn run_post_uninstall_hooks(
        &self,
        plugin: &Plugin,
        binary_filename: &str,
        dry_run: bool,
    ) -> Result<()> {
        self.run_hook_commands(
            plugin.post_uninstall.as_deref(),
            plugin,
            binary_filename,
            dry_run,
            "post-uninstall",
        )
    }

    fn run_managed_completion_uninstall(
        &self,
        plugin: &Plugin,
        dry_run: bool,
    ) -> Result<()> {
        if plugin.completions.is_none() {
            return Ok(());
        }

        let home = dirs::home_dir().context(
            "Failed to determine home directory for managed completion uninstall",
        )?;

        for shell in [
            ManagedCompletionShell::Bash,
            ManagedCompletionShell::Zsh,
            ManagedCompletionShell::Fish,
        ] {
            let managed_file = shell.managed_file_path(&home, &plugin.name);
            if dry_run {
                println!(
                    "[dry-run] Would remove managed completion file {}",
                    managed_file.display()
                );
            } else if managed_file.exists() {
                fs::remove_file(&managed_file).with_context(|| {
                    format!(
                        "Failed to remove managed completion file {}",
                        managed_file.display()
                    )
                })?;
            }

            if let (Some(profile_path), Some(source_line)) =
                (shell.profile_path(&home), shell.source_line(&plugin.name))
            {
                if dry_run {
                    println!(
                        "[dry-run] Would remove managed completion source line from {}",
                        profile_path.display()
                    );
                    continue;
                }
                remove_line_from_file(&profile_path, &source_line)?;
            }
        }

        Ok(())
    }

    fn run_cleanup_hooks(
        &self,
        plugin: &Plugin,
        binary_filename: &str,
        dry_run: bool,
    ) -> Result<()> {
        let Some(hooks) = plugin.cleanup.as_deref() else {
            return Ok(());
        };

        if dry_run {
            println!(
                "[dry-run] Would prompt to remove user-generated data for '{}'",
                plugin.name
            );
            return self.run_hook_commands(
                Some(hooks),
                plugin,
                binary_filename,
                true,
                "cleanup",
            );
        }

        if !self.confirm_cleanup(plugin)? {
            println!("Skipped cleanup hooks for '{}'.", plugin.name);
            return Ok(());
        }

        self.run_hook_commands(Some(hooks), plugin, binary_filename, false, "cleanup")
    }

    fn run_hook_commands(
        &self,
        hooks: Option<&[String]>,
        plugin: &Plugin,
        binary_filename: &str,
        dry_run: bool,
        hook_kind: &str,
    ) -> Result<()> {
        let Some(hooks) = hooks else {
            return Ok(());
        };
        for hook in hooks {
            let command = self.expand_command_template(hook, plugin, binary_filename, None);
            if dry_run {
                println!("[dry-run] Would run {hook_kind} hook: {command}");
                continue;
            }
            let status = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .status()
                .with_context(|| {
                    format!(
                        "Failed to execute {hook_kind} hook for '{}': {}",
                        plugin.name, command
                    )
                })?;
            if !status.success() {
                anyhow::bail!(
                    "{} hook failed for '{}': {}",
                    hook_kind,
                    plugin.name,
                    command
                );
            }
        }
        Ok(())
    }

    fn install_managed_completion(
        &self,
        shell: ManagedCompletionShell,
        plugin: &Plugin,
        home: &Path,
        script_contents: &[u8],
    ) -> Result<()> {
        let managed_file = shell.managed_file_path(home, &plugin.name);
        if let Some(parent) = managed_file.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create completion directory {}",
                    parent.display()
                )
            })?;
        }
        fs::write(&managed_file, script_contents).with_context(|| {
            format!(
                "Failed to write managed completion file {}",
                managed_file.display()
            )
        })?;

        if let (Some(profile_path), Some(source_line)) =
            (shell.profile_path(home), shell.source_line(&plugin.name))
        {
            let profile_contents = match fs::read_to_string(&profile_path) {
                Ok(contents) => contents,
                Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "Failed to read shell profile {}",
                            profile_path.display()
                        )
                    });
                }
            };

            if !shell.profile_loads_drop_in(&profile_contents) {
                append_line_if_missing(&profile_path, &source_line)?;
            }
        }

        Ok(())
    }

    fn expand_command_template(
        &self,
        template: &str,
        plugin: &Plugin,
        binary_filename: &str,
        shell: Option<&str>,
    ) -> String {
        let binary_path = self.local_bin.join(binary_filename);
        let command = template
            .replace("{binary_path}", &binary_path.display().to_string())
            .replace("{binary_name}", binary_filename)
            .replace("{plugin}", &plugin.name)
            .replace("{install_dir}", &self.local_bin.display().to_string())
            .replace("{man_dir}", &self.local_man.display().to_string());

        match shell {
            Some(shell) => command.replace("{shell}", shell),
            None => command,
        }
    }

    fn confirm_cleanup(&self, plugin: &Plugin) -> Result<bool> {
        if !io::stdin().is_terminal() {
            println!(
                "Skipping cleanup hooks for '{}': confirmation requires an interactive terminal.",
                plugin.name
            );
            return Ok(false);
        }

        let stdin = io::stdin();
        let mut input = stdin.lock();
        let stdout = io::stdout();
        let mut output = stdout.lock();
        prompt_cleanup_confirmation(&mut input, &mut output, &plugin.name)
    }

    fn same_version_already_installed(
        &self,
        plugin_name: &str,
        version: &str,
    ) -> Result<bool> {
        let installed =
            self.load_state()?.installed.into_iter().find(|package| {
                package.name == plugin_name && package.version == version
            });
        let Some(package) = installed else {
            return Ok(false);
        };

        Ok(self.local_bin.join(package.binary).exists())
    }

    fn capture_binary_audit_baseline(&self, binary_path: &Path) -> Result<AuditBaseline> {
        let metadata = fs::metadata(binary_path).with_context(|| {
            format!(
                "Failed to inspect installed binary {}",
                binary_path.display()
            )
        })?;
        Ok(audit_baseline_from_metadata(&metadata))
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

    pub fn audit(&self, describe: bool) -> Result<Vec<AuditRecord>> {
        let installed = self.load_state()?.installed;
        let mut records = Vec::new();

        for pkg in installed {
            let binary_path = self.local_bin.join(&pkg.binary);
            let Some(expected) = expected_binary_checksum(&pkg) else {
                let detail = audit_untracked_detail(&pkg);
                let describe_record = describe.then(|| AuditDescription {
                    fingerprint: AuditAspect {
                        status: "unknown".to_string(),
                        expected: None,
                        actual: None,
                    },
                    owner: describe_owner_aspect(None, None),
                    permissions: describe_permissions_aspect(None, None),
                    size_bytes: fs::metadata(&binary_path)
                        .ok()
                        .map(|metadata| metadata.len()),
                });
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Untracked,
                    expected_checksum: pkg.binary_checksum_sha256.clone(),
                    actual_checksum: None,
                    detail,
                    describe: describe_record,
                });
                continue;
            };

            if !binary_path.exists() {
                let describe_record = describe.then(|| AuditDescription {
                    fingerprint: AuditAspect {
                        status: "missing".to_string(),
                        expected: Some(expected.clone()),
                        actual: None,
                    },
                    owner: describe_owner_aspect(
                        pkg.binary_owner_uid.zip(pkg.binary_owner_gid),
                        None,
                    ),
                    permissions: describe_permissions_aspect(pkg.binary_mode, None),
                    size_bytes: None,
                });
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Missing,
                    expected_checksum: Some(expected),
                    actual_checksum: None,
                    detail: "Installed binary is missing".to_string(),
                    describe: describe_record,
                });
                continue;
            }

            let metadata = fs::metadata(&binary_path).with_context(|| {
                format!(
                    "Failed to inspect installed binary {}",
                    binary_path.display()
                )
            })?;
            let data = fs::read(&binary_path).with_context(|| {
                format!("Failed to read installed binary {}", binary_path.display())
            })?;
            let actual = installer_archive::sha256_hex(&data);
            let actual_baseline = audit_baseline_from_metadata(&metadata);
            let describe_record = describe.then(|| AuditDescription {
                fingerprint: describe_fingerprint_aspect(&expected, &actual),
                owner: describe_owner_aspect(
                    pkg.binary_owner_uid.zip(pkg.binary_owner_gid),
                    actual_baseline.owner_uid.zip(actual_baseline.owner_gid),
                ),
                permissions: describe_permissions_aspect(
                    pkg.binary_mode,
                    actual_baseline.mode,
                ),
                size_bytes: Some(metadata.len()),
            });
            if actual == expected {
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Ok,
                    expected_checksum: Some(expected),
                    actual_checksum: Some(actual),
                    detail: "Binary matches the recorded SHA-256 checksum".to_string(),
                    describe: describe_record,
                });
            } else {
                records.push(AuditRecord {
                    package: pkg.name,
                    binary_path,
                    status: AuditStatus::Modified,
                    expected_checksum: Some(expected),
                    actual_checksum: Some(actual),
                    detail: "Binary contents have changed since installation".to_string(),
                    describe: describe_record,
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

fn append_line_if_missing(path: &Path, line: &str) -> Result<()> {
    let mut contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to read {}", path.display()));
        }
    };

    if contents.lines().any(|existing| existing == line) {
        return Ok(());
    }

    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(line);
    contents.push('\n');

    fs::write(path, contents)
        .with_context(|| format!("Failed to update {}", path.display()))
}

fn remove_line_from_file(path: &Path, line: &str) -> Result<()> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to read {}", path.display()));
        }
    };

    let had_trailing_newline = contents.ends_with('\n');
    let retained = contents
        .lines()
        .filter(|existing| *existing != line)
        .collect::<Vec<_>>();

    let mut updated = retained.join("\n");
    if had_trailing_newline && !updated.is_empty() {
        updated.push('\n');
    }

    fs::write(path, updated)
        .with_context(|| format!("Failed to update {}", path.display()))
}

fn resolve_release_target(
    plugin: &Plugin,
    os: &str,
    arch: &str,
    target_override: Option<&str>,
) -> Result<String> {
    match target_override {
        Some(target) => Ok(target.to_string()),
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
        }),
    }
}

fn resolve_optional_target(
    plugin: &Plugin,
    os: &str,
    arch: &str,
    target_override: Option<&str>,
) -> Option<String> {
    target_override
        .map(str::to_string)
        .or_else(|| plugin.resolve_target(os, arch))
}

async fn resolve_build_ref(
    plugin: &Plugin,
    client: &GithubClient,
    owner: &str,
    repo: &str,
    cli_tag: Option<&str>,
) -> Result<BuildRefResolution> {
    if let Some(source_ref) = cli_tag.or(plugin.build_branch.as_deref()) {
        let sha = client.resolve_commit_sha(owner, repo, source_ref).await?;
        return Ok(BuildRefResolution {
            source_ref: source_ref.to_string(),
            template_tag: source_ref.to_string(),
            version_label: format_build_version(source_ref, &sha),
        });
    }

    let release = client.get_latest_release(owner, repo).await?;
    Ok(BuildRefResolution {
        source_ref: release.tag_name.clone(),
        template_tag: release.tag_name.clone(),
        version_label: release.tag_name,
    })
}

fn format_build_version(source_ref: &str, sha: &str) -> String {
    let short_sha: String = sha.chars().take(12).collect();
    format!("{source_ref}@{short_sha}")
}

fn expand_build_template(
    plugin: &Plugin,
    template: &str,
    tag: &str,
    target: Option<&str>,
) -> Result<String> {
    if template.contains("{target}") && target.is_none() {
        anyhow::bail!(
            "Plugin '{}' uses '{{target}}' in build metadata but does not define a target for this platform. Add [plugin.targets] or pass --target.",
            plugin.name
        );
    }

    Ok(plugin.expand_template(template, tag, target.unwrap_or("")))
}

fn expand_build_command(
    plugin: &Plugin,
    command: &str,
    tag: &str,
    target: Option<&str>,
    repo_dir: &Path,
    binary_path: &Path,
) -> Result<String> {
    let command = expand_build_template(plugin, command, tag, target)?;
    Ok(command
        .replace("{repo_dir}", &repo_dir.display().to_string())
        .replace("{binary_path}", &binary_path.display().to_string())
        .replace(
            "{binary_name}",
            binary_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
        .replace("{plugin}", &plugin.name)
        .replace("{tag}", tag)
        .replace("{target}", target.unwrap_or("")))
}

fn extract_github_tarball(data: &[u8], destination: &Path) -> Result<PathBuf> {
    let decoder = GzDecoder::new(std::io::Cursor::new(data));
    let mut archive = Archive::new(decoder);
    archive.unpack(destination).with_context(|| {
        format!(
            "Failed to extract source archive into {}",
            destination.display()
        )
    })?;

    let mut entries = fs::read_dir(destination)
        .with_context(|| {
            format!(
                "Failed to inspect extracted source tree in {}",
                destination.display()
            )
        })?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            anyhow!(
                "Extracted source archive did not contain a repository root directory"
            )
        })
}

fn infer_build_commands(repo_dir: &Path, target: Option<&str>) -> Result<Vec<String>> {
    if repo_dir.join("Cargo.toml").exists() {
        let command = match target {
            Some(target) => format!("cargo build --release --target {target}"),
            None => "cargo build --release".to_string(),
        };
        return Ok(vec![command]);
    }
    if repo_dir.join("CMakeLists.txt").exists() {
        return Ok(vec![
            "cmake -S . -B build -DCMAKE_BUILD_TYPE=Release".to_string(),
            "cmake --build build --config Release".to_string(),
        ]);
    }
    if repo_dir.join("Makefile").exists() || repo_dir.join("makefile").exists() {
        return Ok(vec!["make".to_string()]);
    }

    anyhow::bail!(
        "Could not infer how to build the source tree at {}. Add plugin.build_script for this plugin.",
        repo_dir.display()
    )
}

fn first_binary_on_path(binary_name: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .and_then(|paths| first_binary_in_dirs(binary_name, paths))
}

fn first_binary_in_dirs<I>(binary_name: &str, paths: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    paths.into_iter().find_map(|dir| {
        let candidate = dir.join(binary_name);
        is_executable_file(&candidate).then_some(candidate)
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn prompt_cleanup_confirmation<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    plugin_name: &str,
) -> Result<bool> {
    loop {
        write!(
            output,
            "Remove user-generated data for '{}' as well? [y/N]: ",
            plugin_name
        )?;
        output.flush()?;

        let mut line = String::new();
        let read = input.read_line(&mut line)?;
        if read == 0 {
            writeln!(output)?;
            return Ok(false);
        }

        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => {
                writeln!(output, "Please answer 'y' or 'n'.")?;
            }
        }
    }
}

fn describe_fingerprint_aspect(expected: &str, actual: &str) -> AuditAspect {
    AuditAspect {
        status: if actual == expected {
            "unchanged"
        } else {
            "changed"
        }
        .to_string(),
        expected: Some(expected.to_string()),
        actual: Some(actual.to_string()),
    }
}

fn expected_binary_checksum(package: &InstalledPackage) -> Option<String> {
    if let Some(binary_checksum) = package.binary_checksum_sha256.clone() {
        return Some(binary_checksum);
    }

    let checksum = package.checksum_sha256.clone()?;
    if package
        .asset_name
        .as_deref()
        .is_some_and(asset_name_looks_like_archive)
    {
        return None;
    }

    Some(checksum)
}

fn audit_untracked_detail(package: &InstalledPackage) -> String {
    if package.binary_checksum_sha256.is_none()
        && package
            .asset_name
            .as_deref()
            .is_some_and(asset_name_looks_like_archive)
        && package.checksum_sha256.is_some()
    {
        return "Legacy install record stores the archive checksum, not the extracted binary checksum; reinstall with --force to refresh audit metadata"
            .to_string();
    }

    "No stored checksum; cannot verify local changes".to_string()
}

fn asset_name_looks_like_archive(asset_name: &str) -> bool {
    [
        ".tar.gz",
        ".tgz",
        ".tar.xz",
        ".txz",
        ".tar.zst",
        ".tar.zstd",
        ".tar.bz2",
        ".tbz2",
        ".zip",
        ".gz",
    ]
    .iter()
    .any(|suffix| asset_name.ends_with(suffix))
}

fn describe_owner_aspect(
    expected: Option<(u32, u32)>,
    actual: Option<(u32, u32)>,
) -> Option<AuditAspect> {
    match (expected, actual) {
        (Some(expected), Some(actual)) => Some(AuditAspect {
            status: if expected == actual {
                "unchanged"
            } else {
                "changed"
            }
            .to_string(),
            expected: Some(format!("{}:{}", expected.0, expected.1)),
            actual: Some(format!("{}:{}", actual.0, actual.1)),
        }),
        (Some(expected), None) => Some(AuditAspect {
            status: "missing".to_string(),
            expected: Some(format!("{}:{}", expected.0, expected.1)),
            actual: None,
        }),
        (None, Some(actual)) => Some(AuditAspect {
            status: "unknown".to_string(),
            expected: None,
            actual: Some(format!("{}:{}", actual.0, actual.1)),
        }),
        (None, None) => None,
    }
}

fn describe_permissions_aspect(
    expected: Option<u32>,
    actual: Option<u32>,
) -> Option<AuditAspect> {
    match (expected, actual) {
        (Some(expected), Some(actual)) => Some(AuditAspect {
            status: if expected == actual {
                "unchanged"
            } else {
                "changed"
            }
            .to_string(),
            expected: Some(format!("{:o}", expected)),
            actual: Some(format!("{:o}", actual)),
        }),
        (Some(expected), None) => Some(AuditAspect {
            status: "missing".to_string(),
            expected: Some(format!("{:o}", expected)),
            actual: None,
        }),
        (None, Some(actual)) => Some(AuditAspect {
            status: "unknown".to_string(),
            expected: None,
            actual: Some(format!("{:o}", actual)),
        }),
        (None, None) => None,
    }
}

fn audit_baseline_from_metadata(metadata: &fs::Metadata) -> AuditBaseline {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        AuditBaseline {
            mode: Some(metadata.permissions().mode() & 0o7777),
            owner_uid: Some(metadata.uid()),
            owner_gid: Some(metadata.gid()),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = metadata;
        AuditBaseline::default()
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
