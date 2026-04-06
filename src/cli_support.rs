use crate::{github, installer, plugin, remote_index, settings};
use anyhow::Result;
use futures_util::future;
use serde::Serialize;
use std::{path::Path, sync::Arc};
use tracing::warn;

#[derive(Debug, Serialize)]
pub(crate) struct OutdatedPackage {
    pub(crate) name: String,
    pub(crate) current_version: String,
    pub(crate) latest_version: String,
}

#[derive(Debug)]
pub(crate) struct DoctorCheck {
    pub(crate) name: &'static str,
    pub(crate) ok: bool,
    pub(crate) detail: String,
    pub(crate) remediation: Option<String>,
}

#[derive(Debug)]
pub(crate) struct PackageRequest {
    pub(crate) name: String,
    pub(crate) tag: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstalledPackageStatus {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) binary: String,
    pub(crate) pinned: bool,
    pub(crate) installed_at_unix: Option<u64>,
    pub(crate) latest_version: Option<String>,
    pub(crate) outdated: bool,
}

pub(crate) fn add_plugins_dir_arg(dirs: &mut Vec<String>, extra: Option<&String>) {
    if let Some(extra) = extra {
        dirs.insert(0, extra.clone());
    }
}

pub(crate) async fn resolved_plugin_dirs(
    settings: &settings::AppSettings,
    client: &github::GithubClient,
    extra: Option<&String>,
    force_refresh: bool,
) -> Result<Vec<String>> {
    let mut dirs = settings.default_plugin_dirs();
    add_plugins_dir_arg(&mut dirs, extra);

    let remote_manager = remote_index::RemoteIndexManager::new()?;
    let remote_dirs = remote_manager
        .sync_all(client, settings.index_ttl_secs(), force_refresh)
        .await?;
    if !remote_dirs.is_empty() {
        let insert_at = usize::from(extra.is_some()) + 1;
        for (offset, dir) in remote_dirs.into_iter().enumerate() {
            dirs.insert(insert_at + offset, dir.to_string_lossy().to_string());
        }
    }

    Ok(dirs)
}

pub(crate) async fn resolved_plugin_dirs_for_query(
    settings: &settings::AppSettings,
    client: &github::GithubClient,
    extra: Option<&String>,
    query: &str,
    force_refresh: bool,
) -> Result<Vec<String>> {
    let mut dirs = resolved_plugin_dirs(settings, client, extra, force_refresh).await?;
    let manager = remote_index::RemoteIndexManager::new()?;
    apply_preferred_remote_pin_to_dirs(&manager, &mut dirs, query, extra.is_some())?;
    Ok(dirs)
}

pub(crate) fn parse_package_request(
    package: &str,
    cli_tag: Option<&str>,
) -> Result<PackageRequest> {
    let (name, package_tag) = match package.split_once('@') {
        Some((name, tag)) if !name.is_empty() && !tag.is_empty() => {
            (name.to_string(), Some(tag.to_string()))
        }
        Some(_) => {
            anyhow::bail!(
                "Invalid package specifier '{package}'. Use <name> or <name>@<tag>"
            );
        }
        None => (package.to_string(), None),
    };

    if package_tag.is_some() && cli_tag.is_some() {
        anyhow::bail!("Use either <name>@<tag> or --tag <tag>, not both");
    }

    Ok(PackageRequest {
        name,
        tag: package_tag.or_else(|| cli_tag.map(str::to_string)),
    })
}

pub(crate) fn parse_state_format(
    format: Option<&str>,
    path: Option<&str>,
) -> Result<installer::StateFormat> {
    match format
        .or_else(|| {
            path.and_then(|path| Path::new(path).extension().and_then(|ext| ext.to_str()))
        })
        .unwrap_or("json")
    {
        "json" => Ok(installer::StateFormat::Json),
        "toml" => Ok(installer::StateFormat::Toml),
        other => Err(anyhow::anyhow!(
            "Unsupported state format '{other}'. Use json or toml."
        )),
    }
}

pub(crate) fn parse_repo_arg(repo: &str) -> Result<(&str, &str)> {
    let repo = repo.strip_prefix("github:").unwrap_or(repo);
    let (owner, name) = repo.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("Expected GitHub repository in the form <owner>/<repo>")
    })?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        anyhow::bail!("Expected GitHub repository in the form <owner>/<repo>");
    }
    Ok((owner, name))
}

