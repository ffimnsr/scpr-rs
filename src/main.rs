use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use std::path::Path;
use std::process;
use tracing_subscriber::EnvFilter;

mod cli_definition;
mod cli_support;
mod github;
mod installer;
mod installer_archive;
mod plugin;
mod plugin_scaffold;
mod remote_index;
mod settings;

use cli_support::{
    build_doctor_checks, build_installed_status_rows, collect_outdated_packages,
    matches_query, parse_package_request, parse_repo_arg, parse_state_format,
    print_audit_records, print_available_plugins, print_doctor_checks, print_history,
    print_installed_packages, print_installed_status_rows, print_outdated_packages,
    print_plugin_index_pins, print_plugin_info, print_remote_indexes,
    resolved_plugin_dirs, resolved_plugin_dirs_for_query,
};

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
const UPDATE_ALL_CONCURRENCY: usize = 4;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut cli_app = cli_definition::build_cli(PKG_VERSION, PKG_DESCRIPTION);
    let matches = cli_app.clone().get_matches();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if matches.get_flag("quiet") {
            EnvFilter::new("warn")
        } else {
            match matches.get_count("verbose") {
                0 => EnvFilter::new("info"),
                1 => EnvFilter::new("debug"),
                _ => EnvFilter::new("trace"),
            }
        }
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .init();

    let settings = settings::AppSettings::load()?;
    let force_refresh = matches.get_flag("refresh");

    let client = github::GithubClient::new(PKG_VERSION)?;
    let installer = installer::Installer::new()?;

    match matches.subcommand() {
        Some(("install", sub)) => {
            let package_values: Vec<&String> =
                sub.get_many::<String>("packages").unwrap().collect();
            if package_values.len() > 1 && sub.get_one::<String>("tag").is_some() {
                anyhow::bail!("`install --tag` only supports a single package");
            }
            if package_values.len() > 1 && sub.get_one::<String>("target").is_some() {
                anyhow::bail!("`install --target` only supports a single package");
            }

            let dry_run = sub.get_flag("dry-run");
            for package in package_values {
                let request = parse_package_request(
                    package,
                    sub.get_one::<String>("tag").map(String::as_str),
                )?;
                let dirs = resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    &request.name,
                    force_refresh,
                )
                .await?;
                let plugin = plugin::find_plugin(&request.name, &dirs)?;
                installer
                    .install(
                        &plugin,
                        &client,
                        request.tag.as_deref(),
                        sub.get_one::<String>("target").map(String::as_str),
                        dry_run,
                    )
                    .await?;
            }
        }
        Some(("update", sub)) => {
            let dirs = resolved_plugin_dirs(
                &settings,
                &client,
                sub.get_one::<String>("plugins-dir"),
                force_refresh,
            )
            .await?;

            let update_all = sub.get_flag("all");
            let dry_run = sub.get_flag("dry-run");
            let package = sub.get_one::<String>("package");
            let requested_tag = sub.get_one::<String>("tag").map(String::as_str);
            let requested_target = sub.get_one::<String>("target").map(String::as_str);

            if update_all {
                if package.is_some()
                    || requested_tag.is_some()
                    || requested_target.is_some()
                {
                    anyhow::bail!(
                        "Use either `update <package> [--tag <tag>] [--target <triple>]` or `update --all`, not both"
                    );
                }
                let outdated =
                    collect_outdated_packages(&installer, &client, &dirs, None, true)
                        .await?;
                if outdated.is_empty() {
                    println!("All installed packages are up to date.");
                } else {
                    let installer = installer.clone();
                    let client = client.clone();
                    let settings = settings.clone();
                    let package_count = outdated.len();
                    let mut updates = stream::iter(outdated.into_iter().map(|entry| {
                        let installer = installer.clone();
                        let client = client.clone();
                        let settings = settings.clone();
                        async move {
                            let package_name = entry.name.clone();
                            let package_dirs = match resolved_plugin_dirs_for_query(
                                &settings,
                                &client,
                                None,
                                &entry.name,
                                force_refresh,
                            )
                            .await
                            {
                                Ok(dirs) => dirs,
                                Err(err) => return Err((package_name, err)),
                            };
                            let plugin =
                                match plugin::find_plugin(&entry.name, &package_dirs) {
                                    Ok(plugin) => plugin,
                                    Err(err) => return Err((package_name, err)),
                                };
                            match installer
                                .install(
                                    &plugin,
                                    &client,
                                    Some(&entry.latest_version),
                                    None,
                                    dry_run,
                                )
                                .await
                            {
                                Ok(()) => Ok(entry.name),
                                Err(err) => Err((entry.name, err)),
                            }
                        }
                    }))
                    .buffer_unordered(UPDATE_ALL_CONCURRENCY);

                    let mut succeeded = Vec::new();
                    let mut failed = Vec::new();
                    let mut completed = 0usize;
                    while let Some(result) = updates.next().await {
                        completed += 1;
                        match result {
                            Ok(name) => {
                                println!(
                                    "Progress: {completed}/{package_count} complete ({name}: updated)"
                                );
                                succeeded.push(name);
                            }
                            Err((name, err)) => {
                                eprintln!(
                                    "Progress: {completed}/{package_count} complete ({name}: failed)"
                                );
                                failed.push((name, err));
                            }
                        }
                    }

                    if !failed.is_empty() {
                        eprintln!(
                            "One or more packages failed during `update --all`. Successful updates were kept."
                        );
                        for (name, err) in &failed {
                            eprintln!("  {name}: {err}");
                        }
                    }

                    println!(
                        "Update summary: {} succeeded, {} failed, {} total.",
                        succeeded.len(),
                        failed.len(),
                        package_count
                    );
                    if !succeeded.is_empty() {
                        println!("Succeeded: {}", succeeded.join(", "));
                    }
                    if !failed.is_empty() {
                        println!(
                            "Failed: {}",
                            failed
                                .iter()
                                .map(|(name, _)| name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        let details = failed
                            .into_iter()
                            .map(|(name, err)| format!("{name}: {err}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        anyhow::bail!(
                            "One or more package updates failed during `update --all`:\n{}",
                            details
                        );
                    }
                }
            } else {
                let package = package.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Missing package name. Use `update <package> [--tag <tag>]` or `update --all`"
                    )
                })?;
                let request = parse_package_request(package, requested_tag)?;
                let dirs = resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    &request.name,
                    force_refresh,
                )
                .await?;
                let plugin = plugin::find_plugin(&request.name, &dirs)?;
                installer
                    .install(
                        &plugin,
                        &client,
                        request.tag.as_deref(),
                        requested_target,
                        dry_run,
                    )
                    .await?;
            }
        }
        Some(("uninstall", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            let dry_run = sub.get_flag("dry-run");
            let dirs = resolved_plugin_dirs_for_query(
                &settings,
                &client,
                sub.get_one::<String>("plugins-dir"),
                package,
                force_refresh,
            )
            .await?;
            let plugin = plugin::find_plugin(package, &dirs)?;
            installer.uninstall(&plugin, dry_run).await?;
        }
        Some(("plugins", sub)) => match sub.subcommand() {
            Some(("list", sub)) => {
                let json = sub.get_flag("json");
                let dirs = resolved_plugin_dirs(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    force_refresh,
                )
                .await?;
                let plugins = plugin::load_plugins_from_dirs(&dirs)?;
                print_available_plugins(&plugins, json);
            }
            Some(("search", sub)) => {
                let json = sub.get_flag("json");
                let dirs = resolved_plugin_dirs(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    force_refresh,
                )
                .await?;
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
                let dirs = resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    package,
                    force_refresh,
                )
                .await?;
                let plugin = plugin::find_plugin(package, &dirs)?;
                print_plugin_info(&plugin);
            }
            Some(("new", sub)) => {
                let repo = sub.get_one::<String>("repo").unwrap();
                let output = sub.get_one::<String>("output");
                let print_stdout = sub.get_flag("stdout");
                if print_stdout && output.is_some() {
                    anyhow::bail!(
                        "Use either `plugins new --stdout` or `plugins new --output <path>`, not both"
                    );
                }

                let (owner, repo_name) = parse_repo_arg(repo)?;
                let metadata = client.get_repo_metadata(owner, repo_name).await?;
                let release = client.get_latest_release(owner, repo_name).await?;
                let scaffold =
                    plugin_scaffold::build_plugin_scaffold(repo, &metadata, &release)?;
                if print_stdout {
                    println!("{}", scaffold.contents);
                } else {
                    let output_path =
                        output.map(std::path::PathBuf::from).unwrap_or_else(|| {
                            Path::new("plugins").join(&scaffold.file_name)
                        });
                    if output_path.exists() {
                        anyhow::bail!(
                            "Refusing to overwrite existing plugin file '{}'. Use --output to choose a different path.",
                            output_path.display()
                        );
                    }
                    if let Some(parent) = output_path.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(anyhow::Error::from)
                            .map_err(|err| {
                                err.context(format!(
                                    "Failed to create plugin scaffold directory '{}'",
                                    parent.display()
                                ))
                            })?;
                    }
                    std::fs::write(&output_path, scaffold.contents)
                        .map_err(anyhow::Error::from)
                        .map_err(|err| {
                            err.context(format!(
                                "Failed to write plugin scaffold to '{}'",
                                output_path.display()
                            ))
                        })?;
                    println!(
                        "Wrote plugin scaffold for '{}' to '{}'.",
                        scaffold.plugin_name,
                        output_path.display()
                    );
                    println!(
                        "Review asset patterns, binary paths, and target mappings before using it."
                    );
                }
            }
            Some(("index", sub)) => match sub.subcommand() {
                Some(("add", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.add(repo, &client).await?;
                    println!(
                        "Added remote plugin index '{}' on branch '{}'.",
                        index.repo, index.branch
                    );
                }
                Some(("list", sub)) => {
                    let json = sub.get_flag("json");
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let indexes = manager.list()?;
                    print_remote_indexes(&indexes, json);
                }
                Some(("enable", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.enable(repo)?;
                    println!("Enabled remote plugin index '{}'.", index.repo);
                }
                Some(("disable", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.disable(repo)?;
                    println!("Disabled remote plugin index '{}'.", index.repo);
                }
                Some(("remove", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.remove(repo)?;
                    println!("Removed remote plugin index '{}'.", index.repo);
                }
                Some(("sync", sub)) => {
                    let manager = remote_index::RemoteIndexManager::new()?;
                    if sub.get_flag("all") {
                        if sub.get_one::<String>("repo").is_some() {
                            anyhow::bail!(
                                "Use either `plugins index sync <repo>` or `plugins index sync --all`, not both"
                            );
                        }
                        let indexes = manager.sync_all_indexes(&client).await?;
                        if indexes.is_empty() {
                            println!("No enabled remote plugin indexes to sync.");
                        } else {
                            println!("Synced {} remote plugin index(es).", indexes.len());
                        }
                    } else {
                        let repo = sub.get_one::<String>("repo").ok_or_else(|| {
                            anyhow::anyhow!(
                                "Missing repo. Use `plugins index sync <owner>/<repo>` or `plugins index sync --all`"
                            )
                        })?;
                        let index = manager.sync_one(repo, &client).await?;
                        println!(
                            "Synced remote plugin index '{}' on branch '{}'.",
                            index.repo, index.branch
                        );
                    }
                }
                Some(("promote", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.promote(repo)?;
                    println!(
                        "Promoted remote plugin index '{}' in resolution order.",
                        index.repo
                    );
                }
                Some(("demote", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.demote(repo)?;
                    println!(
                        "Demoted remote plugin index '{}' in resolution order.",
                        index.repo
                    );
                }
                Some(("pin", sub)) => {
                    let plugin_name = sub.get_one::<String>("plugin").unwrap();
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.get_index(repo)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Remote plugin index '{}' is not configured",
                            repo
                        )
                    })?;
                    if !index.enabled {
                        anyhow::bail!(
                            "Remote plugin index '{}' is disabled. Enable it before pinning plugins to it.",
                            index.repo
                        );
                    }
                    manager.sync_one(&index.repo, &client).await?;
                    let cache_dir = manager.cache_dir_for_repo(&index.repo)?;
                    let cache_dir = cache_dir.to_string_lossy().to_string();
                    let plugin = plugin::find_plugin(plugin_name, &[cache_dir])?;
                    let pin = manager.pin_plugin_to_index(&plugin.name, &index.repo)?;
                    println!(
                        "Pinned plugin '{}' to remote plugin index '{}'.",
                        pin.plugin, pin.repo
                    );
                }
                Some(("unpin", sub)) => {
                    let plugin_name = sub.get_one::<String>("plugin").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let pin = manager.unpin_plugin(plugin_name)?;
                    println!("Removed remote plugin index pin for '{}'.", pin.plugin);
                }
                Some(("pins", sub)) => {
                    let json = sub.get_flag("json");
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let pins = manager.list_plugin_pins()?;
                    print_plugin_index_pins(&pins, json);
                }
                _ => {}
            },
            _ => {}
        },
        Some(("outdated", sub)) => {
            let json = sub.get_flag("json");
            let package = sub.get_one::<String>("package").map(String::as_str);
            let dirs = resolved_plugin_dirs(
                &settings,
                &client,
                sub.get_one::<String>("plugins-dir"),
                force_refresh,
            )
            .await?;
            let filter_name = match package {
                Some(package) => {
                    let package_dirs = resolved_plugin_dirs_for_query(
                        &settings,
                        &client,
                        sub.get_one::<String>("plugins-dir"),
                        package,
                        force_refresh,
                    )
                    .await?;
                    let name = plugin::find_plugin(package, &package_dirs)?.name;
                    if !installer
                        .list_installed()?
                        .iter()
                        .any(|installed| installed.name == name)
                    {
                        anyhow::bail!("'{}' is not installed", name);
                    }
                    Some(name)
                }
                None => None,
            };
            let outdated = collect_outdated_packages(
                &installer,
                &client,
                &dirs,
                filter_name.as_deref(),
                false,
            )
            .await?;
            print_outdated_packages(&outdated, json);
        }
        Some(("doctor", _)) => {
            let checks =
                build_doctor_checks(&installer, &settings.default_plugin_dirs())?;
            print_doctor_checks(&checks);
        }
        Some(("list", sub)) | Some(("status", sub)) => {
            let json = sub.get_flag("json");
            let installed = installer.list_installed()?;
            if sub.get_flag("outdated") {
                let dirs = resolved_plugin_dirs(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    force_refresh,
                )
                .await?;
                let outdated =
                    collect_outdated_packages(&installer, &client, &dirs, None, false)
                        .await?;
                let rows = build_installed_status_rows(installed, &outdated);
                print_installed_status_rows(&rows, json);
            } else {
                print_installed_packages(&installed, json);
            }
        }
        Some(("verify", _)) => {
            let records = installer.audit()?;
            print_audit_records(&records, false);
        }
        Some(("audit", sub)) => {
            let json = sub.get_flag("json");
            let records = installer.audit()?;
            print_audit_records(&records, json);
        }
        Some(("history", sub)) => match sub.subcommand() {
            Some(("clear", clear_sub)) => {
                let package = clear_sub.get_one::<String>("package").map(String::as_str);
                let removed = installer.clear_history(package)?;
                if let Some(package) = package {
                    println!("Cleared {removed} history event(s) for '{}'.", package);
                } else {
                    println!("Cleared {removed} history event(s).");
                }
            }
            _ => {
                let json = sub.get_flag("json");
                let graph = sub.get_flag("graph");
                let package = sub.get_one::<String>("package").map(String::as_str);
                let limit = sub.get_one::<usize>("limit").copied();
                let events = installer.history_limited(package, limit)?;
                print_history(&events, graph, json);
            }
        },
        Some(("export", sub)) => {
            let format = parse_state_format(
                sub.get_one::<String>("format").map(String::as_str),
                sub.get_one::<String>("path").map(String::as_str),
            )?;
            let content = installer.export_state(format)?;
            if let Some(path) = sub.get_one::<String>("path") {
                std::fs::write(path, content)
                    .map_err(anyhow::Error::from)
                    .map_err(|err| {
                        err.context(format!("Failed to write state export to '{}'", path))
                    })?;
                println!("Exported state to '{}'.", path);
            } else {
                println!("{content}");
            }
        }
        Some(("restore", sub)) => {
            let path = sub.get_one::<String>("path").unwrap();
            let format = parse_state_format(
                sub.get_one::<String>("format").map(String::as_str),
                Some(path.as_str()),
            )?;
            let content = std::fs::read_to_string(path)
                .map_err(anyhow::Error::from)
                .map_err(|err| {
                    err.context(format!("Failed to read state backup from '{}'", path))
                })?;
            installer.restore_state(&content, format)?;
            println!("Restored state from '{}'.", path);
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
