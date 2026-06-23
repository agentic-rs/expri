pub fn patch_apply_script(patch_digest: &str) -> String {
  format!(
    r#"import pathlib, shutil, zipfile

meta_dir = pathlib.Path(".expri")
patch_dir = meta_dir / "patch"
manifest_path = meta_dir / "patch.manifest"

if manifest_path.exists():
  for line in manifest_path.read_text().splitlines():
    if not line:
      continue
    path = pathlib.Path(line)
    if path.exists() or path.is_symlink():
      path.unlink()

shutil.rmtree(patch_dir, ignore_errors=True)
patch_dir.mkdir(parents=True, exist_ok=True)
with zipfile.ZipFile(meta_dir / "patch.zip") as archive:
  archive.extractall(patch_dir)

deleted = patch_dir / ".deleted"
if deleted.exists():
  for line in deleted.read_text().splitlines():
    if line:
      path = pathlib.Path(line)
      if path.exists() or path.is_symlink():
        path.unlink()
  deleted.unlink()

manifest = []
for src in patch_dir.rglob("*"):
  if src.is_dir():
    continue
  dst = pathlib.Path(src.relative_to(patch_dir))
  dst.parent.mkdir(parents=True, exist_ok=True)
  shutil.copy2(src, dst)
  manifest.append(dst.as_posix())

manifest_path.write_text("".join(f"{{path}}\n" for path in sorted(manifest)))
(meta_dir / "patch.sha256").write_text("{patch_digest}")
"#
  )
}
