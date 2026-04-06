use clap::{Arg, ArgAction, Command};

pub(crate) fn build_cli(version: &'static str, description: &'static str) -> Command {
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

    let quiet_arg = Arg::new("quiet")
        .long("quiet")
        .short('q')
        .global(true)
        .action(ArgAction::SetTrue)
        .conflicts_with("verbose")
        .help("Reduce output to warnings and errors");

    let verbose_arg = Arg::new("verbose")
        .long("verbose")
        .short('v')
        .global(true)
        .action(ArgAction::Count)
        .help("Increase output verbosity (-v for debug, -vv for trace)");

    let refresh_arg = Arg::new("refresh")
        .long("refresh")
        .global(true)
        .action(ArgAction::SetTrue)
        .help("Force refresh remote plugin indexes instead of using cached TTL");

    let target_arg = Arg::new("target")
        .long("target")
        .action(ArgAction::Set)
        .help("Override the resolved release target triple");

    Command::new("scpr")
        .bin_name("scpr")
        .version(version)
        .about(description)
        .arg_required_else_help(true)
        .propagate_version(true)
        .arg(quiet_arg)
        .arg(verbose_arg)
        .arg(refresh_arg)
        .subcommand(
            Command::new("install")
                .about("Install one or more packages")
                .arg(
                    Arg::new("packages")
                        .required(true)
                        .num_args(1..)
                        .help("Plugin name or alias, optionally as <name>@<tag>"),
                )
                .arg(
                    Arg::new("tag")
                        .long("tag")
                        .action(ArgAction::Set)
                        .help("Install a specific release tag (single package only)"),
                )
                .arg(target_arg.clone())
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
                .arg(target_arg.clone())
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
                                .help("Case-insensitive plugin name, alias, or description filter")
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
                )
                .subcommand(
                    Command::new("new")
                        .about("Generate a plugin skeleton from a GitHub repo's latest release")
                        .arg(
                            Arg::new("repo")
                                .required(true)
                                .help("GitHub repo in the form <owner>/<repo>"),
                        )
                        .arg(
                            Arg::new("output")
                                .long("output")
                                .short('o')
                                .action(ArgAction::Set)
                                .help("Write the generated plugin TOML to a specific path"),
                        )
                        .arg(
                            Arg::new("stdout")
                                .long("stdout")
                                .action(ArgAction::SetTrue)
                                .help("Print the generated plugin TOML to stdout"),
                        ),
                )
                .subcommand(
                    Command::new("index")
                        .about("Manage remote plugin indexes")
                        .subcommand(
                            Command::new("add")
                                .about("Add a remote plugin index from a GitHub repository")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("list")
                                .about("List configured remote plugin indexes")
                                .arg(json_arg.clone()),
                        )
                        .subcommand(
                            Command::new("enable")
                                .about("Enable a configured remote plugin index")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("disable")
                                .about("Disable a configured remote plugin index")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("remove")
                                .about("Remove a configured remote plugin index")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("sync")
                                .about("Sync one configured remote plugin index, or all with --all")
                                .arg(
                                    Arg::new("repo")
                                        .required(false)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                )
                                .arg(
                                    Arg::new("all")
                                        .long("all")
                                        .action(ArgAction::SetTrue)
                                        .help("Sync all enabled remote plugin indexes"),
                                ),
                        )
                        .subcommand(
                            Command::new("promote")
                                .about("Move an index earlier in plugin resolution order")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("demote")
                                .about("Move an index later in plugin resolution order")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("pin")
                                .about("Pin a plugin to prefer a specific remote plugin index")
                                .arg(
                                    Arg::new("plugin")
                                        .required(true)
                                        .help("Plugin name or alias"),
                                )
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("unpin")
                                .about("Remove a plugin's preferred remote plugin index")
                                .arg(
                                    Arg::new("plugin")
                                        .required(true)
                                        .help("Plugin name"),
                                ),
                        )
                        .subcommand(
                            Command::new("pins")
                                .about("List plugin-specific remote index pins")
                                .arg(json_arg.clone()),
                        ),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("List all installed packages")
                .arg(
                    Arg::new("outdated")
                        .long("outdated")
                        .action(ArgAction::SetTrue)
                        .help("Include latest-version status inline"),
                )
                .arg(json_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("outdated")
                .about("List installed packages with newer releases")
                .arg(
                    Arg::new("package")
                        .required(false)
                        .help("Installed package name or alias"),
                )
                .arg(json_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(Command::new("doctor").about("Check the local installer setup"))
        .subcommand(
            Command::new("status")
                .about("Show installed packages (alias for list)")
                .arg(
                    Arg::new("outdated")
                        .long("outdated")
                        .action(ArgAction::SetTrue)
                        .help("Include latest-version status inline"),
                )
                .arg(json_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(Command::new("verify").about("Alias for audit"))
        .subcommand(
            Command::new("audit")
                .about("Audit installed binaries for missing or modified files")
                .arg(json_arg.clone()),
        )
        .subcommand(
            Command::new("history")
                .about("Show package install, update, remove, and pin history")
                .args_conflicts_with_subcommands(true)
                .arg(
                    Arg::new("package")
                        .required(false)
                        .help("Filter history to a single package"),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .action(ArgAction::Set)
                        .value_parser(clap::value_parser!(usize))
                        .help("Show only the most recent N history events"),
                )
                .arg(
                    Arg::new("graph")
                        .long("graph")
                        .action(ArgAction::SetTrue)
                        .help("Group history by package as a simple movement graph"),
                )
                .arg(json_arg.clone())
                .subcommand(
                    Command::new("clear")
                        .about("Clear history for all packages, or one package if provided")
                        .arg(
                            Arg::new("package")
                                .required(false)
                                .help("Clear history only for one package"),
                        ),
                ),
        )
        .subcommand(
            Command::new("export")
                .about("Export installed state and history")
                .arg(
                    Arg::new("path")
                        .required(false)
                        .help("Write to a file instead of stdout"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .action(ArgAction::Set)
                        .default_value("json")
                        .value_parser(["json", "toml"])
                        .help("Export format"),
                ),
        )
        .subcommand(
            Command::new("restore")
                .about("Restore installed state and history from a backup file")
                .arg(
                    Arg::new("path")
                        .required(true)
                        .help("Backup file to restore from"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .action(ArgAction::Set)
                        .value_parser(["json", "toml"])
                        .help("Backup format (defaults to file extension)"),
                ),
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
        )
}