pub(crate) fn preferred_remote_pin_for_query(
    manager: &remote_index::RemoteIndexManager,
    query: &str,
) -> Result<Option<remote_index::PluginIndexPin>> {
    if let Some(pin) = manager.preferred_index_for_plugin(query)? {
        return Ok(Some(pin));
    }

    for pin in manager.list_plugin_pins()? {
        let cache_dir = manager.cache_dir_for_repo(&pin.repo)?;
        let cache_dir = cache_dir.to_string_lossy().to_string();
        for plugin in plugin::load_plugins_from_dir(&cache_dir)? {
            if plugin.name == query || plugin.alias.iter().any(|alias| alias == query) {
                return Ok(Some(pin));
            }
        }
    }

    Ok(None)
}

pub(crate) fn apply_preferred_remote_pin_to_dirs(
    manager: &remote_index::RemoteIndexManager,
    dirs: &mut Vec<String>,
    query: &str,
    has_extra_plugins_dir: bool,
) -> Result<()> {
    let Some(pin) = preferred_remote_pin_for_query(manager, query)? else {
        return Ok(());
    };

    let index = manager.get_index(&pin.repo)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Plugin '{}' is pinned to remote index '{}', but that index is no longer configured",
            pin.plugin,
            pin.repo
        )
    })?;

    if !index.enabled {
        anyhow::bail!(
            "Plugin '{}' is pinned to remote index '{}', but that index is disabled. Enable it or unpin the plugin.",
            pin.plugin,
            pin.repo
        );
    }

    let preferred_dir = manager.cache_dir_for_repo(&pin.repo)?;
    if let Some(position) = dirs
        .iter()
        .position(|dir| Path::new(dir) == preferred_dir.as_path())
    {
        let dir = dirs.remove(position);
        let insert_at = usize::from(has_extra_plugins_dir) + 1;
        dirs.insert(insert_at.min(dirs.len()), dir);
        Ok(())
    } else {
        anyhow::bail!(
            "Plugin '{}' is pinned to remote index '{}', but its cached plugin directory is unavailable at {}",
            pin.plugin,
            pin.repo,
            preferred_dir.display()
        )
    }
}

pub(crate) async fn collect_outdated_packages(
    installer: &installer::Installer,
    client: &github::GithubClient,
    dirs: &[String],
    filter_name: Option<&str>,
    skip_pinned: bool,
) -> Result<Vec<OutdatedPackage>> {
    let client = Arc::new(client);
    let all_installed = installer.list_installed()?;
    let filter_name = filter_name.map(str::to_string);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));

    let futures: Vec<_> = all_installed
        .into_iter()
        .filter(|package| !skip_pinned || !package.pinned)
        .filter(|package| {
            filter_name
                .as_deref()
                .is_none_or(|name| package.name == name)
        })
        .map(|installed| {
            let client = Arc::clone(&client);
            let sem = Arc::clone(&semaphore);
            let dirs = dirs.to_vec();
            async move {
                let _permit = sem.acquire().await.ok()?;
                let manager = match remote_index::RemoteIndexManager::new() {
                    Ok(manager) => manager,
                    Err(err) => {
                        warn!("Skipping '{}' during update check: {err}", installed.name);
                        return None;
                    }
                };
                let mut package_dirs = dirs;
                if let Err(err) = apply_preferred_remote_pin_to_dirs(
                    &manager,
                    &mut package_dirs,
                    &installed.name,
                    false,
                ) {
                    warn!("Skipping '{}' during update check: {err}", installed.name);
                    return None;
                }
                let plugin = match plugin::find_plugin(&installed.name, &package_dirs) {
                    Ok(p) => p,
                    Err(err) => {
                        warn!("Skipping '{}' during update check: {err}", installed.name);
                        return None;
                    }
                };
                let (owner, repo) = plugin.github_repo()?;
                let release = client.get_latest_release(owner, repo).await.ok()?;
                if release.tag_name != installed.version {
                    Some(OutdatedPackage {
                        name: installed.name,
                        current_version: installed.version,
                        latest_version: release.tag_name,
                    })
                } else {
                    None
                }
            }
        })
        .collect();

    let mut outdated: Vec<OutdatedPackage> = future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect();
    outdated.sort_by(|l, r| l.name.cmp(&r.name));
    Ok(outdated)
}

