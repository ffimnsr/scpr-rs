use crate::{
    github::GithubClient,
    plugin::Plugin,
};
use anyhow::{Context, Result, anyhow};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, Cursor},
    path::{Path, PathBuf},
};
use tar::Archive;
use tracing::{debug, info, warn};

// ── State ──────────────────────────────────────────────────────────────────

/// Record of a single installed package, persisted in
/// `~/.local/share/scarper/state.toml`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    /// Filename of the installed binary (just the name, not the full path).
    pub binary: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    installed: Vec<InstalledPackage>,
}

// ── Installer ──────────────────────────────────────────────────────────────

/// Installs and uninstalls GitHub-release binaries into the user's local
/// directories (`~/.local/bin`, `~/.local/share/man`).
pub struct Installer {
    /// `~/.local/bin`
    local_bin: PathBuf,
    /// `~/.local/share/man/man1`
    local_man: PathBuf,
    /// `~/.local/share/scarper/state.toml`
    state_file: PathBuf,
}

impl Installer {
    /// Create a new [`Installer`], ensuring all required directories exist.
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir().context("Failed to determine home directory")?;
        let local_bin = home.join(".local/bin");
        let local_man = home.join(".local/share/man/man1");
        let state_dir = home.join(".local/share/scarper");
        let state_file = state_dir.join("state.toml");

