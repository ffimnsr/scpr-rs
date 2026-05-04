use super::{
    AuditStatus, HistoryAction, InstalledPackage, Installer, LegacyStateV0,
    STATE_VERSION, State, StateFormat,
};
use crate::installer_archive::{
    InstallPayload, parse_sha256_checksum_file, parse_sha256_digest,
};
use crate::plugin::Plugin;
use std::path::PathBuf;

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
        binary: "{name}-{version}-{target}/rg".to_string(),
        man_pages: Some(vec!["{name}-{version}-{target}/doc/rg.1".to_string()]),
        post_install: None,
        targets: None,
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
                name: "ripgrep".to_string(),
                version: "v15.1.0".to_string(),
                binary: "rg".to_string(),
                source: Some("github:BurntSushi/ripgrep".to_string()),
                target: Some("x86_64-unknown-linux-musl".to_string()),
                asset_name: Some(
                    "ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz".to_string(),
                ),
                checksum_sha256: Some("a".repeat(64)),
                man_pages: vec!["rg.1".to_string()],
                installed_at_unix: Some(1),
                pinned: false,
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
            installed: vec![InstalledPackage {
                name: "ripgrep".to_string(),
                version: "v15.1.0".to_string(),
                binary: "rg".to_string(),
                source: None,
                target: None,
                asset_name: None,
                checksum_sha256: Some("a".repeat(64)),
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                pinned: false,
            }],
            history: Vec::new(),
        })
        .unwrap();

    let audit = installer.audit().unwrap();
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
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                pinned: false,
            }],
            history: Vec::new(),
        })
        .unwrap();

    let audit = installer.audit().unwrap();
    assert_eq!(audit.len(), 1);
    assert!(matches!(audit[0].status, AuditStatus::Untracked));
    assert!(
        audit[0]
            .detail
            .contains("No stored checksum; cannot verify local changes")
    );
}

#[test]
fn test_pin_records_history() {
    let installer = temp_installer();
    installer
        .save_state(&State {
            version: STATE_VERSION,
            installed: vec![InstalledPackage {
                name: "ripgrep".to_string(),
                version: "v15.1.0".to_string(),
                binary: "rg".to_string(),
                source: None,
                target: None,
                asset_name: None,
                checksum_sha256: Some("a".repeat(64)),
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                pinned: false,
            }],
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
            installed: vec![InstalledPackage {
                name: "ripgrep".to_string(),
                version: "v2".to_string(),
                binary: "rg".to_string(),
                source: None,
                target: None,
                asset_name: None,
                checksum_sha256: Some("a".repeat(64)),
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                pinned: false,
            }],
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
            installed: vec![InstalledPackage {
                name: "ripgrep".to_string(),
                version: "v1".to_string(),
                binary: "rg".to_string(),
                source: None,
                target: None,
                asset_name: None,
                checksum_sha256: Some("a".repeat(64)),
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                pinned: false,
            }],
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
            installed: vec![InstalledPackage {
                name: "ripgrep".to_string(),
                version: "v15.1.0".to_string(),
                binary: "rg".to_string(),
                source: None,
                target: None,
                asset_name: None,
                checksum_sha256: Some("a".repeat(64)),
                man_pages: Vec::new(),
                installed_at_unix: Some(1),
                pinned: false,
            }],
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
        installed: vec![InstalledPackage {
            name: "ripgrep".to_string(),
            version: "v15.1.0".to_string(),
            binary: "rg".to_string(),
            source: None,
            target: None,
            asset_name: None,
            checksum_sha256: Some("a".repeat(64)),
            man_pages: Vec::new(),
            installed_at_unix: Some(1),
            pinned: false,
        }],
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