pub(crate) fn build_installed_status_rows(
    installed: Vec<installer::InstalledPackage>,
    outdated: &[OutdatedPackage],
) -> Vec<InstalledPackageStatus> {
    let latest_versions = outdated
        .iter()
        .map(|package| (package.name.as_str(), package.latest_version.as_str()))
        .collect::<std::collections::HashMap<_, _>>();

    let mut rows = installed
        .into_iter()
        .map(|pkg| {
            let latest_version = latest_versions
                .get(pkg.name.as_str())
                .map(|value| (*value).to_string());
            let outdated = latest_version.is_some();
            InstalledPackageStatus {
                name: pkg.name,
                version: pkg.version.clone(),
                binary: pkg.binary,
                pinned: pkg.pinned,
                installed_at_unix: pkg.installed_at_unix,
                latest_version: Some(latest_version.unwrap_or(pkg.version)),
                outdated,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

pub(crate) fn build_doctor_checks(
    installer: &installer::Installer,
    plugin_dirs: &[String],
) -> Result<Vec<DoctorCheck>> {
    let mut checks = Vec::new();
    let local_bin = installer.local_bin_dir();
    let path_entries = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    let path_has_local_bin = path_entries.iter().any(|entry| entry == local_bin);
    checks.push(DoctorCheck {
        name: "PATH",
        ok: path_has_local_bin,
        detail: format!("{} in PATH", local_bin.display()),
        remediation: (!path_has_local_bin).then(|| {
            format!(
                "Add this to your shell profile, then restart your shell:\n  export PATH=\"{}:$PATH\"",
                local_bin.display()
            )
        }),
    });

    let state_file = installer.state_file_path();
    checks.push(DoctorCheck {
        name: "State File",
        ok: !state_file.exists() || state_file.is_file(),
        detail: format!("{}", state_file.display()),
        remediation: (state_file.exists() && !state_file.is_file()).then(|| {
            format!(
                "Move the unexpected path out of the way and let scpr recreate it:\n  mv \"{}\" \"{}.bak\"",
                state_file.display(),
                state_file.display()
            )
        }),
    });

    let installed = installer.list_installed()?;
    let missing_binaries = installed
        .iter()
        .filter(|package| !installer.local_bin_dir().join(&package.binary).exists())
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Installed Binaries",
        ok: missing_binaries.is_empty(),
        detail: if missing_binaries.is_empty() {
            "All installed binaries are present".to_string()
        } else {
            format!("Missing binaries for: {}", missing_binaries.join(", "))
        },
        remediation: (!missing_binaries.is_empty()).then(|| {
            format!(
                "Reinstall the affected packages, or inspect drift with `scpr audit`.\n  scpr install {}",
                missing_binaries.join(" ")
            )
        }),
    });

    let missing_man_pages = installed
        .iter()
        .flat_map(|package| {
            package.man_pages.iter().filter_map(|page| {
                let path = installer.local_man_dir().join(page);
                if path.exists() {
                    None
                } else {
                    Some(format!("{} ({})", page, package.name))
                }
            })
        })
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Man Pages",
        ok: missing_man_pages.is_empty(),
        detail: if missing_man_pages.is_empty() {
            "All recorded man pages are present".to_string()
        } else {
            format!("Missing man pages: {}", missing_man_pages.join(", "))
        },
        remediation: (!missing_man_pages.is_empty()).then(|| {
            "Reinstall the affected package so scpr can restore the recorded man pages."
                .to_string()
        }),
    });

    let unreadable_plugin_dirs = plugin_dirs
        .iter()
        .filter_map(|dir| match plugin::load_plugins_from_dir(dir) {
            Ok(_) => None,
            Err(err) => Some(format!("{dir}: {err}")),
        })
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Plugin Dirs",
        ok: unreadable_plugin_dirs.is_empty(),
        detail: if unreadable_plugin_dirs.is_empty() {
            "Plugin directories are readable".to_string()
        } else {
            unreadable_plugin_dirs.join("; ")
        },
        remediation: (!unreadable_plugin_dirs.is_empty()).then(|| {
            "Check the configured plugin paths, or override them with `SCPR_PLUGINS_DIR` / `scpr --plugins-dir`."
                .to_string()
        }),
    });

    let local_man = installer.local_man_dir();
    let man_base = local_man
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let manpath_entries: Vec<std::path::PathBuf> = std::env::var("MANPATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let man_in_path = man_base
        .as_ref()
        .is_some_and(|base| manpath_entries.contains(base));
    checks.push(DoctorCheck {
        name: "MANPATH",
        ok: man_in_path,
        detail: format!("{} in MANPATH", local_man.display()),
        remediation: (!man_in_path).then(|| {
            let man_base = man_base
                .map(|base| base.display().to_string())
                .unwrap_or_else(|| installer.local_man_dir().display().to_string());
            format!(
                "Add this to your shell profile if `man` cannot find installed pages:\n  export MANPATH=\"{}:$MANPATH\"",
                man_base
            )
        }),
    });

    Ok(checks)
}

