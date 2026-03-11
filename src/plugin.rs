use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use tracing::debug;
use walkdir::{DirEntry, WalkDir};

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
#[derive(Debug, Deserialize, Clone, Default)]
pub struct Plugin {
    pub name: String,
    pub alias: Vec<String>,
    pub description: Option<String>,
    /// GitHub location in the form `github:<owner>/<repo>`.
    pub location: String,
    /// Asset filename pattern with template placeholders.
    pub asset_pattern: String,
    /// Path to the binary within the extracted archive.
    pub binary: String,
    /// Paths to man pages within the extracted archive.
    pub man_pages: Option<Vec<String>>,
    /// Map from `<os>-<arch>` key to the target triple used in release asset names.
    pub targets: Option<HashMap<String, String>>,
}

impl Plugin {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn alias(&self) -> &[String] {
        &self.alias
    }

    /// Parse the `github:<owner>/<repo>` location and return `(owner, repo)`.
    pub fn github_repo(&self) -> Option<(&str, &str)> {
        let loc = self.location.strip_prefix("github:")?;
        let mut parts = loc.splitn(2, '/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        Some((owner, repo))
    }

    /// Look up the platform target triple for the given OS and architecture.
    pub fn resolve_target(&self, os: &str, arch: &str) -> Option<String> {
        let key = format!("{os}-{arch}");
        self.targets.as_ref()?.get(&key).cloned()
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
}

#[derive(Deserialize)]
struct PluginContainer {
    plugin: Plugin,
}

/// Parse a plugin TOML file and return the [`Plugin`].
pub fn parse(path: &str) -> Result<Plugin> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read plugin file: {path}"))?;

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

/// Load all plugins from a directory (non-recursive, `.toml` files only).
pub fn load_plugins_from_dir(dir: &str) -> Result<Vec<Plugin>> {
    let mut plugins = Vec::new();

    for entry in WalkDir::new(dir)
        .max_depth(2)
        .into_iter()
        .filter_entry(|e| is_not_hidden(e))
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_type().is_dir())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "toml")
                .unwrap_or(false)
        })
    {
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
    for dir in dirs {
        if let Ok(plugins) = load_plugins_from_dir(dir.as_ref()) {
            for plugin in plugins {
                if plugin.name == name || plugin.alias.iter().any(|a| a == name) {
                    return Ok(plugin);
                }
            }
        }
    }
    Err(anyhow!("Plugin '{name}' not found in any plugins directory"))
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
        assert!(plugin.targets.is_some());
    }

    #[test]
    fn test_github_repo_parsing() {
        let plugin = parse("plugins/ripgrep.toml").unwrap();
        let (owner, repo) = plugin.github_repo().unwrap();
        assert_eq!(owner, "BurntSushi");
        assert_eq!(repo, "ripgrep");
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
}
