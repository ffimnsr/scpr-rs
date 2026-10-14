use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::debug;
use walkdir::{DirEntry, WalkDir};

static EMITTED_PLUGIN_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Plugin definition loaded from a TOML file.
///
/// Example plugin file (`plugins/ripgrep.toml`):
/// ```toml
/// [plugin]
/// name = "ripgrep"
/// alias = ["rg", "ripgrep"]
/// description = "A fast line-oriented search tool"
/// location = "github:BurntSushi/ripgrep"
/// asset_pattern = "{name}-{version}-{target}.tar.gz"
/// binary = "{name}-{version}-{target}/rg"
/// man_pages = ["{name}-{version}-{target}/doc/rg.1"]
///
/// [plugin.targets]
/// "linux-x86_64" = "x86_64-unknown-linux-musl"
/// "macos-aarch64" = "aarch64-apple-darwin"
/// ```
///
/// Template placeholders supported in `asset_pattern`, `binary`, and `man_pages`:
/// - `{name}`    — plugin name (e.g. "ripgrep")
/// - `{version}` — release version with leading `v` stripped (e.g. "14.1.0")
/// - `{tag}`     — release tag as returned by GitHub (e.g. "14.1.0" or "v1.2.0")
/// - `{target}`  — platform target triple resolved from `[plugin.targets]`
#[derive(Debug, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct PluginCompletions {
    /// Shell command template that prints completion script content to stdout.
    ///
    /// Supported placeholders match hook templates plus `{shell}`.
    pub command: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct PluginFile {
    /// Path inside the release archive (or the source checkout when `--build`
    /// is used). When it names a directory, the whole tree is installed.
    /// Supports the standard template placeholders.
    pub from: String,
    /// Destination path. Must start with `{install_dir}`, `{data_home}`, or
    /// `{config_home}` followed by a relative path. Supports the standard
    /// template placeholders.
    pub to: String,
    /// Force the executable bit on the installed files.
    #[serde(default)]
    pub executable: bool,
}

/// Destination root for a [`PluginFile`] mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRoot {
    /// The scpr-managed binary directory (`~/.local/bin` by default).
    InstallDir,
    /// The XDG data home (`~/.local/share` by default).
    DataHome,
    /// The XDG config home (`~/.config` by default).
    ConfigHome,
}

impl FileRoot {
    /// Stable key persisted in the installer state file.
    pub fn key(self) -> &'static str {
        match self {
            Self::InstallDir => "install_dir",
            Self::DataHome => "data_home",
            Self::ConfigHome => "config_home",
        }
    }

    /// Parse a key produced by [`FileRoot::key`].
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "install_dir" => Some(Self::InstallDir),
            "data_home" => Some(Self::DataHome),
            "config_home" => Some(Self::ConfigHome),
            _ => None,
        }
    }
}

/// A [`PluginFile`] with templates expanded and its destination split into a
/// root and a relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPluginFile {
    pub from: String,
    pub root: FileRoot,
    pub relative_path: PathBuf,
    pub executable: bool,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Plugin {
    pub name: String,
    pub alias: Vec<String>,
    pub description: Option<String>,
    /// GitHub location in the form `github:<owner>/<repo>`.
    pub location: String,
    /// Asset filename pattern with template placeholders.
    #[serde(default)]
    pub asset_pattern: String,
    /// Optional checksum asset filename pattern used when release metadata
    /// does not expose an asset digest directly.
    pub checksum_asset_pattern: Option<String>,
    /// Explicit opt-in for releases that ship no SHA-256 metadata at all.
    #[serde(default)]
    pub allow_insecure_no_checksum: bool,
    /// Optional signature asset filename pattern for release verification.
    pub signature_asset_pattern: Option<String>,
    /// Signature verification backend: `gpg` or `minisign`.
    pub signature_format: Option<String>,
    /// Optional key material or key identifier for signature verification.
    pub signature_key: Option<String>,
    /// Optional source ref for `--build`; accepts a branch name or tag.
    pub build_branch: Option<String>,
    /// Optional commands to build the project from source when `--build` is used.
    pub build_script: Option<Vec<String>>,
    /// Optional commands to run after source builds complete.
    pub post_build: Option<Vec<String>>,
    /// Path to the binary within the extracted archive.
    pub binary: String,
    /// Paths to man pages within the extracted archive.
    pub man_pages: Option<Vec<String>>,
    /// Additional files or directory trees to install from the archive (or
    /// source checkout when `--build` is used), beyond `binary`/`man_pages`.
    #[serde(default)]
    pub files: Vec<PluginFile>,
    /// Optional managed shell completion metadata.
    pub completions: Option<PluginCompletions>,
    /// Optional commands to run after installation.
    pub post_install: Option<Vec<String>>,
    /// Optional commands to run after uninstalling tracked files.
    pub post_uninstall: Option<Vec<String>>,
    /// Optional commands to remove user-generated data after confirmation.
    pub cleanup: Option<Vec<String>>,
    /// Map from `<os>-<arch>` key to the target triple used in release asset names.
    pub targets: Option<HashMap<String, String>>,
}