pub(crate) fn matches_query(plugin: &plugin::Plugin, query: Option<&str>) -> bool {
    let Some(query) = query else {
        return true;
    };

    plugin.name.to_ascii_lowercase().contains(query)
        || plugin
            .alias
            .iter()
            .any(|alias| alias.to_ascii_lowercase().contains(query))
        || plugin
            .description
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains(query)
}

pub(crate) fn print_available_plugins(plugins: &[plugin::Plugin], json: bool) {
    if json {
        let values: Vec<serde_json::Value> = plugins
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "aliases": p.alias,
                    "description": p.description,
                    "location": p.location,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&values).unwrap());
        return;
    }

    if plugins.is_empty() {
        println!("No available plugins found.");
        return;
    }

    println!("{:<20} {:<18} Description", "Plugin", "Aliases");
    println!("{}", "-".repeat(80));
    for plugin in plugins {
        let aliases = if plugin.alias.is_empty() {
            "-".to_string()
        } else {
            plugin.alias.join(", ")
        };
        let description = plugin.description.as_deref().unwrap_or("-");
        println!("{:<20} {:<18} {}", plugin.name, aliases, description);
    }
}

pub(crate) fn print_installed_packages(
    installed: &[installer::InstalledPackage],
    json: bool,
) {
    if json {
        println!("{}", serde_json::to_string_pretty(installed).unwrap());
        return;
    }

    if installed.is_empty() {
        println!("No packages installed.");
        return;
    }

    println!(
        "{:<20} {:<20} {:<20} {:<12} Installed",
        "Package", "Version", "Binary", "Pinned"
    );
    println!("{}", "-".repeat(85));
    for pkg in installed {
        let installed_date = pkg
            .installed_at_unix
            .map(|ts| {
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0)
                    .unwrap_or_default();
                dt.format("%Y-%m-%d").to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<20} {:<20} {:<12} {}",
            pkg.name,
            pkg.version,
            pkg.binary,
            if pkg.pinned { "yes" } else { "no" },
            installed_date
        );
    }
}

pub(crate) fn print_installed_status_rows(rows: &[InstalledPackageStatus], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }

    if rows.is_empty() {
        println!("No packages installed.");
        return;
    }

    println!(
        "{:<20} {:<20} {:<20} {:<12} {:<20} {:<12} Latest",
        "Package", "Version", "Binary", "Pinned", "Installed", "Status"
    );
    println!("{}", "-".repeat(124));
    for row in rows {
        let installed_date = row
            .installed_at_unix
            .map(|ts| {
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0)
                    .unwrap_or_default();
                dt.format("%Y-%m-%d").to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<20} {:<20} {:<12} {:<20} {:<12} {}",
            row.name,
            row.version,
            row.binary,
            if row.pinned { "yes" } else { "no" },
            installed_date,
            if row.outdated { "outdated" } else { "current" },
            row.latest_version.as_deref().unwrap_or("-")
        );
    }
}

pub(crate) fn print_outdated_packages(packages: &[OutdatedPackage], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(packages).unwrap());
        return;
    }

    if packages.is_empty() {
        println!("All installed packages are up to date.");
        return;
    }

    println!("{:<20} {:<20} {:<20}", "Package", "Installed", "Latest");
    println!("{}", "-".repeat(60));
    for package in packages {
        println!(
            "{:<20} {:<20} {:<20}",
            package.name, package.current_version, package.latest_version
        );
    }
}

