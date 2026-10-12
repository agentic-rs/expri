use super::*;
use clap::Parser;

#[derive(Parser)]
struct TestCli {
  #[command(flatten)]
  assets: AssetsCommand,
}

#[test]
fn source_forms_are_relative_and_named_files_are_required() {
  assert_eq!(
    parse_source("hf://datasets/example/corpus@v1/data/train.csv").unwrap(),
    Source::HuggingFace {
      repo_id: "example/corpus".into(),
      repo_type: "dataset".into(),
      filename: "data/train.csv".into(),
      revision: "v1".into(),
      requested_revision: Some("v1".into()),
    }
  );
  assert!(parse_source("hf://example/model").is_err());
  assert!(parse_source("https://example.net/data?token=secret").is_err());
  assert!(parse_source("expri://example.net/demo/inputs/data").is_err());
  assert_eq!(
    parse_source("expri://./inputs/data").unwrap(),
    Source::Expri {
      input_id: "data".into()
    }
  );
  assert!(parse_source("expri://./worker-1/run-1/outputs/data").is_err());
}

#[test]
fn output_reference_keeps_exact_scoped_paths_and_rejects_normalized_escapes() {
  assert_eq!(
    parse_output("expri://./worker-1/run-1/outputs/model%20one.pt").unwrap(),
    assets::Producer {
      origin: "worker-1".into(),
      run_id: "run-1".into(),
      path: "outputs/model one.pt".into(),
    }
  );
  for source in [
    "expri://service.example.net/demo/runs/worker-1/run-1/outputs/model.pt",
    "worker-1/run-1/outputs/model.pt",
    "expri://./worker-1/run-1/outputs/../secret",
    "expri://./worker-1/run-1/outputs/%2e%2e/secret",
    "expri://./worker-1/run-1/outputs/a%2fb.pt",
    "expri://./worker-1/run-1/outputs/model.pt?token=secret",
    "expri://./worker-1/run-1/outputs/model%ff.pt",
    "expri://./worker-1/run-1/outputs/a%2.pt",
  ] {
    assert!(parse_output(source).is_err(), "{source}");
  }
  let filename = "字".repeat(330);
  let encoded = "%E5%AD%97".repeat(330);
  let reference = format!("expri://./worker-1/run-1/outputs/{encoded}");
  assert_eq!(
    parse_output(&reference).unwrap().path,
    format!("outputs/{filename}")
  );
}

#[test]
fn repository_config_is_ignored_and_canonical_aliases_cannot_bypass_tracking_guard() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  assert!(
    Command::new("git")
      .args(["init", "--quiet"])
      .current_dir(&root)
      .status()
      .unwrap()
      .success()
  );
  fs::create_dir(root.join(".expri")).unwrap();
  fs::write(
    root.join(".expri/config.toml"),
    "url='https://service.example.net/'\nproject_id='example-project'\ntoken_env='TEST_TOKEN'\n",
  )
  .unwrap();
  client_config(&root, None).unwrap();
  assert!(git_ignored(&root, Path::new(".expri/config.toml")).unwrap());
  assert!(
    Command::new("git")
      .args(["add", "--force", ".expri/config.toml"])
      .current_dir(&root)
      .status()
      .unwrap()
      .success()
  );
  for explicit in [None, Some(root.join(".expri/../.expri/config.toml"))] {
    assert!(
      client_config(&root, explicit.as_deref())
        .unwrap_err()
        .to_string()
        .contains("Git-ignored")
    );
  }
  #[cfg(unix)]
  {
    let alias = root.join("settings-alias.toml");
    std::os::unix::fs::symlink(root.join(".expri/config.toml"), &alias).unwrap();
    assert!(
      client_config(&root, Some(&alias))
        .unwrap_err()
        .to_string()
        .contains("Git-ignored")
    );
  }
}

