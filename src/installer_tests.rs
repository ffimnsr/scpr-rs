use super::{
    AuditStatus, HistoryAction, InstalledPackage, Installer, LegacyStateV0,
    STATE_VERSION, State, StateFormat,
};
use crate::installer_archive::{
    InstallPayload, parse_sha256_checksum_file, parse_sha256_digest,
};
use crate::plugin::{Plugin, PluginCompletions};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn temp_installer() -> Installer {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.keep();
    let local_bin = root.join("bin");
    let local_man = root.join("man");
    let state_dir = root.join("state");
    std::fs::create_dir_all(&local_bin).unwrap();
    std::fs::create_dir_all(&local_man).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    Installer {
        local_bin,
        local_man,
        state_file: state_dir.join("state.toml"),
        lock_stale_after_secs: 300,
    }
}

fn sample_plugin() -> Plugin {
    Plugin {
        name: "ripgrep".to_string(),
        alias: vec!["rg".to_string()],
        description: Some("sample".to_string()),
        location: "github:BurntSushi/ripgrep".to_string(),
        asset_pattern: "{name}-{version}-{target}.tar.gz".to_string(),
        checksum_asset_pattern: Some(
            "{name}-{version}-{target}.tar.gz.sha256".to_string(),
        ),
        allow_insecure_no_checksum: false,
        signature_asset_pattern: None,
        signature_format: None,
        signature_key: None,
        build_branch: None,
        build_script: None,
        post_build: None,
        binary: "{name}-{version}-{target}/rg".to_string(),
        man_pages: Some(vec!["{name}-{version}-{target}/doc/rg.1".to_string()]),
        completions: None,
        post_install: None,
        post_uninstall: None,
        cleanup: None,
        targets: None,
    }
}

fn completion_plugin() -> Plugin {
    let mut plugin = sample_plugin();
    plugin.completions = Some(PluginCompletions {
        command: "{binary_path} {shell}".to_string(),
    });
    plugin
}

fn with_env_vars<R>(pairs: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let previous = pairs
        .iter()
        .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
        .collect::<Vec<_>>();
    for (key, value) in pairs {
        unsafe {
            std::env::set_var(key, value);
        }
    }
    let result = f();
    for (key, value) in previous {
        match value {
            Some(value) => unsafe {
                std::env::set_var(&key, value);
            },
            None => unsafe {
                std::env::remove_var(&key);
            },
        }
    }
    result
}

#[cfg(unix)]
fn write_test_completion_binary(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, b"#!/bin/sh\nprintf 'generated-%s\\n' \"$1\"\n").unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn sample_installed_package(name: &str, version: &str, binary: &str) -> InstalledPackage {
    InstalledPackage {
        name: name.to_string(),
        version: version.to_string(),
        binary: binary.to_string(),
        source: None,
        target: None,
        asset_name: None,
        checksum_sha256: Some("a".repeat(64)),
        binary_checksum_sha256: Some("a".repeat(64)),
        man_pages: Vec::new(),
        installed_at_unix: Some(1),
        binary_mode: None,
        binary_owner_uid: None,
        binary_owner_gid: None,
        pinned: false,
    }
}

