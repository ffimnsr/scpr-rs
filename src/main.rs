use anyhow::Result;
use clap::{Arg, ArgAction, Command};
use serde::Serialize;
use tracing::warn;
use tracing_subscriber::EnvFilter;

mod github;
mod installer;
mod plugin;

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

/// Return the default list of directories to search for plugin TOML files.
///
/// Order:
/// 1. `~/.local/share/scpr/plugins/` (user-installed plugins)
/// 2. `./plugins/` (development / local checkout)
fn default_plugin_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(
            home.join(".local/share/scpr/plugins")
                .to_string_lossy()
                .to_string(),
        );
    }
    dirs.push("plugins".to_string());
    dirs
}

fn add_plugins_dir_arg(dirs: &mut Vec<String>, extra: Option<&String>) {
    if let Some(extra) = extra {
        dirs.insert(0, extra.clone());
    }
}

#[derive(Debug, Serialize)]
struct OutdatedPackage {
    name: String,
    current_version: String,
    latest_version: String,
}

#[derive(Debug)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Debug)]
struct PackageRequest {
    name: String,
    tag: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Use RUST_LOG if set, otherwise default to info-level output.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .init();

    let plugins_dir_arg = Arg::new("plugins-dir")
        .long("plugins-dir")
        .short('p')
        .help("Additional plugins directory to search")
        .action(clap::ArgAction::Set);

    let dry_run_arg = Arg::new("dry-run")
        .long("dry-run")
        .action(ArgAction::SetTrue)
        .help("Show what would happen without making any changes");

    let json_arg = Arg::new("json")
        .long("json")
        .action(ArgAction::SetTrue)
        .help("Output results as JSON");

