use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Deserialize)]
struct FileSettings {
    install_dir: Option<PathBuf>,
    man_dir: Option<PathBuf>,
    #[serde(default)]
    plugin_dirs: Vec<String>,
    index_ttl_secs: Option<u64>,
    lock_stale_after_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AppSettings {
    install_dir: PathBuf,
    man_dir: PathBuf,
    data_dir: PathBuf,
    plugin_dirs: Vec<String>,
    index_ttl_secs: Option<u64>,
    lock_stale_after_secs: u64,
}

impl AppSettings {
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("Failed to determine home directory")?;
        let config_dir = resolve_xdg_dir(
            env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            dirs::config_dir(),
            &home,
            ".config",
        );
        let data_home = resolve_xdg_dir(
            env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            dirs::data_local_dir(),
            &home,
            ".local/share",
        );
        let config_file = config_dir.join("scpr").join("config.toml");
        let file_settings = load_file_settings(&config_file)?;

        let install_dir = env::var_os("SCPR_BIN_DIR")
            .map(PathBuf::from)
            .or(file_settings.install_dir)
            .unwrap_or_else(|| home.join(".local/bin"));
        let man_dir = env::var_os("SCPR_MAN_DIR")
            .map(PathBuf::from)
            .or(file_settings.man_dir)
            .unwrap_or_else(|| data_home.join("man").join("man1"));
        let data_dir = data_home.join("scpr");

        let mut plugin_dirs = env::var_os("SCPR_PLUGINS_DIR")
            .map(|value| {
                env::split_paths(&value)
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        plugin_dirs.extend(file_settings.plugin_dirs);

        Ok(Self {
            install_dir,
            man_dir,
            data_dir,
            plugin_dirs,
            index_ttl_secs: Some(file_settings.index_ttl_secs.unwrap_or(600)),
            lock_stale_after_secs: env::var("SCPR_LOCK_STALE_AFTER_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .or(file_settings.lock_stale_after_secs)
                .unwrap_or(300),
        })
    }

    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    pub fn man_dir(&self) -> &Path {
        &self.man_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn index_ttl_secs(&self) -> Option<u64> {
        self.index_ttl_secs
    }

    pub fn lock_stale_after_secs(&self) -> u64 {
        self.lock_stale_after_secs
    }

    pub fn default_plugin_dirs(&self) -> Vec<String> {
        let mut dirs = self.plugin_dirs.clone();
        dirs.push(self.data_dir.join("plugins").to_string_lossy().to_string());
        dirs.push("plugins".to_string());
        dirs
    }
}

fn resolve_xdg_dir(
    env_value: Option<PathBuf>,
    dirs_value: Option<PathBuf>,
    home: &Path,
    fallback_suffix: &str,
) -> PathBuf {
    env_value
        .or(dirs_value)
        .unwrap_or_else(|| home.join(fallback_suffix))
}

fn load_file_settings(path: &Path) -> Result<FileSettings> {
    if !path.exists() {
        return Ok(FileSettings::default());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{load_file_settings, resolve_xdg_dir};
    use std::path::{Path, PathBuf};

    #[test]
    fn test_load_file_settings_defaults_when_missing() {
        let path = Path::new("tests/does-not-exist.toml");
        let settings = load_file_settings(path).unwrap();
        assert!(settings.install_dir.is_none());
        assert!(settings.man_dir.is_none());
        assert!(settings.plugin_dirs.is_empty());
        assert!(settings.index_ttl_secs.is_none());
        assert!(settings.lock_stale_after_secs.is_none());
    }

    #[test]
    fn test_resolve_xdg_dir_prefers_explicit_env_value() {
        let resolved = resolve_xdg_dir(
            Some(PathBuf::from("/tmp/config")),
            Some(PathBuf::from("/ignored")),
            Path::new("/home/alice"),
            ".config",
        );
        assert_eq!(resolved, PathBuf::from("/tmp/config"));
    }

    #[test]
    fn test_resolve_xdg_dir_falls_back_to_home_suffix() {
        let resolved =
            resolve_xdg_dir(None, None, Path::new("/home/alice"), ".local/share");
        assert_eq!(resolved, PathBuf::from("/home/alice/.local/share"));
    }
}
