
use super::*;
use flate2::{Compression, write::GzEncoder};

fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (path, contents, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder.append(&header, *contents).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn mappings() -> Vec<ResolvedPluginFile> {
    vec![
        ResolvedPluginFile {
            from: "pkg-1.0.0/agent".to_string(),
            root: FileRoot::InstallDir,
            relative_path: PathBuf::from("agent"),
            executable: true,
        },
        ResolvedPluginFile {
            from: "pkg-1.0.0/share/pkg".to_string(),
            root: FileRoot::DataHome,
            relative_path: PathBuf::from("pkg"),
            executable: false,
        },
        ResolvedPluginFile {
            from: "pkg-1.0.0/share/pkg/plugins".to_string(),
            root: FileRoot::ConfigHome,
            relative_path: PathBuf::from("pkg/plugins"),
            executable: false,
        },
    ]
}

#[test]
fn test_extract_install_payload_reads_additional_files_and_tree_mappings() {
    let archive = tar_gz(&[
        ("pkg-1.0.0/pkg", b"binary", 0o755),
        ("pkg-1.0.0/agent", b"agent", 0o644),
        ("pkg-1.0.0/share/pkg/grammars/rust.so", b"grammar", 0o644),
        (
            "pkg-1.0.0/share/pkg/plugins/xi-lsp/manifest.toml",
            b"manifest",
            0o644,
        ),
        (
            "pkg-1.0.0/share/pkg/plugins/xi-lsp/bin/xi-lsp",
            b"lsp",
            0o755,
        ),
    ]);

    let payload = extract_install_payload(
        "pkg-1.0.0-x86_64-unknown-linux-musl.tar.gz",
        &archive,
        "pkg-1.0.0/pkg",
        &[],
        &mappings(),
        "pkg",
    )
    .unwrap();

    assert_eq!(payload.binary_filename, "pkg");
    assert_eq!(payload.binary_contents, b"binary");

    let agent = payload
        .files
        .iter()
        .find(|file| file.root == FileRoot::InstallDir)
        .expect("agent mapping");
    assert_eq!(agent.relative_path, PathBuf::from("agent"));
    assert_eq!(agent.contents, b"agent");
    assert!(agent.executable, "explicit executable flag should win");

    // The tree mapping also carries the nested plugins subtree, mirroring
    // the upstream installer.
    let data_files = payload
        .files
        .iter()
        .filter(|file| file.root == FileRoot::DataHome)
        .collect::<Vec<_>>();
    assert_eq!(data_files.len(), 3);
    let grammar = data_files
        .iter()
        .find(|file| file.relative_path == Path::new("pkg/grammars/rust.so"))
        .expect("grammar mapping");
    assert!(
        !grammar.executable,
        "archive mode 0644 stays non-executable"
    );

    let config_files = payload
        .files
        .iter()
        .filter(|file| file.root == FileRoot::ConfigHome)
        .collect::<Vec<_>>();
    assert_eq!(config_files.len(), 2);
    let lsp = config_files
        .iter()
        .find(|file| file.relative_path == Path::new("pkg/plugins/xi-lsp/bin/xi-lsp"))
        .expect("lsp mapping");
    assert!(lsp.executable, "archive mode 0755 should stay executable");
    let manifest = config_files
        .iter()
        .find(|file| file.relative_path == Path::new("pkg/plugins/xi-lsp/manifest.toml"))
        .expect("manifest mapping");
    assert!(!manifest.executable);
}

#[test]
fn test_extract_install_payload_errors_when_declared_file_is_missing() {
    let archive = tar_gz(&[("pkg-1.0.0/pkg", b"binary", 0o755)]);

    let error = extract_install_payload(
        "pkg.tar.gz",
        &archive,
        "pkg-1.0.0/pkg",
        &[],
        &mappings(),
        "pkg",
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("Additional file 'pkg-1.0.0/agent' was not found"),
        "unexpected error: {error}"
    );
}

#[test]
fn test_commit_install_writes_files_and_records_checksums() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("bin");
    let man_dir = temp.path().join("man");
    let data_home = temp.path().join("data");
    let config_home = temp.path().join("config");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&man_dir).unwrap();

    let payload = InstallPayload {
        binary_filename: "pkg".to_string(),
        binary_contents: b"binary".to_vec(),
        man_pages: Vec::new(),
        files: vec![
            PayloadFile {
                root: FileRoot::InstallDir,
                relative_path: PathBuf::from("pkg-agent"),
                contents: b"agent".to_vec(),
                executable: true,
            },
            PayloadFile {
                root: FileRoot::DataHome,
                relative_path: PathBuf::from("pkg/grammars/rust.so"),
                contents: b"grammar".to_vec(),
                executable: false,
            },
            PayloadFile {
                root: FileRoot::ConfigHome,
                relative_path: PathBuf::from("pkg/plugins/manifest.toml"),
                contents: b"manifest".to_vec(),
                executable: false,
            },
        ],
    };

    let installed = commit_install(
        InstallRoots {
            install_dir: &install_dir,
            man_dir: &man_dir,
            data_home: &data_home,
            config_home: &config_home,
        },
        payload,
    )
    .unwrap();

    assert_eq!(installed.files.len(), 3);
    assert_eq!(fs::read(install_dir.join("pkg-agent")).unwrap(), b"agent");
    assert_eq!(
        fs::read(data_home.join("pkg/grammars/rust.so")).unwrap(),
        b"grammar"
    );
    assert_eq!(
        fs::read(config_home.join("pkg/plugins/manifest.toml")).unwrap(),
        b"manifest"
    );

    let agent = installed
        .files
        .iter()
        .find(|file| file.root == "install_dir")
        .expect("agent record");
    assert_eq!(agent.path, "pkg-agent");
    assert_eq!(agent.checksum_sha256, sha256_hex(b"agent"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(install_dir.join("pkg-agent"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "agent should be executable");
        let mode = fs::metadata(data_home.join("pkg/grammars/rust.so"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0, "data files should not be executable");
    }
}

#[test]
fn test_commit_install_replaces_existing_additional_files() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("bin");
    let man_dir = temp.path().join("man");
    let data_home = temp.path().join("data");
    let config_home = temp.path().join("config");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&man_dir).unwrap();
    fs::create_dir_all(data_home.join("pkg")).unwrap();
    fs::write(data_home.join("pkg/old.txt"), b"old").unwrap();

    let payload = InstallPayload {
        binary_filename: "pkg".to_string(),
        binary_contents: b"binary".to_vec(),
        man_pages: Vec::new(),
        files: vec![PayloadFile {
            root: FileRoot::DataHome,
            relative_path: PathBuf::from("pkg/old.txt"),
            contents: b"new".to_vec(),
            executable: false,
        }],
    };

    commit_install(
        InstallRoots {
            install_dir: &install_dir,
            man_dir: &man_dir,
            data_home: &data_home,
            config_home: &config_home,
        },
        payload,
    )
    .unwrap();

    assert_eq!(fs::read(data_home.join("pkg/old.txt")).unwrap(), b"new");
}
