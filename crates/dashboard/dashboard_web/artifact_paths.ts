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
