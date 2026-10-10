use super::*;

fn descriptor() -> Descriptor {
  Descriptor {
    version: 1,
    source: Source::Url {
      url: "https://example.com/train.parquet?download=1".into(),
    },
    size: 7,
    sha256: "1".repeat(64),
  }
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  (temporary, root)
}

#[test]
fn descriptor_roundtrip_is_strict_versioned_and_atomic() {
  let (_temporary, root) = fixture();
  let path = root.join("data/train.parquet.expri.toml");
  let first = descriptor();
  save(&path, &first).unwrap();
  assert_eq!(load(&path).unwrap(), first);
  let mut second = first;
  second.size = 8;
  second.sha256 = "2".repeat(64);
  save(&path, &second).unwrap();
  assert_eq!(load(&path).unwrap(), second);
  let contents = fs::read_to_string(&path).unwrap();
  fs::write(&path, format!("unknown='ignored'\n{contents}")).unwrap();
  assert!(load(&path).is_err());
  fs::write(
    &path,
    contents.replace("kind = \"url\"", "kind = \"url\"\nsecret = 'ignored'"),
  )
  .unwrap();
  assert!(load(&path).is_err());
  second.version = 2;
  assert!(save(&path, &second).is_err());
  assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn fingerprints_and_descriptors_have_bounded_formats() {
  let mut value = descriptor();
  for digest in ["", "abc", &"A".repeat(64), &"g".repeat(64), &"1".repeat(65)] {
    value.sha256 = digest.to_string();
    assert!(value.validate().is_err(), "accepted {digest}");
  }
  let (_temporary, root) = fixture();
  let path = root.join("large.expri.toml");
  fs::write(&path, vec![b'x'; MAX_DESCRIPTOR_BYTES as usize + 1]).unwrap();
  assert!(load(&path).unwrap_err().to_string().contains("size limit"));
}

#[test]
fn public_source_rejects_credentials_signed_queries_and_ambiguous_urls() {
  for url in [
    "file:///etc/passwd",
    "https://user:password@example.com/file",
    "https://example.com/file#fragment",
    "https://example.com/file?token=secret",
    "https://example.com/file?X-Amz-Signature=secret",
    "https://example.com/file?x-goog-credential=secret",
    "https://example.com/file?AWSAccessKeyId=secret",
    "https://example.com/file?access%5Ftoken=secret",
    " https://example.com/file",
    "https:\\example.com\\file",
  ] {
    assert!(
      Source::Url { url: url.into() }.validate().is_err(),
      "accepted {url}"
    );
  }
  for url in [
    "https://example.com/data?download=1&version=2",
    "http://127.0.0.1:8080/test",
  ] {
    Source::Url { url: url.into() }.validate().unwrap();
  }
}

#[test]
fn hugging_face_resolution_pins_a_commit_without_losing_the_requested_reference() {
  let source = Source::HuggingFace {
    repo_id: "owner/model".into(),
    repo_type: "model".into(),
    filename: "weights/model.safetensors".into(),
    revision: "main".into(),
    requested_revision: Some("main".into()),
  };
  source.validate().unwrap();
  let mut value = descriptor();
  value.source = source;
  assert!(value.validate().is_err());
  if let Source::HuggingFace { revision, .. } = &mut value.source {
    *revision = "a".repeat(40);
  }
  value.validate().unwrap();
  let encoded = toml::to_string(&value).unwrap();
  assert!(encoded.contains("requested_revision = \"main\""));
  for (repo_id, repo_type, filename, revision) in [
    ("owner/too/many", "model", "weights.bin", "main"),
    ("../model", "model", "weights.bin", "main"),
    ("owner/model", "space", "weights.bin", "main"),
    ("owner/model", "dataset", "../weights.bin", "main"),
    ("owner/model", "model", "/weights.bin", "main"),
    ("owner/model", "model", "weights.bin", "../main"),
  ] {
    assert!(
      Source::HuggingFace {
        repo_id: repo_id.into(),
        repo_type: repo_type.into(),
        filename: filename.into(),
        revision: revision.into(),
        requested_revision: None
      }
      .validate()
      .is_err()
    );
  }
}

#[test]
fn private_source_stores_only_an_origin_and_stable_identifiers() {
  for url in [
    "https://expri.example",
    "https://expri.example/",
    "http://127.0.0.1:8000",
  ] {
    Source::Expri {
      url: url.into(),
      project_id: "vision".into(),
      input_id: "dataset-v1".into(),
    }
    .validate()
    .unwrap();
  }
  for (url, project_id, input_id) in [
    ("https://expri.example/api", "vision", "dataset"),
    ("https://expri.example/?token=value", "vision", "dataset"),
    ("https://secret@expri.example/", "vision", "dataset"),
    ("https://expri.example/", "../vision", "dataset"),
    ("https://expri.example/", "vision", "dataset/nested"),
  ] {
    assert!(
      Source::Expri {
        url: url.into(),
        project_id: project_id.into(),
        input_id: input_id.into()
      }
      .validate()
      .is_err()
    );
  }
}

#[test]
fn sidecar_names_preserve_the_file_extension_and_reject_reserved_destinations() {
  let asset = Path::new("data/train.parquet");
  assert_eq!(
    sidecar_path(asset),
    Path::new("data/train.parquet.expri.toml")
  );
  assert_eq!(asset_path(&sidecar_path(asset)).unwrap(), asset);
  for path in [
    ".expri.toml",
    "../file.expri.toml",
    "/file.expri.toml",
    "data//file.expri.toml",
    "data/.secret.expri.toml",
    "data/file.expri.toml.expri.toml",
    "pyproject.toml.expri.toml",
    "uv.lock.expri.toml",
    "expri.worker.toml.expri.toml",
    "src/.git/config.expri.toml",
    "target/file.expri.toml",
  ] {
    assert!(asset_path(Path::new(path)).is_err(), "accepted {path}");
  }
  assert!(asset_path(Path::new("data/train.parquet")).is_err());
}

#[test]
fn discovery_orders_assets_and_skips_generated_and_private_directories() {
  let (_temporary, root) = fixture();
  save(&root.join("models/weights.bin.expri.toml"), &descriptor()).unwrap();
  save(&root.join("data/train.parquet.expri.toml"), &descriptor()).unwrap();
  for directory in [
    ".git",
    ".expri",
    ".venv",
    "target",
    "node_modules",
    "__pycache__",
  ] {
    fs::create_dir(root.join(directory)).unwrap();
    fs::write(
      root.join(directory).join("bad.expri.toml"),
      "not valid TOML",
    )
    .unwrap();
  }
  let assets = discover(&root).unwrap();
  assert_eq!(
    assets
      .iter()
      .map(|asset| asset.path.as_path())
      .collect::<Vec<_>>(),
    [
      Path::new("data/train.parquet"),
      Path::new("models/weights.bin")
    ]
  );
  assert_eq!(
    assets[0].sidecar,
    Path::new("data/train.parquet.expri.toml")
  );
  assert_eq!(
    cache_file(&root, &descriptor().sha256),
    root.join(".expri/assets").join("1".repeat(64)).join("file")
  );
}

#[test]
fn discovery_rejects_overlapping_assets_and_limits_descriptor_count() {
  let (_temporary, root) = fixture();
  save(&root.join("data.expri.toml"), &descriptor()).unwrap();
  save(&root.join("data/train.bin.expri.toml"), &descriptor()).unwrap();
  assert!(discover(&root).unwrap_err().to_string().contains("overlap"));
  let (_temporary, root) = fixture();
  for index in 0..=MAX_ASSETS {
    save(
      &root.join(format!("asset-{index}.bin.expri.toml")),
      &descriptor(),
    )
    .unwrap();
  }
  assert!(
    discover(&root)
      .unwrap_err()
      .to_string()
      .contains("at most 64")
  );
}

#[cfg(unix)]
#[test]
fn descriptors_and_destinations_reject_symlinks_without_touching_external_files() {
  use std::os::unix::fs::symlink;
  let (_temporary, root) = fixture();
  let external = tempfile::NamedTempFile::new().unwrap();
  fs::write(external.path(), toml::to_string(&descriptor()).unwrap()).unwrap();
  let sidecar = root.join("data.bin.expri.toml");
  symlink(external.path(), &sidecar).unwrap();
  assert!(load(&sidecar).is_err());
  assert!(save(&sidecar, &descriptor()).is_err());
  assert!(discover(&root).is_err());
  fs::remove_file(&sidecar).unwrap();
  save(&sidecar, &descriptor()).unwrap();
  symlink(external.path(), root.join("data.bin")).unwrap();
  assert!(discover(&root).is_err());
  let outside = tempfile::tempdir().unwrap();
  symlink(outside.path(), root.join("linked")).unwrap();
  assert!(save(&root.join("linked/train.bin.expri.toml"), &descriptor()).is_err());
  assert!(ensure_parent(&root, Path::new("linked/train.bin")).is_err());
  assert!(!outside.path().join("train.bin.expri.toml").exists());
}