impl Plugin {
    /// Parse the `github:<owner>/<repo>` location and return `(owner, repo)`.
    pub fn github_repo(&self) -> Option<(&str, &str)> {
        let loc = self.location.strip_prefix("github:")?;
        let (owner, repo) = loc.split_once('/')?;
        Some((owner, repo))
    }

    /// Look up the platform target triple for the given OS and architecture.
    pub fn resolve_target(&self, os: &str, arch: &str) -> Option<String> {
        let key = format!("{os}-{arch}");
        self.targets.as_ref()?.get(&key).cloned()
    }

    pub fn available_target_keys(&self) -> Vec<String> {
        let mut keys = self
            .targets
            .as_ref()
            .map(|targets| targets.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort();
        keys
    }

    pub fn signature_format_name(&self) -> Option<&str> {
        self.signature_format.as_deref()
    }

    /// Expand a template string replacing all supported placeholders.
    ///
    /// `tag` is the raw GitHub tag (e.g. "14.1.0" or "v1.2.0");
    /// `{version}` is always the tag with a leading `v` stripped.
    pub fn expand_template(&self, template: &str, tag: &str, target: &str) -> String {
        let version = tag.strip_prefix('v').unwrap_or(tag);
        template
            .replace("{name}", &self.name)
            .replace("{tag}", tag)
            .replace("{version}", version)
            .replace("{target}", target)
    }

    /// Expand the `files` mappings declared by this plugin.
    ///
    /// Fails when a `from` path is empty or escapes the archive root, when a
    /// `to` path does not start with a supported destination root, or when the
    /// destination escapes that root.
    pub fn resolve_files(
        &self,
        tag: &str,
        target: &str,
    ) -> Result<Vec<ResolvedPluginFile>> {
        self.files
            .iter()
            .map(|file| {
                let from = self.expand_template(&file.from, tag, target);
                let from = from.trim_end_matches('/').to_string();
                if from.trim().is_empty() {
                    return Err(anyhow!(
                        "Plugin '{}' has a files entry with an empty from path",
                        self.name
                    ));
                }
                if path_has_traversal(Path::new(&from)) {
                    return Err(anyhow!(
                        "Plugin '{}' files.from must be a relative path inside the archive: {from}",
                        self.name
                    ));
                }

                let to = self.expand_template(&file.to, tag, target);
                let (root, relative_path) =
                    split_destination_path(&to).ok_or_else(|| {
                        anyhow!(
                            "Plugin '{}' files.to must start with {{install_dir}}, {{data_home}}, or {{config_home}}: {to}",
                            self.name
                        )
                    })?;
                if relative_path.trim().is_empty() {
                    return Err(anyhow!(
                        "Plugin '{}' files.to must include a path below the destination root: {to}",
                        self.name
                    ));
                }
                if path_has_traversal(Path::new(relative_path)) {
                    return Err(anyhow!(
                        "Plugin '{}' files.to must not escape its destination root: {to}",
                        self.name
                    ));
                }

                Ok(ResolvedPluginFile {
                    from,
                    root,
                    relative_path: PathBuf::from(relative_path),
                    executable: file.executable,
                })
            })
            .collect()
    }
}

/// Split an expanded `files.to` value into its destination root and the path
/// below it.
///
/// The placeholder must be followed by `/`, so `{data_home}evil` is rejected
/// rather than silently treated as `{data_home}/evil`.
fn split_destination_path(value: &str) -> Option<(FileRoot, &str)> {
    for (placeholder, root) in [
        ("{install_dir}", FileRoot::InstallDir),
        ("{data_home}", FileRoot::DataHome),
        ("{config_home}", FileRoot::ConfigHome),
    ] {
        if let Some(rest) = value.strip_prefix(placeholder) {
            return Some((root, rest.strip_prefix('/')?));
        }
    }
    None
}

/// Whether a relative path escapes its root (absolute paths, prefixes, or `..`).
pub(crate) fn path_has_traversal(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::RootDir
                | std::path::Component::Prefix(_)
                | std::path::Component::ParentDir
        )
    })
}

