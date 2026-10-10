import { useEffect, useRef, useState } from "react";
import {
  apiUrl,
  artifactScopeMatchesRun,
  dateText,
  parseRunDeepLink,
  type ArtifactCatalog,
  type ArtifactFile,
  type ArtifactScope,
} from "./dashboard_model";

export function safeArtifactPath(path: string): boolean {
  return (
    path.startsWith("outputs/") &&
    new TextEncoder().encode(path).length <= 1024 &&
    !/[\\\u0000-\u001f\u007f-\u009f]/.test(path) &&
    !path
      .split("/")
      .some(
        (part) =>
          part === "" ||
          part.startsWith(".") ||
          part === "cache" ||
          part === "__pycache__" ||
          part === "node_modules",
      )
  );
}
export function artifactCanSelect(
  file: ArtifactFile,
  scope: ArtifactScope | null,
  source_id: string,
  run_id: string,
): boolean {
  if (!safeArtifactPath(file.path)) return false;
  if (source_id.startsWith("hosted-project:") && !artifactScopeMatchesRun(scope, source_id, run_id))
    return false;
  if (artifactDownloadUrl(file, source_id, run_id, scope)) return true;
  return (
    file.download_url === null &&
    file.cloud === true &&
    artifactScopeMatchesRun(scope, source_id, run_id)
  );
}
/** Accept only the catalog's scoped, same-origin artifact endpoint. */
export function artifactDownloadUrl(
  file: ArtifactFile,
  source_id: string,
  run_id: string,
  scope: ArtifactScope | null = null,
): string | null {
  if (source_id.startsWith("hosted-project:") && !artifactScopeMatchesRun(scope, source_id, run_id))
    return null;
  const value = file.download_url;
  if (typeof value !== "string" || !value.startsWith("/api/artifact?")) return null;
  if (!safeArtifactPath(file.path)) return null;
  try {
    const url = new URL(value, "https://expri.invalid");
    if (
      url.origin !== "https://expri.invalid" ||
      url.pathname !== "/api/artifact" ||
      url.hash ||
      url.username ||
      url.password
    )
      return null;
    const expected = { source: source_id, run_id, path: file.path };
    const entries = [...url.searchParams];
    if (
      entries.length !== 3 ||
      Object.entries(expected).some(
        ([name, expected_value]) =>
          url.searchParams.getAll(name).length !== 1 ||
          url.searchParams.get(name) !== expected_value,
      )
    )
      return null;
    return value;
  } catch {
    return null;
  }
}
export function formatFileSize(size: number): string {
  if (!Number.isFinite(size) || size < 0) return "Unknown";
  if (size < 1024) return `${Math.round(size)} B`;
  const units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
  let value = size / 1024,
    index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index++;
  }
  return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(value)} ${units[index]}`;
}
export function posixQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}
export function artifactFetchCommand(
  scope: ArtifactScope | null,
  files: ArtifactFile[],
  selected: string[],
  config_path: string,
): string | null {
  if (
    !scope ||
    !config_path.trim() ||
    config_path.includes("\0") ||
    !parseRunDeepLink(apiUrl("", scope))
  )
    return null;
  const paths = [...new Set(selected)].filter((path) =>
    files.some((file) => file.path === path && file.cloud === true && safeArtifactPath(file.path)),
  );
  if (!paths.length || paths.length > 64) return null;
  const args = [
    "--config",
    config_path,
    "--project-id",
    scope.project_id,
    "--origin",
    scope.origin,
    "--run-id",
    scope.run_id,
    "--repo",
    ".",
  ];
  for (const path of paths) args.push("--artifact", path);
  return `expri service fetch ${args.map((value, index) => (index % 2 === 0 ? value : posixQuote(value))).join(" ")}`;
}
function Locations({ file }: { file: ArtifactFile }) {
  const labels = [
    file.local === true && "Local",
    file.cloud === true && "Cloud",
    file.worker === true && "Worker (reported)",
  ].filter(Boolean);
  return (
    <span className="file-locations">
      {labels.length ? (
        labels.map((label) => (
          <span key={String(label)} className="file-location">
            {label}
          </span>
        ))
      ) : (
        <span className="muted">
          {[file.local, file.cloud, file.worker].some((value) => value === null)
            ? "Location unknown"
            : "Not available"}
        </span>
      )}
    </span>
  );
}
export function artifactSyncStatus(file: ArtifactFile): string {
  switch (file.sync_status) {
    case "registered": return artifactSyncError(file) ? "Retrying upload" : "Registered";
    case "uploading": return "Uploading";
    case "needs_attention": return "Needs attention";
    case "cloud": return file.downloaded === true ? "Downloaded locally" :
      file.cloud === true ? "Available in cloud" : "Awaiting cloud confirmation";
    default: return file.downloaded === true ? "Downloaded locally" :
      file.cloud === true ? "Available in cloud" : "Not reported";
  }
}
export function artifactSyncError(file: ArtifactFile): string | null {
  if (!["registered", "uploading", "needs_attention"].includes(file.sync_status ?? "")) return null;
  const error = file.sync_error;
  return typeof error === "string" && new TextEncoder().encode(error).length <= 512 &&
    !/[\u0000-\u001f\u007f-\u009f]/.test(error) ? error || null : null;
}
export function artifactLabels(file: ArtifactFile): string[] {
  return Array.isArray(file.labels) && file.labels.length <= 2
    ? [...new Set(file.labels.filter((label) => label === "best" || label === "latest"))]
    : [];
}
export function FilesPanel({
  catalog,
  source_id,
  run_id,
  selected,
  loading,
  error,
  on_select,
  on_clear,
  on_refresh,
}: {
  catalog: ArtifactCatalog | null;
  source_id: string;
  run_id: string;
  selected: string[];
  loading: boolean;
  error: string | null;
  on_select: (path: string, checked: boolean) => void;
  on_clear: () => void;
  on_refresh: () => void;
}) {
  const [search, setSearch] = useState("");
  const [downloads_open, setDownloadsOpen] = useState(false);
  const [config_path, setConfigPath] = useState("");
  const [copy_status, setCopyStatus] = useState("");
  const command_node = useRef<HTMLTextAreaElement>(null);
  const files = (catalog?.files ?? []).filter((file) => safeArtifactPath(file.path));
  const visible = files.filter((file) =>
    file.path.toLocaleLowerCase().includes(search.toLocaleLowerCase()),
  );
  const chosen = files.filter((file) => selected.includes(file.path));
  const scope = catalog?.pull_scope ?? null;
  const pull_scope = artifactScopeMatchesRun(scope, source_id, run_id) ? scope : null;
  const downloadable = chosen.filter((file) => artifactDownloadUrl(file, source_id, run_id, pull_scope));
  const cloud = chosen.filter((file) => file.cloud === true);
  const command = artifactFetchCommand(pull_scope, files, selected, config_path);
  useEffect(() => {
    setCopyStatus("");
  }, [command]);
  async function copyCommand(): Promise<void> {
    if (!command) return;
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard unavailable");
      await navigator.clipboard.writeText(command);
      setCopyStatus("Command copied.");
    } catch {
      command_node.current?.focus();
      command_node.current?.select();
      setCopyStatus("Command selected. Copy it with your keyboard or context menu.");
    }
  }
  return (
    <section className="card files-card" aria-busy={loading}>
      <div className="section-heading">
        <div>
          <h3>Files</h3>
          <p className="muted">Outputs and checkpoints recorded for this run.</p>
        </div>
        <button
          id="refresh-files"
          className="text-button"
          type="button"
          disabled={loading || !run_id}
          onClick={on_refresh}
        >
          Refresh files
        </button>
      </div>
      <p id="files-loading" className="muted" hidden={!loading}>
        Loading file inventory…
      </p>
      <div id="files-error" className="notice error" role="alert" hidden={!error}>
        {error}
      </div>
      {catalog && (
        <>
          <div id="files-warnings" hidden={!catalog.warnings.length}>
            {catalog.warnings.length > 0 && (
              <div className="notice">
                <ul>
                  {catalog.warnings.slice(0, 100).map((warning, index) => (
                    <li key={index}>{warning.message}</li>
                  ))}
                </ul>
              </div>
            )}
          </div>
          {files.length !== catalog.files.length && (
            <p className="notice">Some file paths were excluded from the output preview.</p>
          )}
          <label className="field file-search">
            Search files
            <input
              id="files-search"
              type="search"
              autoComplete="off"
              placeholder="File path"
              value={search}
              onChange={(event) => setSearch(event.currentTarget.value)}
            />
          </label>
          <div className="table-scroll files-scroll">
            <table className="files-table">
              <thead>
                <tr>
                  <th className="selection-column">
                    <span className="sr-only">Select for download</span>
                  </th>
                  <th scope="col">File</th>
                  <th scope="col">Size</th>
                  <th scope="col">Available in</th>
                  <th scope="col">Transfer status</th>
                  <th scope="col">
                    <span className="sr-only">Download</span>
                  </th>
                </tr>
              </thead>
              <tbody id="file-rows">
                {visible.map((file) => {
                  const url = artifactDownloadUrl(file, source_id, run_id, pull_scope),
                    selectable = artifactCanSelect(file, pull_scope, source_id, run_id),
                    checked = selected.includes(file.path);
                  return (
                    <tr key={file.path}>
                      <td className="selection-column">
                        <input
                          type="checkbox"
                          aria-label={`Select ${file.path} for download`}
                          checked={checked}
                          disabled={!selectable || (selected.length >= 64 && !checked)}
                          onChange={(event) => on_select(file.path, event.currentTarget.checked)}
                        />
                      </td>
                      <th scope="row">
                        <code>{file.path}</code>
                        {artifactLabels(file).length > 0 && (
                          <span className="file-labels">
                            {artifactLabels(file).map((label) => (
                              <span key={label} className="file-location">
                                {label}{file.labels_pending === true ? " (pending)" : ""}
                              </span>
                            ))}
                          </span>
                        )}
                      </th>
                      <td
                        className="number"
                        title={
                          Number.isFinite(file.size)
                            ? `${file.size.toLocaleString()} bytes`
                            : undefined
                        }
                      >
                        {formatFileSize(file.size)}
                      </td>
                      <td>
                        <Locations file={file} />
                      </td>
                      <td>
                        <span className="file-sync-status">{artifactSyncStatus(file)}</span>
                        {artifactSyncError(file) && (
                          <p className="muted file-sync-error">{artifactSyncError(file)}</p>
                        )}
                        {file.cloud === true && ["registered", "uploading", "needs_attention"].includes(file.sync_status ?? "") && (
                          <p className="muted file-sync-error">
                            Cloud copy is not yet confirmed for this registration.
                          </p>
                        )}
                      </td>
                      <td>
                        {url ? (
                          <a
                            className="text-button"
                            href={url}
                            download={file.path.split("/").at(-1)}
                            target="_blank"
                            rel="noopener noreferrer"
                            aria-label={`Download ${file.path}`}
                          >
                            Download
                          </a>
                        ) : (
                          <span className="muted">
                            {selectable && file.cloud === true
                              ? "Fetch with CLI"
                              : file.worker === true && file.cloud !== true && file.local !== true
                                ? "Upload to download"
                                : "Unavailable"}
                          </span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          <p id="files-empty" className="muted" hidden={visible.length > 0}>
            {files.length
              ? "No files match this search."
              : "No output files are available. Have the experiment save checkpoints in EXPRI_OUTPUT_DIR; upload or download them to review their availability here."}
          </p>
          {catalog.truncated && (
            <p className="notice">
              This is a limited file preview. Use the CLI to inspect or retrieve other files by
              exact path.
            </p>
          )}
          <p className="muted file-location-note">
            Register a completed checkpoint to publish it while training. Registered and uploading
            are worker reports; available in cloud means the server recorded the completed upload.
            Downloaded locally comes from a verified download receipt. Worker availability is reported,
            not a live check.
            {catalog.inventory_recorded_at
              ? ` Inventory reported ${dateText(catalog.inventory_recorded_at)}.`
              : ""}
            {files.some((file) => file.local === null)
              ? " This dashboard cannot see files downloaded to your laptop."
              : ""}
          </p>
          <div className="file-selection">
            <p id="files-selection-count" className="muted">
              {selected.length
                ? `${selected.length} of 64 files selected`
                : "Select files to download"}
            </p>
            <div className="file-selection-actions">
              <button
                type="button"
                className="text-button"
                disabled={!selected.length}
                onClick={on_clear}
              >
                Clear file selection
              </button>
              <button
                id="download-selected-files"
                className="button secondary"
                type="button"
                disabled={!downloadable.length}
                onClick={() => setDownloadsOpen(true)}
              >
                Download selected files
              </button>
            </div>
          </div>
          {chosen.length > downloadable.length && (
            <p className="muted">
              Some selected cloud files are not in this local cache. Use the CLI below to download
              them.
            </p>
          )}
          {downloads_open && downloadable.length > 0 && (
            <div id="selected-file-downloads" className="selected-file-downloads">
              <h4>Selected downloads</h4>
              <p className="muted">
                Open each link to download. Browsers may restrict multiple downloads; use the CLI
                below for large files.
              </p>
              <ul>
                {downloadable.map((file) => {
                  const url = artifactDownloadUrl(file, source_id, run_id, pull_scope);
                  return (
                    url && (
                      <li key={file.path}>
                        <a
                          href={url}
                          download={file.path.split("/").at(-1)}
                          target="_blank"
                          rel="noopener noreferrer"
                        >
                          {file.path}
                        </a>
                      </li>
                    )
                  );
                })}
              </ul>
            </div>
          )}
          {cloud.length > 0 && pull_scope && (
            <section className="file-cli">
              <h4>Resumable CLI download</h4>
              <p className="muted">
                Run from your local project directory. Metadata, parameters, metrics, and logs are
                included automatically. Repeating the same command resumes selected downloads.
                {chosen.length !== cloud.length
                  ? " This command includes only the selected cloud files."
                  : ""}
              </p>
              <label className="field">
                Service client config path
                <input
                  id="artifact-config-path"
                  type="text"
                  autoComplete="off"
                  placeholder="/path/to/owner.toml"
                  maxLength={4096}
                  value={config_path}
                  onChange={(event) => {
                    setConfigPath(event.currentTarget.value);
                    setCopyStatus("");
                  }}
                />
              </label>
              <p className="muted">
                Use an existing client config; keep credentials in its named environment variable.
                Do not enter tokens here.
              </p>
              {command ? (
                <>
                  <textarea
                    id="artifact-fetch-command"
                    ref={command_node}
                    className="file-command"
                    readOnly
                    aria-label="Resumable service fetch command"
                    value={command}
                    rows={5}
                    spellCheck={false}
                  />
                  <button
                    id="copy-artifact-command"
                    className="text-button"
                    type="button"
                    onClick={() => {
                      void copyCommand();
                    }}
                  >
                    Copy command
                  </button>
                </>
              ) : (
                <p className="muted">Enter the config file path to generate a runnable command.</p>
              )}
              <p className="muted" role="status">
                {copy_status}
              </p>
            </section>
          )}
        </>
      )}
    </section>
  );
}