        fs::create_dir_all(&local_bin)
            .with_context(|| format!("Failed to create {}", local_bin.display()))?;
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("Failed to create {}", state_dir.display()))?;

        Ok(Self {
            local_bin,
            local_man,
            state_file,
        })
    }

    // ── State helpers ──────────────────────────────────────────────────────

    fn load_state(&self) -> Result<State> {
        if !self.state_file.exists() {
            return Ok(State::default());
        }
        let content =
            fs::read_to_string(&self.state_file).context("Failed to read state file")?;
        toml::from_str(&content).context("Failed to parse state file")
    }

    fn save_state(&self, state: &State) -> Result<()> {
        let content = toml::to_string(state).context("Failed to serialize state")?;
        fs::write(&self.state_file, content).context("Failed to write state file")
    }

    // ── Public API ─────────────────────────────────────────────────────────

    /// Return all currently installed packages.
    pub fn list_installed(&self) -> Result<Vec<InstalledPackage>> {
        Ok(self.load_state()?.installed)
    }

    /// Return `true` if a package with the given name is already installed.
    pub fn is_installed(&self, name: &str) -> Result<bool> {
        Ok(self.load_state()?.installed.iter().any(|p| p.name == name))
    }

    /// Download and install the latest release of `plugin` from GitHub.
    ///
    /// If the package is already installed its binary and man pages are
    /// overwritten with the new version.
    pub async fn install(&self, plugin: &Plugin, client: &GithubClient) -> Result<()> {
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

        let target = plugin.resolve_target(os, arch).ok_or_else(|| {
            anyhow!(
                "No target triple defined for {os}/{arch} in plugin '{}'",
                plugin.name
            )
        })?;
        debug!("Resolved target: {target}");

        // Fetch the latest release tag and asset list.
        let release = client.get_latest_release(owner, repo).await?;
        let tag = &release.tag_name;
        info!("Latest release: {tag}");

        // Resolve asset filename and all in-archive paths.
        let asset_name = plugin.expand_template(&plugin.asset_pattern, tag, &target);
        let binary_path = plugin.expand_template(&plugin.binary, tag, &target);
        let man_paths: Vec<String> = plugin
            .man_pages
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|t| plugin.expand_template(t, tag, &target))
            .collect();

        debug!("Asset: {asset_name}");
        debug!("Binary path in archive: {binary_path}");

        // Find the matching release asset.
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| {
                let available = release
                    .assets
                    .iter()
                    .map(|a| a.name.as_str())
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

        // Derive the binary destination name (last path component).
        let binary_filename = Path::new(&binary_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&plugin.name);
        let binary_dest = self.local_bin.join(binary_filename);

        // Extract everything in a single pass.
        if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
            self.extract_from_targz(&data, &binary_path, &binary_dest, &man_paths)?;
        } else if asset_name.ends_with(".zip") {
            self.extract_from_zip(&data, &binary_path, &binary_dest, &man_paths)?;
        } else {
            // Assume the asset itself is the binary.
            fs::write(&binary_dest, &data).with_context(|| {
                format!("Failed to write binary to {}", binary_dest.display())
            })?;
        }

        // Mark executable on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&binary_dest)
                .with_context(|| format!("Failed to stat {}", binary_dest.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_dest, perms)?;
        }

        info!(
            "✓ Installed '{}' → {}",
            binary_filename,
            binary_dest.display()
        );

        // Persist state.
        let mut state = self.load_state()?;
        state.installed.retain(|p| p.name != plugin.name);
        state.installed.push(InstalledPackage {
            name: plugin.name.clone(),
            version: tag.clone(),
            binary: binary_filename.to_string(),
        });
        self.save_state(&state)?;

        Ok(())
    }

    /// Remove an installed package and its man pages.
    pub fn uninstall(&self, plugin: &Plugin) -> Result<()> {
        let state = self.load_state()?;
        let package = state
            .installed
            .iter()
            .find(|p| p.name == plugin.name)
            .ok_or_else(|| anyhow!("'{}' is not installed", plugin.name))?
            .clone();

        // Remove the binary.
        let binary_dest = self.local_bin.join(&package.binary);
        if binary_dest.exists() {
            fs::remove_file(&binary_dest).with_context(|| {
                format!("Failed to remove {}", binary_dest.display())
            })?;
            info!("Removed {}", binary_dest.display());
        }

        // Remove man pages (best-effort; missing files are not an error).
        if let Some(man_page_templates) = &plugin.man_pages {
            for template in man_page_templates {
                // Extract just the filename from the template (e.g. "rg.1" from
                // "{name}-{version}-{target}/doc/rg.1").
                let filename = template
                    .split('/')
                    .last()
                    .unwrap_or(template.as_str())
                    .to_string();
                let man_dest = self.local_man.join(&filename);
                if man_dest.exists() {
                    if let Err(e) = fs::remove_file(&man_dest) {
                        warn!("Failed to remove man page {}: {e}", man_dest.display());
                    } else {
                        info!("Removed {}", man_dest.display());
                    }
                }
            }
        }

        // Update state.
        let mut state = self.load_state()?;
        state.installed.retain(|p| p.name != plugin.name);
        self.save_state(&state)?;

        info!("✓ Uninstalled '{}'", plugin.name);
        Ok(())
    }

    // ── Archive extraction ─────────────────────────────────────────────────

    /// Extract `binary_path` and all `man_paths` from a `.tar.gz` archive in a
    /// single sequential pass.
    fn extract_from_targz(
        &self,
        data: &[u8],
        binary_path: &str,
        binary_dest: &Path,
        man_paths: &[String],
    ) -> Result<()> {
        let gz = GzDecoder::new(Cursor::new(data));
        let mut archive = Archive::new(gz);
        self.extract_from_tar(archive.entries()?, binary_path, binary_dest, man_paths)
    }

    /// Inner helper shared by all tar variants.
    fn extract_from_tar<R: io::Read>(
        &self,
        entries: tar::Entries<'_, R>,
        binary_path: &str,
        binary_dest: &Path,
        man_paths: &[String],
    ) -> Result<()> {
        let mut binary_installed = false;
        let mut man_pages_left: HashSet<&str> = man_paths.iter().map(String::as_str).collect();

        for entry in entries {
            let mut entry = entry.context("Failed to read tar entry")?;
            let path = entry.path().context("Failed to get tar entry path")?;
            let path_str = path.to_string_lossy().to_string();

            if !binary_installed && path_str == binary_path {
                debug!("Extracting binary: {path_str}");
                let mut dest_file = fs::File::create(binary_dest).with_context(|| {
                    format!("Failed to create {}", binary_dest.display())
                })?;
                io::copy(&mut entry, &mut dest_file)
                    .context("Failed to write binary from archive")?;
                binary_installed = true;
            } else if man_pages_left.contains(path_str.as_str()) {
                let man_filename = Path::new(&path_str)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| anyhow!("Invalid man page path: {path_str}"))?;
                fs::create_dir_all(&self.local_man).with_context(|| {
                    format!("Failed to create {}", self.local_man.display())
                })?;
                let dest = self.local_man.join(man_filename);
                let mut dest_file = fs::File::create(&dest)
                    .with_context(|| format!("Failed to create {}", dest.display()))?;
                io::copy(&mut entry, &mut dest_file)
                    .context("Failed to write man page from archive")?;
                info!("Installed man page → {}", dest.display());
                man_pages_left.remove(path_str.as_str());
            }

            if binary_installed && man_pages_left.is_empty() {
                break;
            }
        }

        if !binary_installed {
            return Err(anyhow!("Binary '{binary_path}' not found in archive"));
        }

        for missing in &man_pages_left {
            warn!("Man page '{missing}' not found in archive, skipping");
        }

        Ok(())
    }

    /// Extract `binary_path` and all `man_paths` from a `.zip` archive.
    fn extract_from_zip(
        &self,
        data: &[u8],
        binary_path: &str,
        binary_dest: &Path,
        man_paths: &[String],
    ) -> Result<()> {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(data)).context("Failed to open zip archive")?;

        // Extract binary.
        {
            let mut entry = archive
                .by_name(binary_path)
                .with_context(|| format!("Binary '{binary_path}' not found in zip archive"))?;
            let mut dest_file = fs::File::create(binary_dest).with_context(|| {
                format!("Failed to create {}", binary_dest.display())
            })?;
            io::copy(&mut entry, &mut dest_file)
                .context("Failed to write binary from zip archive")?;
        }

        // Extract man pages.
        for man_path in man_paths {
            match archive.by_name(man_path) {
                Ok(mut entry) => {
                    let man_filename = Path::new(man_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| anyhow!("Invalid man page path: {man_path}"))?;
                    fs::create_dir_all(&self.local_man).with_context(|| {
                        format!("Failed to create {}", self.local_man.display())
                    })?;
                    let dest = self.local_man.join(man_filename);
                    let mut dest_file = fs::File::create(&dest)
                        .with_context(|| format!("Failed to create {}", dest.display()))?;
                    io::copy(&mut entry, &mut dest_file)
                        .context("Failed to write man page from zip archive")?;
                    info!("Installed man page → {}", dest.display());
                }
                Err(_) => {
                    warn!("Man page '{man_path}' not found in archive, skipping");
                }
            }
        }

        Ok(())
    }
}