#[derive(Deserialize)]
struct PluginContainer {
    plugin: Plugin,
}

/// Parse a plugin TOML file and return the [`Plugin`].
pub fn parse(path: &str) -> Result<Plugin> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read plugin file: {path}"))?;

    debug!("Parsing plugin: {path}");

    let container: PluginContainer = toml::from_str(&content)
        .with_context(|| format!("Failed to parse plugin TOML: {path}"))?;

    Ok(container.plugin)
}

fn is_not_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| entry.depth() == 0 || !s.starts_with('.'))
        .unwrap_or(false)
}

/// Load all plugins from a directory (`.toml` files only, top-level entries).
pub fn load_plugins_from_dir(dir: &str) -> Result<Vec<Plugin>> {
    let dir_path = Path::new(dir);
    if !dir_path.exists() {
        debug!("Plugin directory does not exist, skipping: {dir}");
        return Ok(Vec::new());
    }

    let mut plugins = Vec::new();

    for entry in WalkDir::new(dir_path)
        .max_depth(1)
        .into_iter()
        .filter_entry(is_not_hidden)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::warn!("Failed to read plugin entry in {dir}: {err}");
                continue;
            }
        };

        if entry.file_type().is_dir() {
            continue;
        }

        if !entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext == "toml")
            .unwrap_or(false)
        {
            continue;
        }

        let filename = entry
            .path()
            .to_str()
            .context("Failed to convert path to string")?;

        debug!("Loading plugin from: {filename}");
        match parse(filename) {
            Ok(plugin) => plugins.push(plugin),
            Err(e) => tracing::warn!("Failed to load plugin {filename}: {e}"),
        }
    }

    Ok(plugins)
}

/// Search for a plugin by name or alias across one or more directories.
///
/// Directories are searched in order; the first match wins.
pub fn find_plugin(name: &str, dirs: &[impl AsRef<str>]) -> Result<Plugin> {
    for plugin in load_plugins_from_dirs(dirs)? {
        if plugin.name == name || plugin.alias.iter().any(|a| a == name) {
            if let Some(description) = plugin.description.as_deref() {
                debug!("Matched plugin '{}': {description}", plugin.name);
            }
            return Ok(plugin);
        }
    }

    Err(anyhow!(
        "Plugin '{name}' not found in any plugins directory"
    ))
}

