use crate::{github::GithubClient, settings::AppSettings};
use anyhow::{Context, Result, anyhow};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use tracing::warn;

const INDEX_SYNC_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePluginIndex {
    pub repo: String,
    pub branch: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub added_at_unix: Option<u64>,
    #[serde(default)]
    pub last_synced_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginIndexPin {
    pub plugin: String,
    pub repo: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RemoteIndexConfig {
    #[serde(default)]
    indexes: Vec<RemotePluginIndex>,
    #[serde(default)]
    plugin_pins: Vec<PluginIndexPin>,
}

pub struct RemoteIndexManager {
    config_file: PathBuf,
    cache_root: PathBuf,
}

impl RemoteIndexManager {
    pub fn new() -> Result<Self> {
        let settings = AppSettings::load()?;
        Self::from_base_dir(settings.data_dir().to_path_buf())
    }

    pub fn from_base_dir(base_dir: PathBuf) -> Result<Self> {
        let cache_root = base_dir.join("remote-indexes");
        fs::create_dir_all(&cache_root)
            .with_context(|| format!("Failed to create {}", cache_root.display()))?;
        Ok(Self {
            config_file: base_dir.join("remote-indexes.toml"),
            cache_root,
        })
    }

    pub fn list(&self) -> Result<Vec<RemotePluginIndex>> {
        Ok(self.load_config()?.indexes)
    }

    pub fn list_plugin_pins(&self) -> Result<Vec<PluginIndexPin>> {
        Ok(self.load_config()?.plugin_pins)
    }

    pub async fn add(
        &self,
        repo: &str,
        client: &GithubClient,
    ) -> Result<RemotePluginIndex> {
        let repo = normalize_repo(repo)?;
        let mut config = self.load_config()?;
        if config.indexes.iter().any(|index| index.repo == repo) {
            return Err(anyhow!(
                "Remote plugin index '{repo}' is already configured"
            ));
        }

        let (owner, name) = split_repo(&repo)?;
        let metadata = client.get_repo_metadata(owner, name).await?;
        let mut index = RemotePluginIndex {
            repo: repo.clone(),
            branch: metadata.default_branch,
            enabled: true,
            added_at_unix: Some(current_unix_timestamp()?),
            last_synced_unix: None,
        };
        self.sync_index(client, &mut index).await?;
        config.indexes.push(index.clone());
        self.save_config(&config)?;
        Ok(index)
    }

    pub async fn sync_all(
        &self,
        client: &GithubClient,
        ttl_secs: Option<u64>,
        force_refresh: bool,
    ) -> Result<Vec<PathBuf>> {
        let mut config = self.load_config()?;
        let mut dirs = Vec::new();

        for index in &mut config.indexes {
            if !index.enabled {
                continue;
            }
            let cache_dir = self.cache_dir(index);
            if !force_refresh && !self.should_sync(index, &cache_dir, ttl_secs)? {
                dirs.push(cache_dir);
                continue;
            }
            match self.sync_index(client, index).await {
                Ok(dir) => dirs.push(dir),
                Err(err) => {
                    if cache_dir.exists() {
                        warn!(
                            "Failed to sync remote plugin index '{}': {err}. Using cached plugins from {}",
                            index.repo,
                            cache_dir.display()
                        );
                        dirs.push(cache_dir);
                    } else {
                        return Err(err).with_context(|| {
                            format!("Failed to sync remote plugin index '{}'", index.repo)
                        });
                    }
                }
            }
        }

        self.save_config(&config)?;
        Ok(dirs)
    }

    pub async fn sync_all_indexes(
        &self,
        client: &GithubClient,
    ) -> Result<Vec<RemotePluginIndex>> {
        let mut config = self.load_config()?;
        let client = client.clone();
        let results = stream::iter(
            config
                .indexes
                .iter()
                .filter(|index| index.enabled)
                .cloned()
                .map(|mut index| {
                    let client = client.clone();
                    async move {
                        self.sync_index(&client, &mut index).await?;
                        Ok::<RemotePluginIndex, anyhow::Error>(index)
                    }
                }),
        )
        .buffer_unordered(INDEX_SYNC_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

        let mut synced = Vec::new();
        for result in results {
            let synced_index = result?;
            if let Some(index) = config
                .indexes
                .iter_mut()
                .find(|index| index.repo == synced_index.repo)
            {
                *index = synced_index.clone();
            }
            synced.push(synced_index);
        }

        self.save_config(&config)?;
        Ok(synced)
    }

    pub async fn sync_one(
        &self,
        repo: &str,
        client: &GithubClient,
    ) -> Result<RemotePluginIndex> {
        let repo = normalize_repo(repo)?;
        let mut config = self.load_config()?;
        let index = config
            .indexes
            .iter_mut()
            .find(|index| index.repo == repo)
            .ok_or_else(|| anyhow!("Remote plugin index '{repo}' is not configured"))?;
        self.sync_index(client, index).await?;
        let result = index.clone();
        self.save_config(&config)?;
        Ok(result)
    }

    pub fn enable(&self, repo: &str) -> Result<RemotePluginIndex> {
        self.set_enabled(repo, true)
    }

    pub fn disable(&self, repo: &str) -> Result<RemotePluginIndex> {
        self.set_enabled(repo, false)
    }

    pub fn remove(&self, repo: &str) -> Result<RemotePluginIndex> {
        let repo = normalize_repo(repo)?;
        let mut config = self.load_config()?;
        let position = config
            .indexes
            .iter()
            .position(|index| index.repo == repo)
            .ok_or_else(|| anyhow!("Remote plugin index '{repo}' is not configured"))?;
        let removed = config.indexes.remove(position);
        let cache_dir = self.cache_dir(&removed);
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)
                .with_context(|| format!("Failed to remove {}", cache_dir.display()))?;
        }
        config.plugin_pins.retain(|pin| pin.repo != repo);
        self.save_config(&config)?;
        Ok(removed)
    }

    pub fn promote(&self, repo: &str) -> Result<RemotePluginIndex> {
        self.move_index(repo, true)
    }

    pub fn demote(&self, repo: &str) -> Result<RemotePluginIndex> {
        self.move_index(repo, false)
    }

    pub fn pin_plugin_to_index(
        &self,
        plugin: &str,
        repo: &str,
    ) -> Result<PluginIndexPin> {
        let plugin = normalize_plugin_name(plugin)?;
        let repo = normalize_repo(repo)?;
        let mut config = self.load_config()?;
        if !config.indexes.iter().any(|index| index.repo == repo) {
            return Err(anyhow!("Remote plugin index '{repo}' is not configured"));
        }

        if let Some(existing) = config
            .plugin_pins
            .iter_mut()
            .find(|pin| pin.plugin == plugin)
        {
            existing.repo = repo.clone();
            let result = existing.clone();
            self.save_config(&config)?;
            return Ok(result);
        }

        let pin = PluginIndexPin { plugin, repo };
        config.plugin_pins.push(pin.clone());
        config
            .plugin_pins
            .sort_by(|left, right| left.plugin.cmp(&right.plugin));
        self.save_config(&config)?;
        Ok(pin)
    }

    pub fn unpin_plugin(&self, plugin: &str) -> Result<PluginIndexPin> {
        let plugin = normalize_plugin_name(plugin)?;
        let mut config = self.load_config()?;
        let position = config
            .plugin_pins
            .iter()
            .position(|pin| pin.plugin == plugin)
            .ok_or_else(|| {
                anyhow!("Plugin '{plugin}' is not pinned to a remote index")
            })?;
        let pin = config.plugin_pins.remove(position);
        self.save_config(&config)?;
        Ok(pin)
    }

    pub fn preferred_index_for_plugin(
        &self,
        plugin: &str,
    ) -> Result<Option<PluginIndexPin>> {
        let plugin = normalize_plugin_name(plugin)?;
        Ok(self
            .load_config()?
            .plugin_pins
            .into_iter()
            .find(|pin| pin.plugin == plugin))
    }

    pub fn get_index(&self, repo: &str) -> Result<Option<RemotePluginIndex>> {
        let repo = normalize_repo(repo)?;
        Ok(self
            .load_config()?
            .indexes
            .into_iter()
            .find(|index| index.repo == repo))
    }

    pub fn cache_dir_for_repo(&self, repo: &str) -> Result<PathBuf> {
        let repo = normalize_repo(repo)?;
        Ok(self.cache_root.join(repo.replace('/', "__")))
    }

    fn load_config(&self) -> Result<RemoteIndexConfig> {
        if !self.config_file.exists() {
            return Ok(RemoteIndexConfig::default());
        }
        let content = fs::read_to_string(&self.config_file)
            .with_context(|| format!("Failed to read {}", self.config_file.display()))?;
        toml::from_str(&content).context("Failed to parse remote index config")
    }

    fn save_config(&self, config: &RemoteIndexConfig) -> Result<()> {
        let content =
            toml::to_string(config).context("Failed to serialize remote index config")?;
        let config_dir = self
            .config_file
            .parent()
            .context("Remote index config has no parent directory")?;
        let mut temp = NamedTempFile::new_in(config_dir).with_context(|| {
            format!("Failed to create temp file in {}", config_dir.display())
        })?;
        std::io::Write::write_all(&mut temp, content.as_bytes())
            .context("Failed to write staged remote index config")?;
        temp.persist(&self.config_file).map_err(|err| {
            anyhow!(
                "Failed to replace remote index config {}: {}",
                self.config_file.display(),
                err.error
            )
        })?;
        Ok(())
    }

    async fn sync_index(
        &self,
        client: &GithubClient,
        index: &mut RemotePluginIndex,
    ) -> Result<PathBuf> {
        let (owner, repo) = split_repo(&index.repo)?;
        let tree = client.get_git_tree(owner, repo, &index.branch).await?;
        let plugin_paths: Vec<&str> = tree
            .tree
            .iter()
            .filter(|entry| entry.entry_type == "blob")
            .map(|entry| entry.path.as_str())
            .filter(|path| path.starts_with("plugins/") && path.ends_with(".toml"))
            .collect();

        if plugin_paths.is_empty() {
            return Err(anyhow!(
                "Remote plugin index '{}' does not contain any plugin TOML files under plugins/",
                index.repo
            ));
        }

        let cache_dir = self.cache_dir(index);
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("Failed to create {}", cache_dir.display()))?;

        for path in fs::read_dir(&cache_dir)
            .with_context(|| format!("Failed to read {}", cache_dir.display()))?
        {
            let path = path?.path();
            if path.is_file() {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
        }

        for plugin_path in plugin_paths {
            let raw_url = format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{path}",
                branch = index.branch,
                path = plugin_path
            );
            let content = client.download_text(&raw_url).await?;
            let filename = plugin_path
                .strip_prefix("plugins/")
                .unwrap_or(plugin_path)
                .replace('/', "__");
            let dest = cache_dir.join(filename);
            fs::write(&dest, content)
                .with_context(|| format!("Failed to write {}", dest.display()))?;
        }

        index.last_synced_unix = Some(current_unix_timestamp()?);
        Ok(cache_dir)
    }

    fn cache_dir(&self, index: &RemotePluginIndex) -> PathBuf {
        self.cache_root.join(index.repo.replace('/', "__"))
    }

    fn should_sync(
        &self,
        index: &RemotePluginIndex,
        cache_dir: &std::path::Path,
        ttl_secs: Option<u64>,
    ) -> Result<bool> {
        if !cache_dir.exists() {
            return Ok(true);
        }
        let Some(ttl_secs) = ttl_secs else {
            return Ok(true);
        };
        let Some(last_synced) = index.last_synced_unix else {
            return Ok(true);
        };
        let now = current_unix_timestamp()?;
        Ok(now.saturating_sub(last_synced) >= ttl_secs)
    }

    fn set_enabled(&self, repo: &str, enabled: bool) -> Result<RemotePluginIndex> {
        let repo = normalize_repo(repo)?;
        let mut config = self.load_config()?;
        let index = config
            .indexes
            .iter_mut()
            .find(|index| index.repo == repo)
            .ok_or_else(|| anyhow!("Remote plugin index '{repo}' is not configured"))?;
        index.enabled = enabled;
        let result = index.clone();
        self.save_config(&config)?;
        Ok(result)
    }

    fn move_index(&self, repo: &str, toward_front: bool) -> Result<RemotePluginIndex> {
        let repo = normalize_repo(repo)?;
        let mut config = self.load_config()?;
        let index = config
            .indexes
            .iter()
            .position(|index| index.repo == repo)
            .ok_or_else(|| anyhow!("Remote plugin index '{repo}' is not configured"))?;

        if toward_front {
            if index > 0 {
                config.indexes.swap(index, index - 1);
            }
        } else if index + 1 < config.indexes.len() {
            config.indexes.swap(index, index + 1);
        }

        let result = config
            .indexes
            .iter()
            .find(|index| index.repo == repo)
            .cloned()
            .expect("index present after move");
        self.save_config(&config)?;
        Ok(result)
    }
}