#[test]
fn test_parse_sha256_digest_accepts_prefixed_value() {
    let value = parse_sha256_digest(
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    assert_eq!(
        value,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[test]
fn test_parse_sha256_checksum_file_matches_asset_name() {
    let checksum = parse_sha256_checksum_file(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ripgrep.tar.gz",
        "ripgrep.tar.gz",
    )
    .unwrap();
    assert_eq!(
        checksum,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[test]
fn test_parse_sha256_checksum_file_accepts_bsd_format() {
    let checksum = parse_sha256_checksum_file(
        "SHA256 (ripgrep.tar.gz) = 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "ripgrep.tar.gz",
    )
    .unwrap();
    assert_eq!(
        checksum,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[test]
fn test_parse_sha256_checksum_file_accepts_single_value() {
    let checksum = parse_sha256_checksum_file(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "ignored",
    )
    .unwrap();
    assert_eq!(
        checksum,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}

#[tokio::test]
async fn test_acquire_state_lock_blocks_when_lock_exists() {
    let installer = temp_installer();
    let lock_path = installer.state_file_path().with_extension("lock");
    std::fs::write(&lock_path, b"busy").unwrap();

    let error = installer.acquire_state_lock().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Timed out waiting for installer lock")
    );

    std::fs::remove_file(lock_path).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn test_acquire_state_lock_clears_stale_lock() {
    let mut installer = temp_installer();
    installer.lock_stale_after_secs = 0;
    let lock_path = installer.state_file_path().with_extension("lock");
    std::fs::write(&lock_path, b"stale").unwrap();

    let _lock = installer.acquire_state_lock().await.unwrap();
    assert!(lock_path.exists());
}

#[tokio::test]
async fn test_state_lock_removed_on_drop() {
    let installer = temp_installer();
    let lock_path: PathBuf = installer.state_file_path().with_extension("lock");

    {
        let _lock = installer.acquire_state_lock().await.unwrap();
        assert!(lock_path.exists());
    }

    assert!(!lock_path.exists());
}

#[test]
fn test_commit_install_writes_binary_and_man_page() {
    let installer = temp_installer();
    let payload = InstallPayload {
        binary_filename: "rg".to_string(),
        binary_contents: b"binary".to_vec(),
        man_pages: vec![("rg.1".to_string(), b"manual".to_vec())],
    };

    let installed = installer.commit_install(payload).unwrap();

    assert_eq!(installed.binary_filename, "rg");
    assert_eq!(installed.man_page_filenames, vec!["rg.1".to_string()]);
    assert_eq!(
        std::fs::read(installer.local_bin_dir().join("rg")).unwrap(),
        b"binary"
    );
    assert_eq!(
        std::fs::read(installer.local_man_dir().join("rg.1")).unwrap(),
        b"manual"
    );
}

#[test]
fn test_commit_install_cleans_orphaned_backup_files() {
    let installer = temp_installer();
    let backup_path = installer
        .local_bin_dir()
        .join(format!("rg.scpr-old.{}.0", std::process::id()));
    std::fs::write(&backup_path, b"stale").unwrap();

    let payload = InstallPayload {
        binary_filename: "rg".to_string(),
        binary_contents: b"binary".to_vec(),
        man_pages: Vec::new(),
    };

    let _installed = installer.commit_install(payload).unwrap();
    assert!(!backup_path.exists());
}

#[cfg(unix)]
#[test]
fn test_managed_completion_install_writes_bash_drop_in_and_profile_line() {
    let installer = temp_installer();
    let plugin = completion_plugin();
    let home = tempfile::tempdir().unwrap();
    let binary_path = installer.local_bin_dir().join("rg");
    write_test_completion_binary(&binary_path);

    with_env_vars(
        &[
            ("HOME", home.path().to_str().unwrap()),
            ("SHELL", "/bin/bash"),
        ],
        || {
            installer
                .run_managed_completion_install(&plugin, "rg", false)
                .unwrap();
        },
    );

    let completion_file = home.path().join(".bashrc.d").join("ripgrep.sh");
    let profile = home.path().join(".bashrc");
    assert_eq!(
        std::fs::read_to_string(completion_file).unwrap(),
        "generated-bash\n"
    );
    assert_eq!(
        std::fs::read_to_string(profile).unwrap(),
        "[ -f \"$HOME/.bashrc.d/ripgrep.sh\" ] && . \"$HOME/.bashrc.d/ripgrep.sh\"\n"
    );
}

#[cfg(unix)]
#[test]
fn test_managed_completion_install_skips_profile_line_when_loader_exists() {
    let installer = temp_installer();
    let plugin = completion_plugin();
    let home = tempfile::tempdir().unwrap();
    let binary_path = installer.local_bin_dir().join("rg");
    write_test_completion_binary(&binary_path);
    let profile = home.path().join(".bashrc");
    std::fs::write(
        &profile,
        "for f in \"$HOME/.bashrc.d/\"*.sh; do\n  [ -r \"$f\" ] && . \"$f\"\ndone\n",
    )
    .unwrap();

    with_env_vars(
        &[
            ("HOME", home.path().to_str().unwrap()),
            ("SHELL", "/bin/bash"),
        ],
        || {
            installer
                .run_managed_completion_install(&plugin, "rg", false)
                .unwrap();
        },
    );

    assert_eq!(
        std::fs::read_to_string(profile).unwrap(),
        "for f in \"$HOME/.bashrc.d/\"*.sh; do\n  [ -r \"$f\" ] && . \"$f\"\ndone\n"
    );
}

#[cfg(unix)]
#[test]
fn test_managed_completion_uninstall_removes_generated_files_and_profile_line() {
    let installer = temp_installer();
    let plugin = completion_plugin();
    let home = tempfile::tempdir().unwrap();
    let bash_file = home.path().join(".bashrc.d").join("ripgrep.sh");
    let zsh_file = home.path().join(".zshrc.d").join("ripgrep.sh");
    let fish_file = home
        .path()
        .join(".config")
        .join("fish")
        .join("completions")
        .join("ripgrep.fish");
    std::fs::create_dir_all(bash_file.parent().unwrap()).unwrap();
    std::fs::create_dir_all(zsh_file.parent().unwrap()).unwrap();
    std::fs::create_dir_all(fish_file.parent().unwrap()).unwrap();
    std::fs::write(&bash_file, "bash completion\n").unwrap();
    std::fs::write(&zsh_file, "zsh completion\n").unwrap();
    std::fs::write(&fish_file, "fish completion\n").unwrap();
    std::fs::write(
        home.path().join(".bashrc"),
        "first\n[ -f \"$HOME/.bashrc.d/ripgrep.sh\" ] && . \"$HOME/.bashrc.d/ripgrep.sh\"\nlast\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".zshrc"),
        "[ -f \"$HOME/.zshrc.d/ripgrep.sh\" ] && . \"$HOME/.zshrc.d/ripgrep.sh\"\n",
    )
    .unwrap();

    with_env_vars(&[("HOME", home.path().to_str().unwrap())], || {
        installer
            .run_managed_completion_uninstall(&plugin, false)
            .unwrap();
    });

    assert!(!bash_file.exists());
    assert!(!zsh_file.exists());
    assert!(!fish_file.exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join(".bashrc")).unwrap(),
        "first\nlast\n"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join(".zshrc")).unwrap(),
        ""
    );
}

#[cfg(unix)]
#[test]
fn test_managed_completion_install_writes_fish_completion_file() {
    let installer = temp_installer();
    let plugin = completion_plugin();
    let home = tempfile::tempdir().unwrap();
    let binary_path = installer.local_bin_dir().join("rg");
    write_test_completion_binary(&binary_path);

    with_env_vars(
        &[
            ("HOME", home.path().to_str().unwrap()),
            ("SHELL", "/usr/bin/fish"),
        ],
        || {
            installer
                .run_managed_completion_install(&plugin, "rg", false)
                .unwrap();
        },
    );

    assert_eq!(
        std::fs::read_to_string(
            home.path()
                .join(".config")
                .join("fish")
                .join("completions")
                .join("ripgrep.fish")
        )
        .unwrap(),
        "generated-fish\n"
    );
}

#[test]
fn test_uninstall_removes_tracked_files_and_state() {
    let installer = temp_installer();
    let plugin = sample_plugin();
    let binary_path = installer.local_bin_dir().join("rg");
    let man_path = installer.local_man_dir().join("rg.1");

    std::fs::write(&binary_path, b"binary").unwrap();
    std::fs::write(&man_path, b"manual").unwrap();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![InstalledPackage {
                source: Some("github:BurntSushi/ripgrep".to_string()),
                target: Some("x86_64-unknown-linux-musl".to_string()),
                asset_name: Some(
                    "ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz".to_string(),
                ),
                man_pages: vec!["rg.1".to_string()],
                ..sample_installed_package("ripgrep", "v15.1.0", "rg")
            }],
            history: Vec::new(),
        })
        .unwrap();

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(installer.uninstall(&plugin, false))
        .unwrap();

    assert!(!binary_path.exists());
    assert!(!man_path.exists());
    assert!(installer.list_installed().unwrap().is_empty());
    let history = installer.history(Some("ripgrep")).unwrap();
    assert!(matches!(
        history.last().unwrap().action,
        HistoryAction::Removed
    ));
}

#[test]
fn test_audit_detects_modified_binary() {
    let installer = temp_installer();
    let binary_path = installer.local_bin_dir().join("rg");
    std::fs::write(&binary_path, b"modified").unwrap();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![sample_installed_package("ripgrep", "v15.1.0", "rg")],
            history: Vec::new(),
        })
        .unwrap();

    let audit = installer.audit(false).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(matches!(audit[0].status, AuditStatus::Modified));
}

#[test]
fn test_audit_marks_packages_without_checksum_as_untracked() {
    let installer = temp_installer();
    let binary_path = installer.local_bin_dir().join("navi");
    std::fs::write(&binary_path, b"binary").unwrap();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![InstalledPackage {
                name: "navi".to_string(),
                version: "v2.24.0".to_string(),
                binary: "navi".to_string(),
                source: None,
                target: None,
                asset_name: None,
                checksum_sha256: None,
                binary_checksum_sha256: None,
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                binary_mode: None,
                binary_owner_uid: None,
                binary_owner_gid: None,
                pinned: false,
            }],
            history: Vec::new(),
        })
        .unwrap();

    let audit = installer.audit(false).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(matches!(audit[0].status, AuditStatus::Untracked));
    assert!(
        audit[0]
            .detail
            .contains("No stored checksum; cannot verify local changes")
    );
}

