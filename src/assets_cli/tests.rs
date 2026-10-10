use super::*;
use clap::Parser;

#[derive(Parser)]
struct TestCli {
  #[command(flatten)]
  assets: AssetsCommand,
}

#[test]
fn source_forms_are_explicit_and_named_files_are_required() {
  assert_eq!(
    parse_source("hf://datasets/example/corpus@v1/data/train.csv", None).unwrap(),
    Source::HuggingFace {
      repo_id: "example/corpus".into(),
      repo_type: "dataset".into(),
      filename: "data/train.csv".into(),
      revision: "v1".into(),
      requested_revision: Some("v1".into()),
    }
  );
  assert!(parse_source("hf://example/model", None).is_err());
  assert!(parse_source("https://example.net/data?token=secret", None).is_err());
  assert!(parse_source("expri://user:secret@example.net/demo/inputs/data", None).is_err());
  assert_eq!(
    parse_source("expri://example.net/demo/inputs/data", None).unwrap(),
    Source::Expri {
      url: "https://example.net/".into(),
      project_id: "demo".into(),
      input_id: "data".into(),
    }
  );
  assert_eq!(
    parse_source("expri://[::1]:8787/demo/inputs/data", None).unwrap(),
    Source::Expri {
      url: "https://[::1]:8787/".into(),
      project_id: "demo".into(),
      input_id: "data".into(),
    }
  );
}

#[test]
fn private_source_uses_only_the_matching_client_origin() {
  let temporary = tempfile::tempdir().unwrap();
  let config = temporary.path().canonicalize().unwrap().join("client.toml");
  fs::write(
    &config,
    "url='http://localhost:8787/'\ntoken_env='TEST_TOKEN'\n",
  )
  .unwrap();
  assert_eq!(
    parse_source("expri://localhost:8787/demo/inputs/data", Some(&config)).unwrap(),
    Source::Expri {
      url: "http://localhost:8787/".into(),
      project_id: "demo".into(),
      input_id: "data".into(),
    }
  );
  assert!(parse_source("expri://other.example/demo/inputs/data", Some(&config)).is_err());
  fs::write(
    &config,
    "url='http://localhost:8787/'\ntoken='SECRET_VALUE'\n",
  )
  .unwrap();
  let error = parse_source("expri://localhost:8787/demo/inputs/data", Some(&config))
    .unwrap_err()
    .to_string();
  assert!(!error.contains("SECRET_VALUE"), "{error}");
  fs::write(&config, "x".repeat(32 * 1024 + 1)).unwrap();
  assert!(
    parse_source("expri://localhost:8787/demo/inputs/data", Some(&config))
      .unwrap_err()
      .to_string()
      .contains("size limit")
  );
}

#[test]
fn download_requires_explicit_force_and_import_has_two_operands() {
  assert!(
    TestCli::try_parse_from(["assets", "download", "data/train.csv", "--force", "--json"]).is_ok()
  );
  assert!(
    TestCli::try_parse_from([
      "assets",
      "import",
      "https://example.net/data",
      "data/train.csv"
    ])
    .is_ok()
  );
  assert!(TestCli::try_parse_from(["assets", "import", "https://example.net/data"]).is_err());
  assert!(TestCli::try_parse_from(["assets", "sync"]).is_err());
}

fn cached_descriptor(root: &Path, bytes: &[u8]) -> (PathBuf, Descriptor) {
  let source = root.join(format!("cache-{}", bytes.len()));
  fs::write(&source, bytes).unwrap();
  let (sha256, size) = crate::archive::sha256_file(&source).unwrap();
  (
    source,
    Descriptor {
      version: 1,
      source: Source::Url {
        url: "https://example.net/data.bin".into(),
      },
      size,
      sha256,
    },
  )
}

#[test]
fn import_publication_failure_removes_only_its_new_binding() {
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
fn import_rejects_the_sixty_fifth_descriptor_before_downloading() {
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
  let error = import(&root, &source, Path::new("new-data.bin"), None).unwrap_err();
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