pub(crate) fn print_doctor_checks(checks: &[DoctorCheck]) {
    let failed = checks.iter().any(|check| !check.ok);
    for check in checks {
        let status = if check.ok { "[OK]" } else { "[FAIL]" };
        println!("{:<14} {:<6} {}", check.name, status, check.detail);
        if !check.ok
            && let Some(remediation) = &check.remediation
        {
            println!("  fix: {}", remediation.replace('\n', "\n       "));
        }
    }

    if failed {
        println!("Doctor found one or more issues.");
    } else {
        println!("Doctor did not find any issues.");
    }
}

pub(crate) fn print_audit_records(records: &[installer::AuditRecord], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(records).unwrap());
        return;
    }

    if records.is_empty() {
        println!("No packages installed.");
        return;
    }

    println!("{:<20} {:<12} Details", "Package", "Status");
    println!("{}", "-".repeat(90));
    for record in records {
        let status = match record.status {
            installer::AuditStatus::Ok => "[OK]",
            installer::AuditStatus::Modified => "[MODIFIED]",
            installer::AuditStatus::Missing => "[MISSING]",
            installer::AuditStatus::Untracked => "[UNTRACKED]",
        };
        println!("{:<20} {:<12} {}", record.package, status, record.detail);
        println!("  {}", record.binary_path.display());
    }

    let modified = records
        .iter()
        .filter(|record| {
            matches!(
                record.status,
                installer::AuditStatus::Modified | installer::AuditStatus::Missing
            )
        })
        .count();
    if modified == 0 {
        println!("Audit complete: no modified installed binaries detected.");
    } else {
        println!("Audit complete: {modified} package(s) need attention.");
    }
}

pub(crate) fn print_history(events: &[installer::HistoryEvent], graph: bool, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(events).unwrap());
        return;
    }

    if events.is_empty() {
        println!("No package history recorded yet.");
        return;
    }

    if graph {
        use std::collections::BTreeMap;
        let mut by_package: BTreeMap<&str, Vec<&installer::HistoryEvent>> =
            BTreeMap::new();
        for event in events {
            by_package.entry(&event.package).or_default().push(event);
        }

        for (package, events) in by_package {
            println!("{package}");
            for event in events {
                let timestamp = human_timestamp(event.timestamp_unix);
                let marker = match event.action {
                    installer::HistoryAction::Installed => "+",
                    installer::HistoryAction::Updated => "~",
                    installer::HistoryAction::Removed => "-",
                    installer::HistoryAction::Pinned => "P",
                    installer::HistoryAction::Unpinned => "U",
                };
                println!("  {} {} {}", timestamp, marker, history_summary(event));
            }
        }
        return;
    }

    println!("{:<20} {:<19} {:<10} Details", "Package", "When", "Action");
    println!("{}", "-".repeat(90));
    for event in events {
        println!(
            "{:<20} {:<19} {:<10} {}",
            event.package,
            human_timestamp(event.timestamp_unix),
            history_action_label(&event.action),
            history_summary(event)
        );
    }
}

pub(crate) fn print_plugin_info(plugin: &plugin::Plugin) {
    println!("Name: {}", plugin.name);
    println!(
        "Aliases: {}",
        if plugin.alias.is_empty() {
            "-".to_string()
        } else {
            plugin.alias.join(", ")
        }
    );
    println!(
        "Description: {}",
        plugin.description.as_deref().unwrap_or("-")
    );
    println!("Source: {}", plugin.location);
    println!("Asset Pattern: {}", plugin.asset_pattern);
    println!(
        "Checksum Pattern: {}",
        plugin
            .checksum_asset_pattern
            .as_deref()
            .unwrap_or("GitHub digest only")
    );
    println!("Binary Path: {}", plugin.binary);
    println!(
        "Man Pages: {}",
        plugin
            .man_pages
            .as_deref()
            .map(|items| items.join(", "))
            .unwrap_or_else(|| "-".to_string())
    );
    println!("Targets:");
    if let Some(targets) = &plugin.targets {
        let mut targets: Vec<_> = targets.iter().collect();
        targets.sort_by(|left, right| left.0.cmp(right.0));
        for (platform, target) in &targets {
            println!("  {platform} -> {target}");
        }
    } else {
        println!("  -");
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let resolved = plugin
        .resolve_target(os, arch)
        .unwrap_or_else(|| "(not configured)".to_string());
    println!("Current Platform: {os}/{arch} -> {resolved}");
}

pub(crate) fn print_remote_indexes(
    indexes: &[remote_index::RemotePluginIndex],
    json: bool,
) {
    if json {
        println!("{}", serde_json::to_string_pretty(indexes).unwrap());
        return;
    }

    if indexes.is_empty() {
        println!("No remote plugin indexes configured.");
        return;
    }

    println!(
        "{:<4} {:<30} {:<16} {:<10} Last Synced",
        "Ord", "Repository", "Branch", "State"
    );
    println!("{}", "-".repeat(94));
    for (position, index) in indexes.iter().enumerate() {
        let last_synced = index
            .last_synced_unix
            .map(human_timestamp)
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<4} {:<30} {:<16} {:<10} {}",
            position + 1,
            index.repo,
            index.branch,
            if index.enabled { "enabled" } else { "disabled" },
            last_synced
        );
    }

    println!();
    println!(
        "Earlier entries take precedence when multiple indexes provide the same plugin name."
    );
}