    let mut cli_app = Command::new("scpr")
        .bin_name("scpr")
        .version(PKG_VERSION)
        .about(PKG_DESCRIPTION)
        .arg_required_else_help(true)
        .propagate_version(true)
        .subcommand(
            Command::new("install")
                .about("Install the latest release of a package")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Plugin name or alias, optionally as <name>@<tag>"),
                )
                .arg(
                    Arg::new("tag")
                        .long("tag")
                        .action(ArgAction::Set)
                        .help("Install a specific release tag"),
                )
                .arg(dry_run_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("uninstall")
                .about("Uninstall a previously installed package")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Plugin name or alias"),
                )
                .arg(dry_run_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("update")
                .about("Update an installed package, or all packages with --all")
                .arg(
                    Arg::new("package")
                        .required(false)
                        .help("Plugin name or alias, optionally as <name>@<tag>"),
                )
                .arg(
                    Arg::new("all")
                        .long("all")
                        .action(ArgAction::SetTrue)
                        .help("Update all installed packages (pinned packages are skipped)"),
                )
                .arg(
                    Arg::new("tag")
                        .long("tag")
                        .action(ArgAction::Set)
                        .help("Update to a specific release tag"),
                )
                .arg(dry_run_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("plugins")
                .about("Inspect available plugins")
                .subcommand(
                    Command::new("list")
                        .about("List all available plugins")
                        .arg(json_arg.clone())
                        .arg(plugins_dir_arg.clone()),
                )
                .subcommand(
                    Command::new("search")
                        .about("Search available plugins")
                        .arg(
                            Arg::new("query")
                                .help(
                                    "Case-insensitive plugin name, alias, or description filter",
                                )
                                .required(false),
                        )
                        .arg(json_arg.clone())
                        .arg(plugins_dir_arg.clone()),
                )
                .subcommand(
                    Command::new("info")
                        .about("Show details about an available plugin")
                        .arg(
                            Arg::new("package")
                                .required(true)
                                .help("Plugin name or alias"),
                        )
                        .arg(plugins_dir_arg.clone()),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("List all installed packages")
                .arg(json_arg.clone()),
        )
        .subcommand(
            Command::new("outdated")
                .about("List installed packages with newer releases")
                .arg(json_arg.clone()),
        )
        .subcommand(Command::new("doctor").about("Check the local installer setup"))
        .subcommand(
            Command::new("status")
                .about("Show installed packages (alias for list)")
                .arg(json_arg.clone()),
        )
        .subcommand(
            Command::new("verify")
                .about("Verify SHA-256 checksums of all installed binaries"),
        )
        .subcommand(
            Command::new("pin")
                .about("Pin a package so `update --all` skips it")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Installed package name"),
                ),
        )
        .subcommand(
            Command::new("unpin")
                .about("Remove the pin from an installed package")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Installed package name"),
                ),
        )
        .subcommand(
            Command::new("completions")
                .about("Print shell completion script to stdout")
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .help("Shell to generate completions for (bash, zsh, fish, elvish, powershell)"),
                ),
        );

    let matches = cli_app.clone().get_matches();

    let client = github::GithubClient::new(PKG_VERSION)?;
    let installer = installer::Installer::new()?;

    match matches.subcommand() {
        Some(("install", sub)) => {
            let request = parse_package_request(
                sub.get_one::<String>("package").unwrap(),
                sub.get_one::<String>("tag").map(String::as_str),
            )?;
            let dry_run = sub.get_flag("dry-run");
            let mut dirs = default_plugin_dirs();
            add_plugins_dir_arg(&mut dirs, sub.get_one::<String>("plugins-dir"));
            let plugin = plugin::find_plugin(&request.name, &dirs)?;
            installer
                .install(&plugin, &client, request.tag.as_deref(), dry_run)
                .await?;
        }
        Some(("update", sub)) => {
            let mut dirs = default_plugin_dirs();
            add_plugins_dir_arg(&mut dirs, sub.get_one::<String>("plugins-dir"));

            let update_all = sub.get_flag("all");
            let dry_run = sub.get_flag("dry-run");
            let package = sub.get_one::<String>("package");
            let requested_tag = sub.get_one::<String>("tag").map(String::as_str);

            if update_all {
                if package.is_some() || requested_tag.is_some() {
                    anyhow::bail!(
                        "Use either `update <package> [--tag <tag>]` or `update --all`, not both"
                    );
                }
                let outdated =
                    collect_outdated_packages(&installer, &client, &dirs).await?;
                if outdated.is_empty() {
                    println!("All installed packages are up to date.");
                } else {
                    for entry in outdated {
                        let plugin = plugin::find_plugin(&entry.name, &dirs)?;
                        // Pass the already-known latest tag to avoid a redundant API fetch.
                        installer
                            .install(&plugin, &client, Some(&entry.latest_version), dry_run)
                            .await?;
                    }
                }
            } else {
                let package = package.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Missing package name. Use `update <package> [--tag <tag>]` or `update --all`"
                    )
                })?;
                let request = parse_package_request(package, requested_tag)?;
                let plugin = plugin::find_plugin(&request.name, &dirs)?;
                installer
                    .install(&plugin, &client, request.tag.as_deref(), dry_run)
                    .await?;
            }
        }
        Some(("uninstall", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            let dry_run = sub.get_flag("dry-run");
            let mut dirs = default_plugin_dirs();
            add_plugins_dir_arg(&mut dirs, sub.get_one::<String>("plugins-dir"));
            let plugin = plugin::find_plugin(package, &dirs)?;
            installer.uninstall(&plugin, dry_run).await?;
        }
        Some(("plugins", sub)) => match sub.subcommand() {
            Some(("list", sub)) => {
                let json = sub.get_flag("json");
                let mut dirs = default_plugin_dirs();
                add_plugins_dir_arg(&mut dirs, sub.get_one::<String>("plugins-dir"));
                let plugins = plugin::load_plugins_from_dirs(&dirs)?;
                print_available_plugins(&plugins, json);
            }
            Some(("search", sub)) => {
                let json = sub.get_flag("json");
                let mut dirs = default_plugin_dirs();
                add_plugins_dir_arg(&mut dirs, sub.get_one::<String>("plugins-dir"));
                let query = sub
                    .get_one::<String>("query")
                    .map(|value| value.to_ascii_lowercase());
                let plugins = plugin::load_plugins_from_dirs(&dirs)?;
                let filtered: Vec<_> = plugins
                    .into_iter()
                    .filter(|plugin| matches_query(plugin, query.as_deref()))
                    .collect();
                print_available_plugins(&filtered, json);
            }
            Some(("info", sub)) => {
                let package = sub.get_one::<String>("package").unwrap();
                let mut dirs = default_plugin_dirs();
                add_plugins_dir_arg(&mut dirs, sub.get_one::<String>("plugins-dir"));
                let plugin = plugin::find_plugin(package, &dirs)?;
                print_plugin_info(&plugin);
            }
            _ => {}
        },
        Some(("outdated", sub)) => {
            let json = sub.get_flag("json");
            let dirs = default_plugin_dirs();
            let outdated = collect_outdated_packages(&installer, &client, &dirs).await?;
            print_outdated_packages(&outdated, json);
        }
        Some(("doctor", _)) => {
            let checks = build_doctor_checks(&installer, &default_plugin_dirs())?;
            print_doctor_checks(&checks);
        }
        Some(("list", sub)) | Some(("status", sub)) => {
            let json = sub.get_flag("json");
            let installed = installer.list_installed()?;
            print_installed_packages(&installed, json);
        }
        Some(("verify", _)) => {
            installer.verify()?;
        }
        Some(("pin", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            installer.pin(package)?;
        }
        Some(("unpin", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            installer.unpin(package)?;
        }
        Some(("completions", sub)) => {
            use clap_complete::{Shell, generate};
            let shell_str = sub.get_one::<String>("shell").unwrap();
            let shell: Shell = shell_str
                .parse()
                .map_err(|_| anyhow::anyhow!("Unknown shell '{shell_str}'. Supported: bash, zsh, fish, elvish, powershell"))?;
            let mut stdout = std::io::stdout();
            generate(shell, &mut cli_app, "scpr", &mut stdout);
        }
        _ => {}
    }

    Ok(())
}

fn parse_package_request(package: &str, cli_tag: Option<&str>) -> Result<PackageRequest> {
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

async fn collect_outdated_packages(
    installer: &installer::Installer,
    client: &github::GithubClient,
    dirs: &[String],
) -> Result<Vec<OutdatedPackage>> {
    use futures_util::future;
    use std::sync::Arc;

    let client = Arc::new(client);
    let all_installed = installer.list_installed()?;

    // Build one future per non-pinned package, bounded to 8 concurrent requests.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));

    let futures: Vec<_> = all_installed
        .into_iter()
        .filter(|p| !p.pinned)
        .map(|installed| {
            let client = Arc::clone(&client);
            let sem = Arc::clone(&semaphore);
            let dirs = dirs.to_vec();
            async move {
                let _permit = sem.acquire().await.ok()?;
                let plugin = match plugin::find_plugin(&installed.name, &dirs) {
                    Ok(p) => p,
                    Err(err) => {
                        warn!(
                            "Skipping '{}' during update check: {err}",
                            installed.name
                        );
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

    let mut outdated: Vec<OutdatedPackage> =
        future::join_all(futures).await.into_iter().flatten().collect();
    outdated.sort_by(|l, r| l.name.cmp(&r.name));
    Ok(outdated)
}

fn build_doctor_checks(
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
    });

    let state_file = installer.state_file_path();
    checks.push(DoctorCheck {
        name: "State File",
        ok: !state_file.exists() || state_file.is_file(),
        detail: format!("{}", state_file.display()),
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
    });

    // Check whether the man page directory is reachable via MANPATH.
    let local_man = installer.local_man_dir();
    // Walk up two levels: man1 -> man -> share/man, then check that parent.
    let man_base = local_man
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let manpath_entries: Vec<std::path::PathBuf> = std::env::var("MANPATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let man_in_path = man_base.is_some_and(|base| {
        manpath_entries.contains(&base)
    });
    checks.push(DoctorCheck {
        name: "MANPATH",
        ok: man_in_path,
        detail: format!("{} in MANPATH", local_man.display()),
    });

    Ok(checks)
}

fn matches_query(plugin: &plugin::Plugin, query: Option<&str>) -> bool {
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

fn print_available_plugins(plugins: &[plugin::Plugin], json: bool) {
    if json {
        // Serialize a minimal view; Plugin itself derives nothing for JSON so we
        // build ad-hoc values.
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

fn print_installed_packages(installed: &[installer::InstalledPackage], json: bool) {
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

fn print_outdated_packages(packages: &[OutdatedPackage], json: bool) {
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

fn print_doctor_checks(checks: &[DoctorCheck]) {
    let failed = checks.iter().any(|check| !check.ok);
    for check in checks {
        let status = if check.ok { "OK" } else { "FAIL" };
        println!("{:<14} {:<4} {}", check.name, status, check.detail);
    }

    if failed {
        println!("Doctor found one or more issues.");
    } else {
        println!("Doctor did not find any issues.");
    }
}

fn print_plugin_info(plugin: &plugin::Plugin) {
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

    // Show which target triple would be used for the running platform.
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let resolved = plugin
        .resolve_target(os, arch)
        .unwrap_or_else(|| "(not configured)".to_string());
    println!("Current Platform: {os}/{arch} -> {resolved}");
}

#[cfg(test)]
mod tests {
    use super::parse_package_request;

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
}