fn default_enabled() -> bool {
    true
}

fn normalize_repo(repo: &str) -> Result<String> {
    let repo = repo.strip_prefix("github:").unwrap_or(repo).trim();
    let (owner, name) = split_repo(repo)?;
    Ok(format!("{owner}/{name}"))
}

fn split_repo(repo: &str) -> Result<(&str, &str)> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| {
        anyhow!("Expected GitHub repository in the form <owner>/<repo>")
    })?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(anyhow!(
            "Expected GitHub repository in the form <owner>/<repo>"
        ));
    }
    Ok((owner, name))
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_secs())
}

fn normalize_plugin_name(plugin: &str) -> Result<String> {
    let plugin = plugin.trim();
    if plugin.is_empty() {
        return Err(anyhow!("Plugin name cannot be empty"));
    }
    Ok(plugin.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        PluginIndexPin, RemoteIndexConfig, RemoteIndexManager, RemotePluginIndex,
        normalize_plugin_name, normalize_repo, split_repo,
    };

    fn temp_manager() -> RemoteIndexManager {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.keep();
        let base = root.join("scpr");
        std::fs::create_dir_all(base.join("remote-indexes")).unwrap();
        RemoteIndexManager {
            config_file: base.join("remote-indexes.toml"),
            cache_root: base.join("remote-indexes"),
        }
    }

    #[test]
    fn test_normalize_repo_accepts_plain_repo() {
        assert_eq!(
            normalize_repo("ffimnsr/scpr-rs").unwrap(),
            "ffimnsr/scpr-rs"
        );
    }

    #[test]
    fn test_normalize_repo_accepts_github_prefix() {
        assert_eq!(
            normalize_repo("github:ffimnsr/scpr-rs").unwrap(),
            "ffimnsr/scpr-rs"
        );
    }

    #[test]
    fn test_split_repo_rejects_nested_path() {
        assert!(split_repo("owner/repo/extra").is_err());
    }

    #[test]
    fn test_promote_moves_index_forward() {
        let manager = temp_manager();
        manager
            .save_config(&RemoteIndexConfig {
                indexes: vec![
                    RemotePluginIndex {
                        repo: "a/one".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                    RemotePluginIndex {
                        repo: "b/two".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                ],
                plugin_pins: Vec::new(),
            })
            .unwrap();

        manager.promote("b/two").unwrap();
        let repos: Vec<String> = manager
            .list()
            .unwrap()
            .into_iter()
            .map(|i| i.repo)
            .collect();
        assert_eq!(repos, vec!["b/two".to_string(), "a/one".to_string()]);
    }

    #[test]
    fn test_demote_moves_index_backward() {
        let manager = temp_manager();
        manager
            .save_config(&RemoteIndexConfig {
                indexes: vec![
                    RemotePluginIndex {
                        repo: "a/one".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                    RemotePluginIndex {
                        repo: "b/two".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                ],
                plugin_pins: Vec::new(),
            })
            .unwrap();

        manager.demote("a/one").unwrap();
        let repos: Vec<String> = manager
            .list()
            .unwrap()
            .into_iter()
            .map(|i| i.repo)
            .collect();
        assert_eq!(repos, vec!["b/two".to_string(), "a/one".to_string()]);
    }

    #[test]
    fn test_disable_preserves_index_order() {
        let manager = temp_manager();
        manager
            .save_config(&RemoteIndexConfig {
                indexes: vec![
                    RemotePluginIndex {
                        repo: "a/one".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                    RemotePluginIndex {
                        repo: "b/two".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                ],
                plugin_pins: Vec::new(),
            })
            .unwrap();

        manager.disable("a/one").unwrap();
        let indexes = manager.list().unwrap();
        assert_eq!(indexes[0].repo, "a/one");
        assert!(!indexes[0].enabled);
        assert_eq!(indexes[1].repo, "b/two");
        assert!(indexes[1].enabled);
    }

    #[test]
    fn test_pin_plugin_to_index_replaces_existing_pin() {
        let manager = temp_manager();
        manager
            .save_config(&RemoteIndexConfig {
                indexes: vec![
                    RemotePluginIndex {
                        repo: "a/one".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                    RemotePluginIndex {
                        repo: "b/two".to_string(),
                        branch: "main".to_string(),
                        enabled: true,
                        added_at_unix: None,
                        last_synced_unix: None,
                    },
                ],
                plugin_pins: vec![PluginIndexPin {
                    plugin: "ripgrep".to_string(),
                    repo: "a/one".to_string(),
                }],
            })
            .unwrap();

        manager.pin_plugin_to_index("ripgrep", "b/two").unwrap();
        let pins = manager.list_plugin_pins().unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].plugin, "ripgrep");
        assert_eq!(pins[0].repo, "b/two");
    }

    #[test]
    fn test_remove_index_also_removes_plugin_pins() {
        let manager = temp_manager();
        manager
            .save_config(&RemoteIndexConfig {
                indexes: vec![RemotePluginIndex {
                    repo: "a/one".to_string(),
                    branch: "main".to_string(),
                    enabled: true,
                    added_at_unix: None,
                    last_synced_unix: None,
                }],
                plugin_pins: vec![PluginIndexPin {
                    plugin: "ripgrep".to_string(),
                    repo: "a/one".to_string(),
                }],
            })
            .unwrap();

        manager.remove("a/one").unwrap();
        assert!(manager.list_plugin_pins().unwrap().is_empty());
    }

    #[test]
    fn test_normalize_plugin_name_rejects_empty_string() {
        assert!(normalize_plugin_name("   ").is_err());
    }
}