#[test]
fn test_audit_marks_legacy_archive_checksum_as_untracked() {
    let installer = temp_installer();
    let binary_path = installer.local_bin_dir().join("mk");
    std::fs::write(&binary_path, b"binary").unwrap();

    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![InstalledPackage {
                name: "mk".to_string(),
                version: "v1.0.0".to_string(),
                binary: "mk".to_string(),
                source: None,
                target: None,
                asset_name: Some("mk-1.0.0-x86_64-unknown-linux-musl.tar.gz".to_string()),
                checksum_sha256: Some("a".repeat(64)),
                binary_checksum_sha256: None,
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                binary_mode: None,
                binary_owner_uid: None,
                binary_owner_gid: None,
                pinned: false,
            }],
            history: Vec::new(),
        })
        .unwrap();

    let audit = installer.audit(false).unwrap();
    assert!(matches!(audit[0].status, AuditStatus::Untracked));
    assert!(
        audit[0]
            .detail
            .contains("archive checksum, not the extracted binary checksum")
    );
}

#[test]
fn test_audit_describe_reports_owner_and_permissions() {
    let installer = temp_installer();
    let binary_path = installer.local_bin_dir().join("rg");
    std::fs::write(&binary_path, b"binary").unwrap();

    let metadata = std::fs::metadata(&binary_path).unwrap();
    let baseline = super::audit_baseline_from_metadata(&metadata);

    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![InstalledPackage {
                binary_mode: baseline.mode,
                binary_owner_uid: baseline.owner_uid,
                binary_owner_gid: baseline.owner_gid,
                ..sample_installed_package("ripgrep", "v15.1.0", "rg")
            }],
            history: Vec::new(),
        })
        .unwrap();

    let audit = installer.audit(true).unwrap();
    let describe = audit[0].describe.as_ref().unwrap();
    assert_eq!(describe.fingerprint.status, "changed");
    assert!(describe.permissions.is_some());
    assert!(describe.owner.is_some());
}

