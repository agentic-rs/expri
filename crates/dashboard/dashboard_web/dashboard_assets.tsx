import { useEffect, useRef, useState } from "react";
import { apiUrl, jsonObject, parseRunDeepLink, type Detail, type Json } from "./dashboard_model";
import { safeArtifactPath } from "./artifact_paths";

export type AssetProducer = { origin: string; run_id: string; path: string; run_available?: boolean };
export type AssetConsumer = AssetProducer;

export function assetConsumerValid(value: unknown): value is AssetConsumer {
  if (!value || typeof value !== "object") return false;
  const consumer = value as AssetConsumer;
  return typeof consumer.origin === "string" && typeof consumer.run_id === "string" && assetPathValid(consumer.path) &&
    parseRunDeepLink(apiUrl("", { project_id: "project", origin: consumer.origin, run_id: consumer.run_id })) !== null;
}

export function assetProducerValid(value: unknown): value is AssetProducer {
  if (!value || typeof value !== "object") return false;
  const producer = value as AssetProducer;
  return typeof producer.origin === "string" && typeof producer.run_id === "string" &&
    typeof producer.path === "string" && safeArtifactPath(producer.path) &&
    parseRunDeepLink(apiUrl("", { project_id: "project", origin: producer.origin, run_id: producer.run_id })) !== null;
}

export function assetRunUrl(project_id: string, run: Pick<AssetProducer, "origin" | "run_id">): string | null {
  const url = apiUrl("/", { project_id, origin: run.origin, run_id: run.run_id });
  return parseRunDeepLink(url.slice(1)) ? url : null;
}

export function outputAssetReference(producer: AssetProducer): string | null {
  const encode = (value: string): string => encodeURIComponent(value).replace(/[!'()*]/g,
    (character) => `%${character.charCodeAt(0).toString(16).toUpperCase()}`);
  return assetProducerValid(producer)
    ? `expri://./${[producer.origin, producer.run_id, ...producer.path.split("/")].map(encode).join("/")}`
    : null;
}

export function CopyAssetReference({ producer }: { producer: AssetProducer }) {
  const reference = outputAssetReference(producer);
  const [status, setStatus] = useState("");
  const [fallback, setFallback] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (!fallback) return;
    input.current?.focus();
    input.current?.select();
  }, [fallback, reference]);
  if (!reference) return null;
  async function copy(): Promise<void> {
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard unavailable");
      await navigator.clipboard.writeText(reference!);
      setStatus("Reference copied.");
      setFallback(false);
    } catch {
      setFallback(true);
      setStatus("Copy the reference below.");
    }
  }
  return (
    <div className="asset-reference">
      <button className="text-button" type="button" onClick={() => void copy()} aria-label={`Copy reference for ${producer.path}`}>
        Copy reference
      </button>
      {fallback && <input ref={input} aria-label={`Asset reference for ${producer.path}`} className="asset-reference-input" value={reference} readOnly onFocus={() => input.current?.select()} />}
      <span className="muted asset-copy-status" role="status">{status}</span>
    </div>
  );
}

function assetPathValid(path: unknown): path is string {
  return typeof path === "string" && path.length > 0 && new TextEncoder().encode(path).length <= 1024 &&
    !/[\\\u0000-\u001f\u007f-\u009f]/.test(path) && !path.startsWith("/") &&
    path.split("/").every((part) => part !== "" && part !== "." && part !== "..");
}

export function consumedAssets(detail: Detail): { path: string; source: string; producer: AssetProducer | null }[] {
  const assets = jsonObject(detail.state)["assets"];
  if (!Array.isArray(assets)) return [];
  return assets.slice(0, 16).flatMap((value: Json) => {
    const asset = jsonObject(value);
    if (!assetPathValid(asset["path"])) return [];
    const source = jsonObject(asset["source"] ?? null);
    const availability = jsonObject(asset["availability"] ?? null);
    const input_id = source["input_id"] ?? availability["input_id"];
    const label = typeof input_id === "string" ? input_id :
      source["kind"] === "local" ? "Local asset" :
      source["kind"] === "hugging_face" ? "Hugging Face" : "External asset";
    return [{ path: asset["path"], source: label, producer: assetProducerValid(asset["producer"]) ? asset["producer"] : null }];
  });
}

export function RunAssets({ detail }: { detail: Detail }) {
  const assets = consumedAssets(detail);
  if (!assets.length) return null;
  const project_id = detail.source.kind === "hosted_project" ? detail.source.project_id : null;
  return (
    <section className="card run-assets" aria-labelledby="run-assets-heading">
      <h3 id="run-assets-heading">Inputs</h3>
      <p className="muted">Pinned assets prepared for this experiment.</p>
      <div className="table-scroll">
        <table>
          <thead><tr><th scope="col">Path</th><th scope="col">Asset</th><th scope="col">Produced by</th></tr></thead>
          <tbody>{assets.map((asset) => {
            const url = project_id && asset.producer && asset.producer.run_available !== false ? assetRunUrl(project_id, asset.producer) : null;
            return <tr key={asset.path}>
              <th scope="row"><code>{asset.path}</code></th>
              <td><code>{asset.source}</code></td>
              <td>{asset.producer ? <>
                {url ? <a href={url}>{asset.producer.origin} / {asset.producer.run_id}</a> : <code>{asset.producer.origin} / {asset.producer.run_id}</code>}
                <small className="asset-producer-path"><code>{asset.producer.path}</code></small>
                {asset.producer.run_available === false && <small className="muted asset-producer-path">Producing run is no longer available.</small>}
              </> : <span className="muted">—</span>}</td>
            </tr>;
          })}</tbody>
        </table>
      </div>
      {Number(jsonObject(detail.state)["asset_count"]) > assets.length && <p className="muted">Showing {assets.length} input assets. Inspect the run record for the complete list.</p>}
    </section>
  );
}