#[test]
fn local_config_context_errors_do_not_disclose_credentials() {
  let temporary = tempfile::tempdir().unwrap();
  let config = temporary.path().canonicalize().unwrap().join("client.toml");
  fs::write(
    &config,
    "url='http://localhost:8787/'\nproject_id='example-project'\ntoken_env='TEST_TOKEN'\n",
  )
  .unwrap();
  assert_eq!(project_context(Some(&config)).unwrap().1, "example-project");
  fs::write(
    &config,
    "url='http://localhost:8787/'\ntoken='SECRET_VALUE'\n",
  )
  .unwrap();
  let error = project_context(Some(&config)).unwrap_err().to_string();
  assert!(!error.contains("SECRET_VALUE"), "{error}");
  fs::write(&config, "x".repeat(32 * 1024 + 1)).unwrap();
  assert!(
    project_context(Some(&config))
      .unwrap_err()
      .to_string()
      .contains("32 KiB")
  );
}

#[test]
fn private_project_context_accepts_global_server_settings_and_keeps_scope_explicit() {
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().canonicalize().unwrap().join("config.toml");
  fs::write(&path, "[system_profile]\ninterval_seconds=10\n[server]\nurl='https://service.example.net/'\ntoken='stored-asset-context-test-token'\nproject_id='example-project'\n").unwrap();
  assert_eq!(
    project_context(Some(&path)).unwrap(),
    (path.as_path(), "example-project".into())
  );
  for contents in [
    "[server]\nurl='https://service.example.net/'\ntoken='stored-asset-context-test-token'\n",
    "[server]\nurl='https://service.example.net/'\ntoken='stored-asset-context-test-token'\ntoken_env='SECRET_ENV_NAME'\nproject_id='example-project'\n",
    "[server]\nurl='https://service.example.net/prefix/'\ntoken='stored-asset-context-test-token'\nproject_id='example-project'\n",
  ] {
    fs::write(&path, contents).unwrap();
    let error = project_context(Some(&path)).unwrap_err().to_string();
    assert!(
      !error.contains("stored-asset-context-test-token"),
      "{error}"
    );
    assert!(!error.contains("SECRET_ENV_NAME"), "{error}");
  }
  assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
}

#[test]
fn commands_use_six_verbs_and_pinning_flags_are_explicit() {
  for args in [
    vec!["assets", "add", "data/train.csv"],
    vec![
      "assets",
      "add",
      "https://example.net/data",
      "data/train.csv",
    ],
    vec![
      "assets",
      "add",
      "https://example.net/data",
      "data/train.csv",
      "--no-download",
    ],
    vec![
      "assets",
      "import",
      "expri://./worker-1/run-1/outputs/best.pt",
      "models/base.pt",
    ],
    vec!["assets", "upload"],
    vec![
      "assets",
      "upload",
      "data/train.csv",
      "--input-id",
      "data-v1",
    ],
    vec!["assets", "download", "data/train.csv", "--force", "--json"],
    vec!["assets", "update", "data/train.csv"],
    vec!["assets", "status"],
  ] {
    assert!(TestCli::try_parse_from(args.clone()).is_ok(), "{args:?}");
  }
  for args in [
    vec![
      "assets",
      "import",
      "expri://./worker-1/run-1/outputs/best.pt",
    ],
    vec!["assets", "upload", "--input-id", "data-v1"],
    vec![
      "assets",
      "add",
      "https://example.net/data",
      "data/train.csv",
      "--sha256",
      "abc",
    ],
    vec!["assets", "bind"],
    vec!["assets", "promote"],
    vec!["assets", "sync"],
  ] {
    assert!(TestCli::try_parse_from(args.clone()).is_err(), "{args:?}");
  }
}

fn cached_descriptor(root: &Path, bytes: &[u8]) -> (PathBuf, Descriptor) {
  let source = root.join(format!("cache-{}", bytes.len()));
  fs::write(&source, bytes).unwrap();
  let (sha256, size) = crate::archive::sha256_file(&source).unwrap();
  (
    source,
    Descriptor {
      version: 2,
      availability: None,
      producer: None,
      source: Source::Url {
        url: "https://example.net/data.bin".into(),
      },
      size,
      sha256,
    },
  )
}