#[test]
fn test_prompt_cleanup_confirmation_accepts_yes_after_retry() {
    let mut input = Cursor::new(b"maybe\nyes\n".to_vec());
    let mut output = Vec::new();

    let confirmed =
        super::prompt_cleanup_confirmation(&mut input, &mut output, "ripgrep").unwrap();

    assert!(confirmed);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Please answer 'y' or 'n'."));
}

#[test]
fn test_same_version_already_installed_requires_binary_on_disk() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![sample_installed_package("ripgrep", "v15.1.0", "rg")],
            history: Vec::new(),
        })
        .unwrap();

    assert!(
        !installer
            .same_version_already_installed("ripgrep", "v15.1.0")
            .unwrap()
    );

    std::fs::write(installer.local_bin_dir().join("rg"), b"binary").unwrap();

    assert!(
        installer
            .same_version_already_installed("ripgrep", "v15.1.0")
            .unwrap()
    );
}

#[test]
fn test_infer_build_commands_prefers_cargo() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();

    let commands =
        super::infer_build_commands(temp.path(), Some("x86_64-unknown-linux-musl"))
            .unwrap();

    assert_eq!(
        commands,
        vec!["cargo build --release --target x86_64-unknown-linux-musl"]
    );
}

#[test]
fn test_format_build_version_uses_short_sha() {
    let version =
        super::format_build_version("main", "0123456789abcdef0123456789abcdef01234567");

    assert_eq!(version, "main@0123456789ab");
}

