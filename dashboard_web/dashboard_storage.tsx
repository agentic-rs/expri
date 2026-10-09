import { apiUrl } from "./dashboard_model";
import { artifactDownloadUrl, formatFileSize, safeArtifactPath } from "./dashboard_files";

export type StorageKind = "input" | "output";
export type StorageInput = {
  input_id: string;
  size: number;
  download_url: string | null;
};
export type StorageOutput = {
  origin: string;
  run_id: string;
  path: string;
  size: number;
  download_url: string | null;
};
export type StorageCatalog = {
  project_id: string;
  kind: StorageKind;
  items: StorageInput[] | StorageOutput[];
  total_count: number;
  next_offset: number | null;
};

const COMPONENT = /^[A-Za-z0-9._-]{1,96}$/;
function validComponent(value: unknown): value is string {
  return typeof value === "string" && COMPONENT.test(value) && value !== "." && value !== "..";
}

export function boundedStorageSearch(value: string): string {
  const encoder = new TextEncoder();
  let result = "";
  let bytes = 0;
  for (const character of value) {
    const length = encoder.encode(character).length;
    if (bytes + length > 256) break;
    result += character;
    bytes += length;
  }
  return result;
}

export function storageCatalogValid(
  value: StorageCatalog,
  project_id: string,
  kind: StorageKind,
  offset: number,
  limit: number,
): boolean {
  if (
    value?.project_id !== project_id ||
    value.kind !== kind ||
    !Array.isArray(value.items) ||
    value.items.length > limit ||
    !Number.isSafeInteger(value.total_count) ||
    value.total_count < 0 ||
    (value.next_offset !== null &&
      (!Number.isSafeInteger(value.next_offset) ||
        value.next_offset <= offset ||
        value.next_offset > value.total_count))
  ) return false;
  return value.items.every((item) =>
    item !== null && typeof item === "object" &&
    Number.isSafeInteger(item.size) && item.size >= 0 &&
    (kind === "input"
      ? validComponent((item as StorageInput).input_id)
      : validComponent((item as StorageOutput).origin) &&
        validComponent((item as StorageOutput).run_id) &&
        typeof (item as StorageOutput).path === "string" &&
        safeArtifactPath((item as StorageOutput).path)),
  );
}

export function storageInputDownloadUrl(item: StorageInput, project_id: string): string | null {
  if (!validComponent(project_id) || !validComponent(item.input_id)) return null;
  const value = item.download_url;
  if (typeof value !== "string" || !value.startsWith("/api/input?")) return null;
  try {
    const url = new URL(value, "https://expri.invalid");
    if (url.origin !== "https://expri.invalid" || url.pathname !== "/api/input" || url.hash)
      return null;
    const expected = { project_id, input_id: item.input_id };
    if (
      [...url.searchParams].length !== 2 ||
      Object.entries(expected).some(([name, expected_value]) =>
        url.searchParams.getAll(name).length !== 1 ||
        url.searchParams.get(name) !== expected_value)
    ) return null;
    return value;
  } catch {
    return null;
  }
}

export function storageOutputDownloadUrl(item: StorageOutput, project_id: string): string | null {
  if (!validComponent(project_id) || !validComponent(item.origin) || !validComponent(item.run_id) ||
    !safeArtifactPath(item.path)) return null;
  return artifactDownloadUrl(
    { path: item.path, size: item.size, local: null, cloud: true, worker: null, download_url: item.download_url },
    `hosted-project:${project_id}`,
    `${item.origin}:${item.run_id}`,
    { project_id, origin: item.origin, run_id: item.run_id },
  );
}

export function storageUrl(project_id: string, kind: StorageKind, offset: number, search: string): string {
  return apiUrl("/api/storage", { project_id, kind, limit: 100, offset, search: search.trim() });
}