fn producer_identity_fixture() -> (Descriptor, expri_core::service::FileRecord) {
  let producer = assets::Producer {
    origin: "worker-1".into(),
    run_id: "run-1".into(),
    path: "outputs/best.pt".into(),
  };
  let descriptor = Descriptor {
    version: assets::DESCRIPTOR_VERSION,
    source: Source::Expri {
      input_id: "checkpoint".into(),
    },
    size: 7,
    sha256: "a".repeat(64),
    availability: None,
    producer: Some(producer.clone()),
  };
  let record = expri_core::service::FileRecord {
    target: expri_core::service::FileTarget::Input {
      project_id: "example-project".into(),
      input_id: "checkpoint".into(),
    },
    size: descriptor.size,
    sha256: Some(descriptor.sha256.clone()),
    storage: expri_core::service::FileStorage::Object,
    provenance: Some(expri_core::service::AssetProvenance {
      producer,
      captured_at: "2026-01-01T00:00:00Z".into(),
      state: Value::Null,
      snapshot: Value::Null,
      environment: Value::Null,
    }),
  };
  (descriptor, record)
}

#[test]
fn remote_identity_accepts_matching_producer_for_imported_assets() {
  let (descriptor, record) = producer_identity_fixture();
  verify_remote_identity(&record, &descriptor).unwrap();
}

#[test]
fn remote_identity_rejects_missing_or_different_producer_for_identical_bytes() {
  let (descriptor, record) = producer_identity_fixture();
  let expected = descriptor.producer.clone().unwrap();
  let variants = [
    None,
    Some(assets::Producer {
      origin: "worker-2".into(),
      ..expected.clone()
    }),
    Some(assets::Producer {
      run_id: "run-2".into(),
      ..expected.clone()
    }),
    Some(assets::Producer {
      path: "outputs/other.pt".into(),
      ..expected
    }),
  ];
  for producer in variants {
    let mut remote = record.clone();
    remote.provenance = producer.map(|producer| expri_core::service::AssetProvenance {
      producer,
      ..record.provenance.clone().unwrap()
    });
    let error = verify_remote_identity(&remote, &descriptor).unwrap_err();
    assert!(
      error.to_string().contains("producing-run provenance"),
      "{error}"
    );
  }
}

#[test]
fn remote_identity_allows_local_and_public_dedup_without_declared_producer() {
  let (mut descriptor, record) = producer_identity_fixture();
  descriptor.producer = None;
  for source in [
    Source::Local,
    Source::Url {
      url: "https://example.net/data.bin".into(),
    },
  ] {
    descriptor.source = source;
    verify_remote_identity(&record, &descriptor).unwrap();
  }
}

#[test]
fn add_publication_failure_removes_only_its_new_binding() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let path = Path::new("data/train.bin");
  let sidecar = root.join(assets::sidecar_path(path));
  let (source, descriptor) = cached_descriptor(&root, b"new-data");
  let error = publish_workspace_asset(&root, path, &source, &descriptor, None, &mut || {
    fs::create_dir(&sidecar)?;
    Ok(())
  })
  .unwrap_err();
  assert!(error.to_string().contains("regular file"));
  assert!(!root.join(path).exists());
  assert!(sidecar.is_dir());
  assert_eq!(fs::read(&source).unwrap(), b"new-data");
  assert_eq!(fs::read_dir(root.join("data")).unwrap().count(), 1);
}

#[test]
fn update_publication_failure_restores_old_workspace_bytes() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let path = Path::new("data/train.bin");
  let sidecar = root.join(assets::sidecar_path(path));
  let (old_source, old_descriptor) = cached_descriptor(&root, b"old");
  publish_workspace_asset(&root, path, &old_source, &old_descriptor, None, &mut || {
    Ok(())
  })
  .unwrap();
  let old_metadata = assets::open(&root.join(path)).unwrap().metadata().unwrap();
  let (new_source, new_descriptor) = cached_descriptor(&root, b"new-data");
  let error = publish_workspace_asset(
    &root,
    path,
    &new_source,
    &new_descriptor,
    Some(&old_descriptor),
    &mut || Err(message("injected sidecar commit failure")),
  )
  .unwrap_err();
  assert!(error.to_string().contains("injected"));
  assert_eq!(fs::read(root.join(path)).unwrap(), b"old");
  assert!(assets::unchanged(
    &old_metadata,
    &assets::open(&root.join(path)).unwrap().metadata().unwrap()
  ));
  assert_eq!(assets::load(&sidecar).unwrap(), old_descriptor);
  assert_eq!(fs::read_dir(root.join("data")).unwrap().count(), 2);
}