/// Load plugins across one or more directories, deduplicated by plugin name.
pub fn load_plugins_from_dirs(dirs: &[impl AsRef<str>]) -> Result<Vec<Plugin>> {
    let mut load_errors = Vec::new();
    let mut collected_plugins = Vec::new();
    let mut seen = std::collections::HashMap::<String, String>::new();
    let shadowed = collect_shadowed_plugins(dirs)?;

    for dir in dirs {
        match load_plugins_from_dir(dir.as_ref()) {
            Ok(plugins) => {
                for plugin in plugins {
                    if !seen.contains_key(&plugin.name) {
                        seen.insert(plugin.name.clone(), dir.as_ref().to_string());
                        collected_plugins.push(plugin);
                    }
                }
            }
            Err(err) => {
                load_errors.push(format!("{}: {err}", dir.as_ref()));
            }
        }
    }

    if !load_errors.is_empty() {
        return Err(anyhow!(
            "Failed to read plugin directories: {}",
            load_errors.join("; ")
        ));
    }

    emit_shadowed_plugin_warnings_map(shadowed);

    collected_plugins.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(collected_plugins)
}

pub fn emit_shadowed_plugin_warnings(dirs: &[impl AsRef<str>]) -> Result<()> {
    emit_shadowed_plugin_warnings_map(collect_shadowed_plugins(dirs)?);
    Ok(())
}

fn collect_shadowed_plugins(
    dirs: &[impl AsRef<str>],
) -> Result<std::collections::HashMap<(String, String), Vec<String>>> {
    let mut load_errors = Vec::new();
    let mut seen = std::collections::HashMap::<String, String>::new();
    let mut shadowed = std::collections::HashMap::<(String, String), Vec<String>>::new();

    for dir in dirs {
        match load_plugins_from_dir(dir.as_ref()) {
            Ok(plugins) => {
                for plugin in plugins {
                    if let Some(previous_dir) = seen.get(&plugin.name) {
                        shadowed
                            .entry((dir.as_ref().to_string(), previous_dir.clone()))
                            .or_default()
                            .push(plugin.name.clone());
                    } else {
                        seen.insert(plugin.name.clone(), dir.as_ref().to_string());
                    }
                }
            }
            Err(err) => {
                load_errors.push(format!("{}: {err}", dir.as_ref()));
            }
        }
    }

    if !load_errors.is_empty() {
        return Err(anyhow!(
            "Failed to read plugin directories: {}",
            load_errors.join("; ")
        ));
    }

    Ok(shadowed)
}

fn emit_shadowed_plugin_warnings_map(
    shadowed: std::collections::HashMap<(String, String), Vec<String>>,
) {
    let mut shadowed_groups: Vec<_> = shadowed.into_iter().collect();
    shadowed_groups.sort_by(|left, right| left.0.cmp(&right.0));
    for ((current_dir, previous_dir), mut plugin_names) in shadowed_groups {
        plugin_names.sort();
        emit_warning_once(&format_shadowed_plugin_warning(
            &current_dir,
            &previous_dir,
            &plugin_names,
        ));
    }
}

fn format_shadowed_plugin_warning(
    current_dir: &str,
    previous_dir: &str,
    plugin_names: &[String],
) -> String {
    let plugin_label = if plugin_names.len() == 1 {
        "plugin"
    } else {
        "plugins"
    };

    format!(
        "warning: {} {} from '{}' are ignored because an earlier plugin directory takes precedence: '{}'.\n  Shadowed: {}\n  Hint: reorder plugin directories, remove the remote index, or use --plugins-dir if you want local plugins to win.",
        plugin_names.len(),
        plugin_label,
        current_dir,
        previous_dir,
        plugin_names.join(", ")
    )
}