pub(crate) fn print_plugin_index_pins(pins: &[remote_index::PluginIndexPin], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(pins).unwrap());
        return;
    }

    if pins.is_empty() {
        println!("No plugin-specific remote index pins configured.");
        return;
    }

    println!("{:<20} Preferred Remote Index", "Plugin");
    println!("{}", "-".repeat(72));
    for pin in pins {
        println!("{:<20} {}", pin.plugin, pin.repo);
    }
}

fn history_action_label(action: &installer::HistoryAction) -> &'static str {
    match action {
        installer::HistoryAction::Installed => "installed",
        installer::HistoryAction::Updated => "updated",
        installer::HistoryAction::Removed => "removed",
        installer::HistoryAction::Pinned => "pinned",
        installer::HistoryAction::Unpinned => "unpinned",
    }
}

fn history_summary(event: &installer::HistoryEvent) -> String {
    match event.action {
        installer::HistoryAction::Installed => {
            format!(
                "installed {}",
                event.version.as_deref().unwrap_or("unknown")
            )
        }
        installer::HistoryAction::Updated => format!(
            "{} -> {}",
            event.from_version.as_deref().unwrap_or("unknown"),
            event.to_version.as_deref().unwrap_or("unknown")
        ),
        installer::HistoryAction::Removed => {
            format!("removed {}", event.version.as_deref().unwrap_or("unknown"))
        }
        installer::HistoryAction::Pinned | installer::HistoryAction::Unpinned => event
            .detail
            .clone()
            .unwrap_or_else(|| history_action_label(&event.action).to_string()),
    }
}

fn human_timestamp(timestamp_unix: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp_unix as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_package_request, parse_repo_arg, parse_state_format};
    use crate::installer::StateFormat;

    #[test]
    fn test_parse_package_request_with_inline_tag() {
        let request = parse_package_request("ripgrep@15.1.0", None).unwrap();
        assert_eq!(request.name, "ripgrep");
        assert_eq!(request.tag.as_deref(), Some("15.1.0"));
    }

    #[test]
    fn test_parse_package_request_with_cli_tag() {
        let request = parse_package_request("ripgrep", Some("v15.1.0")).unwrap();
        assert_eq!(request.name, "ripgrep");
        assert_eq!(request.tag.as_deref(), Some("v15.1.0"));
    }

    #[test]
    fn test_parse_package_request_rejects_conflicting_tags() {
        let error = parse_package_request("ripgrep@15.1.0", Some("v15.1.0")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Use either <name>@<tag> or --tag <tag>, not both")
        );
    }

    #[test]
    fn test_parse_state_format_prefers_extension_when_flag_missing() {
        let format = parse_state_format(None, Some("backup.toml")).unwrap();
        assert!(matches!(format, StateFormat::Toml));
    }

    #[test]
    fn test_parse_state_format_defaults_to_json() {
        let format = parse_state_format(None, None).unwrap();
        assert!(matches!(format, StateFormat::Json));
    }

    #[test]
    fn test_parse_repo_arg_accepts_github_prefix() {
        let (owner, repo) = parse_repo_arg("github:BurntSushi/ripgrep").unwrap();
        assert_eq!(owner, "BurntSushi");
        assert_eq!(repo, "ripgrep");
    }
}