export function StoragePanel({
  project_id,
  kind,
  search,
  offset,
  catalog,
  loading,
  error,
  on_kind,
  on_search,
  on_previous,
  on_next,
  on_refresh,
}: {
  project_id: string;
  kind: StorageKind;
  search: string;
  offset: number;
  catalog: StorageCatalog | null;
  loading: boolean;
  error: string | null;
  on_kind: (kind: StorageKind) => void;
  on_search: (search: string) => void;
  on_previous: () => void;
  on_next: () => void;
  on_refresh: () => void;
}) {
  const items = catalog?.items ?? [];
  const total_count = catalog?.total_count ?? 0;
  const next_offset = catalog?.next_offset ?? null;
  return (
    <section id="project-storage" className="storage-page" aria-labelledby="storage-heading">
      <p className="muted storage-intro">
        Browse private inputs and uploaded run outputs, including checkpoints, across this project.
      </p>
      <fieldset id="storage-kind-options" className="choice-group storage-kinds">
        <legend>Show</legend>
        <div className="choice-tags">
          {([
            ["input", "Private inputs"],
            ["output", "Run outputs"],
          ] as const).map(([value, label]) => (
            <label className="choice-tag" key={value}>
              <input
                id={`storage-kind-${value}`}
                className="sr-only"
                type="radio"
                name="storage_kind"
                value={value}
                checked={kind === value}
                onChange={() => on_kind(value)}
              />
              <span>{label}</span>
            </label>
          ))}
        </div>
      </fieldset>
      <section className="card storage-card" aria-busy={loading}>
        <div className="section-heading">
          <div>
            <h2>{kind === "input" ? "Private inputs" : "Run outputs"}</h2>
            <p>
              {kind === "input"
                ? "Inputs are identified by their project-wide input ID."
                : "Finalized files from every run and machine in this project."}
            </p>
          </div>
          <button id="refresh-storage" className="text-button" type="button" disabled={loading} onClick={on_refresh}>
            Refresh storage
          </button>
        </div>
        <label className="field storage-search">
          Search {kind === "input" ? "input IDs" : "run outputs"}
          <input
            id="storage-search"
            type="search"
            autoComplete="off"
            placeholder={kind === "input" ? "Input ID" : "Machine, run, or file path"}
            maxLength={256}
            value={search}
            onChange={(event) => on_search(event.currentTarget.value)}
          />
        </label>
        <p id="storage-loading" className="muted" role="status" hidden={!loading}>Loading storage…</p>
        <div id="storage-error" className="notice error" role="alert" hidden={!error}>{error}</div>
        {catalog && (
          <>
            <div className="table-scroll storage-scroll">
              <table className={`storage-table storage-table-${kind}`}>
                <thead>
                  {kind === "input" ? (
                    <tr><th scope="col">Input ID</th><th scope="col">Size</th><th scope="col">Download</th></tr>
                  ) : (
                    <tr><th scope="col">Machine</th><th scope="col">Run</th><th scope="col">File</th><th scope="col">Size</th><th scope="col">Download</th></tr>
                  )}
                </thead>
                <tbody id="storage-rows">
                  {kind === "input"
                    ? (items as StorageInput[]).map((item) => {
                        const url = storageInputDownloadUrl(item, project_id);
                        return (
                          <tr key={item.input_id}>
                            <th scope="row"><code>{item.input_id}</code></th>
                            <td className="number">{formatFileSize(item.size)}</td>
                            <td>{url ? <a className="text-button" href={url} download={item.input_id} target="_blank" rel="noopener noreferrer">Download</a> : <span className="muted">Unavailable</span>}</td>
                          </tr>
                        );
                      })
                    : (items as StorageOutput[]).map((item) => {
                        const url = storageOutputDownloadUrl(item, project_id);
                        return (
                          <tr key={`${item.origin}:${item.run_id}:${item.path}`}>
                            <td><code>{item.origin}</code></td>
                            <td><code>{item.run_id}</code></td>
                            <th scope="row"><code>{item.path}</code></th>
                            <td className="number">{formatFileSize(item.size)}</td>
                            <td>{url ? <a className="text-button" href={url} download={item.path.split("/").at(-1)} target="_blank" rel="noopener noreferrer">Download</a> : <span className="muted">Unavailable</span>}</td>
                          </tr>
                        );
                      })}
                </tbody>
              </table>
            </div>
            <p id="storage-empty" className="muted" hidden={items.length > 0}>
              {search.trim()
                ? "No files match this search."
                : kind === "input"
                  ? "No private inputs have been published in this project."
                  : "No run outputs have been uploaded in this project."}
            </p>
            <div className="storage-pagination">
              <p id="storage-page-label" className="muted">
                {total_count ? `${offset + 1}–${offset + items.length} of ${total_count}` : "0 files"}
              </p>
              <div>
                <button id="storage-previous-page" className="text-button" type="button" disabled={loading || offset === 0} onClick={on_previous}>Previous</button>
                <button id="storage-next-page" className="text-button" type="button" disabled={loading || next_offset === null} onClick={on_next}>Next</button>
              </div>
            </div>
          </>
        )}
      </section>
    </section>
  );
}