fn emit_warning_once(message: &str) {
    let emitted = EMITTED_PLUGIN_WARNINGS.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut emitted) = emitted.lock() else {
        eprintln!("{message}");
        return;
    };

    if emitted.insert(message.to_string()) {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ripgrep_plugin() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        assert_eq!(plugin.name, "ripgrep");
        assert!(plugin.alias.contains(&"rg".to_string()));
        assert!(plugin.alias.contains(&"ripgrep".to_string()));
        assert_eq!(plugin.location, "github:BurntSushi/ripgrep");
        assert!(plugin.binary.contains("rg"));
        assert!(plugin.description.is_some());
        assert!(plugin.checksum_asset_pattern.is_some());
        assert!(!plugin.allow_insecure_no_checksum);
        assert!(plugin.targets.is_some());
        assert!(plugin.signature_asset_pattern.is_none());
        assert!(plugin.build_branch.is_none());
        assert!(plugin.build_script.is_none());
        assert!(plugin.post_build.is_none());
        assert!(plugin.completions.is_none());
    }

    #[test]
    fn test_parse_mk_plugin_targets_and_completion_installer() {
        let plugin = parse("plugins/mk.toml").unwrap();
        assert_eq!(plugin.name, "mk");
        assert_eq!(plugin.resolve_target("macos", "x86_64"), None);
        assert_eq!(
            plugin.resolve_target("linux", "aarch64"),
            Some("aarch64-unknown-linux-gnu".to_string())
        );
        assert_eq!(
            plugin.resolve_target("macos", "aarch64"),
            Some("aarch64-apple-darwin".to_string())
        );
        assert_eq!(
            plugin.completions.as_ref().unwrap().command,
            "{binary_path} completion {shell}"
        );

        assert!(plugin.post_install.is_none());
        assert!(plugin.post_uninstall.is_none());
    }

    #[test]
    fn test_parse_atlas_plugin_targets_and_completion_installer() {
        let plugin = parse("plugins/atlas.toml").unwrap();
        assert_eq!(plugin.name, "atlas");
        assert!(plugin.alias.contains(&"atlas-context-router".to_string()));
        assert!(
            plugin
                .alias
                .contains(&"atlas-context-router-rs".to_string())
        );
        assert_eq!(
            plugin.resolve_target("linux", "x86_64"),
            Some("x86_64-unknown-linux-musl".to_string())
        );
        assert_eq!(plugin.resolve_target("linux", "aarch64"), None);
        assert_eq!(plugin.resolve_target("macos", "x86_64"), None);
        assert_eq!(
            plugin.resolve_target("macos", "aarch64"),
            Some("aarch64-apple-darwin".to_string())
        );
        assert_eq!(
            plugin.completions.as_ref().unwrap().command,
            "{binary_path} completions {shell}"
        );

        assert!(plugin.post_install.is_none());
        assert!(plugin.post_uninstall.is_none());
    }

    #[test]
    fn test_parse_scpr_plugin_targets_and_completion_installer() {
        let plugin = parse("plugins/scpr.toml").unwrap();
        assert_eq!(plugin.name, "scpr");
        assert_eq!(
            plugin.resolve_target("linux", "aarch64"),
            Some("aarch64-unknown-linux-musl".to_string())
        );
        assert_eq!(
            plugin.completions.as_ref().unwrap().command,
            "{binary_path} completions {shell}"
        );
        assert!(plugin.post_install.is_none());
        assert!(plugin.post_uninstall.is_none());
    }

    #[test]
    fn test_parse_sqlx_build_only_plugin() {
        let plugin = parse("plugins/sqlx.toml").unwrap();
        assert_eq!(plugin.name, "sqlx");
        assert!(plugin.alias.contains(&"sqlx-cli".to_string()));
        assert!(plugin.alias.contains(&"cargo-sqlx".to_string()));
        assert_eq!(plugin.location, "github:transact-rs/sqlx");
        assert!(plugin.asset_pattern.is_empty());
        assert_eq!(plugin.build_branch.as_deref(), Some("main"));
        assert_eq!(plugin.binary, "target/release/sqlx");

        let build_script = plugin.build_script.as_ref().unwrap();
        assert_eq!(build_script.len(), 1);
        assert!(build_script[0].contains("cargo build --release -p sqlx-cli"));

        let post_install = plugin.post_install.as_ref().unwrap();
        assert_eq!(post_install.len(), 1);
        assert!(post_install[0].contains("cargo-sqlx"));

        let post_uninstall = plugin.post_uninstall.as_ref().unwrap();
        assert_eq!(post_uninstall.len(), 1);
        assert!(post_uninstall[0].contains("cargo-sqlx"));
    }

    #[test]
    fn test_github_repo_parsing() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let (owner, repo) = plugin.github_repo().unwrap();
        assert_eq!(owner, "BurntSushi");
        assert_eq!(repo, "ripgrep");
    }

    #[test]
    fn test_parse_all_bundled_plugins() {
        let plugins = load_plugins_from_dir("plugins").unwrap();
        assert!(plugins.iter().any(|plugin| plugin.name == "atlas"));
        assert!(plugins.iter().any(|plugin| plugin.name == "difftastic"));
        assert!(plugins.iter().any(|plugin| plugin.name == "navi"));
        assert!(plugins.iter().any(|plugin| plugin.name == "ripgrep"));
        assert!(plugins.iter().any(|plugin| plugin.name == "fd"));
        assert!(plugins.iter().any(|plugin| plugin.name == "jq"));
        assert!(plugins.iter().any(|plugin| plugin.name == "scpr"));
        assert!(plugins.iter().any(|plugin| plugin.name == "sqlx"));
        assert!(plugins.iter().any(|plugin| plugin.name == "tw-dl"));
    }

    #[test]
    fn test_expand_template_no_v_prefix() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let result = plugin.expand_template(
            "{name}-{version}-{target}.tar.gz",
            "14.1.0",
            "x86_64-unknown-linux-musl",
        );
        assert_eq!(result, "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz");
    }

    #[test]
    fn test_expand_template_with_v_prefix() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        // {version} strips the leading 'v'; {tag} keeps it as-is
        let result = plugin.expand_template(
            "{name}-{version}-{target}.tar.gz",
            "v14.1.0",
            "x86_64-unknown-linux-musl",
        );
        assert_eq!(result, "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz");

        let with_tag = plugin.expand_template(
            "{name}-{tag}-{target}.tar.gz",
            "v14.1.0",
            "x86_64-unknown-linux-musl",
        );
        assert_eq!(with_tag, "ripgrep-v14.1.0-x86_64-unknown-linux-musl.tar.gz");
    }

    #[test]
    fn test_expand_checksum_template() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let result = plugin.expand_template(
            plugin.checksum_asset_pattern.as_deref().unwrap(),
            "v14.1.0",
            "x86_64-unknown-linux-musl",
        );
        assert_eq!(
            result,
            "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz.sha256"
        );
    }

    #[test]
    fn test_resolve_target_linux_x86_64() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let target = plugin.resolve_target("linux", "x86_64");
        assert_eq!(target, Some("x86_64-unknown-linux-musl".to_string()));
    }

    #[test]
    fn test_resolve_target_macos_aarch64() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let target = plugin.resolve_target("macos", "aarch64");
        assert_eq!(target, Some("aarch64-apple-darwin".to_string()));
    }

    #[test]
    fn test_resolve_target_unknown_platform() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let target = plugin.resolve_target("freebsd", "x86_64");
        assert_eq!(target, None);
    }

    #[test]
    fn test_load_plugins_from_dir() {
        let plugins = load_plugins_from_dir("plugins").unwrap();
        assert!(!plugins.is_empty());
        assert!(plugins.iter().any(|p| p.name == "ripgrep"));
    }

    #[test]
    fn test_load_plugins_from_missing_dir() {
        let plugins = load_plugins_from_dir("plugins/does-not-exist").unwrap();
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_find_plugin_by_name() {
        let plugin = find_plugin("ripgrep", &["plugins"]).unwrap();
        assert_eq!(plugin.name, "ripgrep");
    }

    #[test]
    fn test_find_plugin_by_alias() {
        let plugin = find_plugin("rg", &["plugins"]).unwrap();
        assert_eq!(plugin.name, "ripgrep");
    }

    #[test]
    fn test_find_plugin_not_found() {
        let result = find_plugin("nonexistent", &["plugins"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_plugins_from_dirs_deduplicates_by_name() {
        let plugins = load_plugins_from_dirs(&["plugins", "plugins"]).unwrap();
        let ripgrep_plugins = plugins
            .iter()
            .filter(|plugin| plugin.name == "ripgrep")
            .count();
        assert_eq!(ripgrep_plugins, 1);
    }

    #[test]
    fn test_format_shadowed_plugin_warning_groups_plugins() {
        let warning = format_shadowed_plugin_warning(
            "plugins",
            "/tmp/remote-indexes/example",
            &["bat".to_string(), "starship".to_string()],
        );

        assert!(warning.contains("2 plugins from 'plugins' are ignored"));
        assert!(warning.contains("/tmp/remote-indexes/example"));
        assert!(warning.contains("Shadowed: bat, starship"));
    }

    #[test]
    fn test_emit_warning_once_tracks_duplicates() {
        let unique_key = format!(
            "warning-test:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let emitted = EMITTED_PLUGIN_WARNINGS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut emitted = emitted.lock().unwrap();

        assert!(emitted.insert(unique_key.clone()));
        assert!(!emitted.insert(unique_key));
    }

    #[test]
    fn test_parse_ee_plugin_files() {
        let plugin = parse("plugins/ee.toml").unwrap();
        assert_eq!(plugin.files.len(), 3);
        assert_eq!(
            plugin.files[0].from,
            "ee-{version}-{target}/ee-openrouter-agent"
        );
        assert_eq!(plugin.files[0].to, "{install_dir}/ee-openrouter-agent");
        assert!(plugin.files[0].executable);
        assert_eq!(plugin.files[1].to, "{data_home}/ee");
        assert!(!plugin.files[1].executable);
        assert_eq!(plugin.files[2].to, "{config_home}/ee/plugins");
    }

    #[test]
    fn test_resolve_files_expands_templates_and_roots() {
        let plugin = parse("plugins/ee.toml").unwrap();
        let files = plugin
            .resolve_files("v0.11.10", "x86_64-unknown-linux-musl")
            .unwrap();

        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].from,
            "ee-0.11.10-x86_64-unknown-linux-musl/ee-openrouter-agent"
        );
        assert_eq!(files[0].root, FileRoot::InstallDir);
        assert_eq!(files[0].relative_path, PathBuf::from("ee-openrouter-agent"));
        assert!(files[0].executable);
        assert_eq!(files[1].root, FileRoot::DataHome);
        assert_eq!(files[1].relative_path, PathBuf::from("ee"));
        assert_eq!(files[2].root, FileRoot::ConfigHome);
        assert_eq!(files[2].relative_path, PathBuf::from("ee/plugins"));
    }

    #[test]
    fn test_resolve_files_rejects_missing_separator_after_root() {
        let mut plugin = parse("plugins/ee.toml").unwrap();
        plugin.files[0].to = "{data_home}evil".to_string();

        let error = plugin.resolve_files("v1", "target").unwrap_err();

        assert!(
            error.to_string().contains("files.to must start with"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_resolve_files_tolerates_trailing_slash_in_from() {
        let mut plugin = parse("plugins/ee.toml").unwrap();
        plugin.files[0].from = "pkg/agent/".to_string();

        let files = plugin.resolve_files("v1", "target").unwrap();

        assert_eq!(files[0].from, "pkg/agent");
    }

    #[test]
    fn test_resolve_files_rejects_unknown_destination_root() {
        let mut plugin = parse("plugins/ee.toml").unwrap();
        plugin.files[0].to = "relative/path".to_string();

        let error = plugin
            .resolve_files("v0.11.10", "x86_64-unknown-linux-musl")
            .unwrap_err();

        assert!(
            error.to_string().contains("files.to must start with"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_resolve_files_rejects_path_traversal() {
        let mut plugin = parse("plugins/ee.toml").unwrap();
        plugin.files[0].from = "../outside/agent".to_string();

        let error = plugin.resolve_files("v1", "target").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("files.from must be a relative path"),
            "unexpected error: {error}"
        );

        let mut plugin = parse("plugins/ee.toml").unwrap();
        plugin.files[0].to = "{data_home}/../escape".to_string();

        let error = plugin.resolve_files("v1", "target").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must not escape its destination root"),
            "unexpected error: {error}"
        );
    }
}