#[test]
fn update_handles_a_nonregular_sidecar_and_preserves_external_changes() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let path = Path::new("data/train.bin");
  let sidecar = root.join(assets::sidecar_path(path));
  let (old_source, old_descriptor) = cached_descriptor(&root, b"old");
  publish_workspace_asset(&root, path, &old_source, &old_descriptor, None, &mut || {
    Ok(())
  })
  .unwrap();
  let (new_source, new_descriptor) = cached_descriptor(&root, b"new-data");
  let error = publish_workspace_asset(
    &root,
    path,
    &new_source,
    &new_descriptor,
    Some(&old_descriptor),
    &mut || {
      fs::remove_file(&sidecar)?;
      fs::create_dir(&sidecar)?;
      Ok(())
    },
  )
  .unwrap_err();
  assert!(error.to_string().contains("regular file"));
  assert_eq!(fs::read(root.join(path)).unwrap(), b"old");
  assert!(sidecar.is_dir());
  assert_eq!(fs::read_dir(root.join("data")).unwrap().count(), 2);
}

#[test]
fn failed_preparation_and_modified_workspace_never_change_existing_asset_metadata() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let path = Path::new("data/train.bin");
  let sidecar = root.join(assets::sidecar_path(path));
  let (old_source, old_descriptor) = cached_descriptor(&root, b"old");
  publish_workspace_asset(&root, path, &old_source, &old_descriptor, None, &mut || {
    Ok(())
  })
  .unwrap();
  let (source, mut descriptor) = cached_descriptor(&root, b"new-data");
  descriptor.size += 1;
  assert!(
    publish_workspace_asset(
      &root,
      path,
      &source,
      &descriptor,
      Some(&old_descriptor),
      &mut || Ok(())
    )
    .is_err()
  );
  assert_eq!(fs::read(root.join(path)).unwrap(), b"old");
  assert_eq!(assets::load(&sidecar).unwrap(), old_descriptor);
  descriptor.size -= 1;
  fs::write(root.join("modified"), b"user-edits").unwrap();
  fs::rename(root.join("modified"), root.join(path)).unwrap();
  assert!(
    publish_workspace_asset(
      &root,
      path,
      &source,
      &descriptor,
      Some(&old_descriptor),
      &mut || Ok(())
    )
    .is_err()
  );
  assert_eq!(fs::read(root.join(path)).unwrap(), b"user-edits");
  assert_eq!(assets::load(&sidecar).unwrap(), old_descriptor);
  assert_eq!(fs::read_dir(root.join("data")).unwrap().count(), 2);
}

#[test]
fn add_rejects_the_sixty_fifth_descriptor_before_downloading() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let (_source, descriptor) = cached_descriptor(&root, b"dataset");
  for index in 0..assets::MAX_ASSETS {
    assets::save(
      &root.join(format!("asset-{index}.bin.expri.toml")),
      &descriptor,
    )
    .unwrap();
  }
  let source = Source::Url {
    url: "https://unavailable.invalid/new-data.bin".into(),
  };
  let error = add_source(&root, &source, Path::new("new-data.bin"), None, false, None).unwrap_err();
  assert!(error.to_string().contains("at most 64"), "{error}");
  assert!(!root.join("new-data.bin").exists());
  assert!(!root.join("new-data.bin.expri.toml").exists());
  assert!(!root.join(".expri/assets").exists());
  assert_eq!(assets::discover(&root).unwrap().len(), assets::MAX_ASSETS);
}

#[test]
fn gitignore_escapes_literal_asset_names_and_preserves_existing_rules() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  fs::write(root.join(".gitignore"), ".expri/").unwrap();
  ignore(&root, Path::new("data/a [1]*.bin")).unwrap();
  ignore(&root, Path::new("data/a [1]*.bin")).unwrap();
  assert_eq!(
    fs::read_to_string(root.join(".gitignore")).unwrap(),
    ".expri/\n.expri-asset-*/\n/data/a\\ \\[1\\]\\*.bin\n!/data/a\\ \\[1\\]\\*.bin.expri.toml\n"
  );
}