#[cfg(unix)]
#[test]
fn test_first_binary_on_path_returns_first_executable() {
    use std::os::unix::fs::PermissionsExt;

    let left = tempfile::tempdir().unwrap();
    let right = tempfile::tempdir().unwrap();
    let left_binary = left.path().join("sqlx");
    let right_binary = right.path().join("sqlx");

    std::fs::write(&left_binary, b"#!/bin/sh\n").unwrap();
    std::fs::write(&right_binary, b"#!/bin/sh\n").unwrap();
    let mut left_permissions = std::fs::metadata(&left_binary).unwrap().permissions();
    left_permissions.set_mode(0o755);
    std::fs::set_permissions(&left_binary, left_permissions).unwrap();
    let mut right_permissions = std::fs::metadata(&right_binary).unwrap().permissions();
    right_permissions.set_mode(0o755);
    std::fs::set_permissions(&right_binary, right_permissions).unwrap();

    let resolved = super::first_binary_in_dirs(
        "sqlx",
        vec![left.path().to_path_buf(), right.path().to_path_buf()],
    )
    .unwrap();

    assert_eq!(resolved, left_binary);
}

#[test]
fn test_pin_records_history() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![sample_installed_package("ripgrep", "v15.1.0", "rg")],
            history: Vec::new(),
        })
        .unwrap();

    installer.pin("ripgrep").unwrap();
    let history = installer.history(Some("ripgrep")).unwrap();
    assert!(matches!(
        history.last().unwrap().action,
        HistoryAction::Pinned
    ));
}

#[test]
fn test_rollback_version_returns_previous_installed_version() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![sample_installed_package("ripgrep", "v2", "rg")],
            history: vec![super::HistoryEvent {
                package: "ripgrep".to_string(),
                action: HistoryAction::Updated,
                timestamp_unix: 2,
                version: Some("v2".to_string()),
                from_version: Some("v1".to_string()),
                to_version: Some("v2".to_string()),
                detail: None,
            }],
        })
        .unwrap();

    assert_eq!(installer.rollback_version("ripgrep").unwrap(), "v1");
}

