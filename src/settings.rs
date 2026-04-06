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
}

#[derive(Debug, Clone)]
pub struct AppSettings {
    install_dir: PathBuf,
    man_dir: PathBuf,
    data_dir: PathBuf,
    plugin_dirs: Vec<String>,
    index_ttl_secs: Option<u64>,
}

impl AppSettings {
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("Failed to determine home directory")?;
        let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let config_file = config_dir.join("scpr").join("config.toml");
        let file_settings = load_file_settings(&config_file)?;

        let install_dir = env::var_os("SCPR_BIN_DIR")
            .map(PathBuf::from)
            .or(file_settings.install_dir)
            .unwrap_or_else(|| home.join(".local/bin"));
        let man_dir = env::var_os("SCPR_MAN_DIR")
            .map(PathBuf::from)
            .or(file_settings.man_dir)
            .unwrap_or_else(|| home.join(".local/share/man/man1"));
        let data_dir = home.join(".local/share/scpr");

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

    pub fn default_plugin_dirs(&self) -> Vec<String> {
        let mut dirs = self.plugin_dirs.clone();
        dirs.push(self.data_dir.join("plugins").to_string_lossy().to_string());
        dirs.push("plugins".to_string());
        dirs
    }
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
    use super::load_file_settings;

    #[test]
    fn test_load_file_settings_defaults_when_missing() {
        let path = std::path::Path::new("tests/does-not-exist.toml");
        let settings = load_file_settings(path).unwrap();
        assert!(settings.install_dir.is_none());
        assert!(settings.man_dir.is_none());
        assert!(settings.plugin_dirs.is_empty());
        assert!(settings.index_ttl_secs.is_none());
    }
}