#[test]
fn test_restore_state_writes_backup_before_overwrite() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![sample_installed_package("ripgrep", "v1", "rg")],
            history: Vec::new(),
        })
        .unwrap();

    let replacement = toml::to_string(&State {
        version: STATE_VERSION,
        installed: Vec::new(),
        history: Vec::new(),
    })
    .unwrap();
    installer
        .restore_state(&replacement, StateFormat::Toml)
        .unwrap();

    let backup =
        std::fs::read_to_string(installer.state_file_path().with_extension("toml.bak"))
            .unwrap();
    assert!(backup.contains("ripgrep"));
}

#[test]
fn test_history_limited_returns_most_recent_events() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: Vec::new(),
            history: vec![
                super::HistoryEvent {
                    package: "ripgrep".to_string(),
                    action: HistoryAction::Installed,
                    timestamp_unix: 1,
                    version: Some("v1".to_string()),
                    from_version: None,
                    to_version: Some("v1".to_string()),
                    detail: None,
                },
                super::HistoryEvent {
                    package: "ripgrep".to_string(),
                    action: HistoryAction::Updated,
                    timestamp_unix: 2,
                    version: Some("v2".to_string()),
                    from_version: Some("v1".to_string()),
                    to_version: Some("v2".to_string()),
                    detail: None,
                },
                super::HistoryEvent {
                    package: "ripgrep".to_string(),
                    action: HistoryAction::Removed,
                    timestamp_unix: 3,
                    version: Some("v2".to_string()),
                    from_version: Some("v2".to_string()),
                    to_version: None,
                    detail: None,
                },
            ],
        })
        .unwrap();

    let history = installer.history_limited(Some("ripgrep"), Some(2)).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].timestamp_unix, 2);
    assert_eq!(history[1].timestamp_unix, 3);
}

#[test]
fn test_export_and_restore_state_json_round_trip() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![sample_installed_package("ripgrep", "v15.1.0", "rg")],
            history: Vec::new(),
        })
        .unwrap();

    let exported = installer.export_state(StateFormat::Json).unwrap();

    let restored = temp_installer();
    restored
        .restore_state(&exported, StateFormat::Json)
        .unwrap();
    let installed = restored.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, "ripgrep");
}

#[test]
fn test_exported_state_includes_schema_version() {
    let installer = temp_installer();
    let exported = installer.export_state(StateFormat::Toml).unwrap();
    assert!(exported.contains(&format!("version = {}", STATE_VERSION)));
}

#[test]
fn test_load_state_rejects_unsupported_schema_version() {
    let installer = temp_installer();
    std::fs::write(
        installer.state_file_path(),
        "version = 99\ninstalled = []\nhistory = []\n",
    )
    .unwrap();

    let error = installer.list_installed().unwrap_err();
    assert!(error.to_string().contains("Unsupported state file version"));
}

#[test]
fn test_load_state_migrates_legacy_v0_without_version() {
    let installer = temp_installer();
    let legacy = toml::to_string(&LegacyStateV0 {
        installed: vec![sample_installed_package("ripgrep", "v15.1.0", "rg")],
        history: Vec::new(),
    })
    .unwrap();
    std::fs::write(installer.state_file_path(), legacy).unwrap();

    let installed = installer.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, "ripgrep");
}

#[test]
fn test_clear_history_removes_matching_events() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: Vec::new(),
            history: vec![
                super::HistoryEvent {
                    package: "ripgrep".to_string(),
                    action: HistoryAction::Installed,
                    timestamp_unix: 1,
                    version: Some("v1".to_string()),
                    from_version: None,
                    to_version: Some("v1".to_string()),
                    detail: None,
                },
                super::HistoryEvent {
                    package: "fd".to_string(),
                    action: HistoryAction::Installed,
                    timestamp_unix: 2,
                    version: Some("v1".to_string()),
                    from_version: None,
                    to_version: Some("v1".to_string()),
                    detail: None,
                },
            ],
        })
        .unwrap();

    let removed = installer.clear_history(Some("ripgrep")).unwrap();
    assert_eq!(removed, 1);
    let history = installer.history(None).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].package, "fd");
}
