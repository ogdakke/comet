import { describe, expect, it } from "vitest";
import { parseStudioManifest } from "./studio-manifest";

const hash = "a".repeat(64);

const manifest = {
  version: 1,
  database: { sha256: hash, sizeBytes: 120 },
  files: [
    { path: "artifacts/abc.png", sha256: hash, sizeBytes: 10, mimeType: "image/png" },
    { path: "previews/abc.webp", sha256: hash, sizeBytes: 2, mimeType: "image/webp" }
  ],
  publishedAt: "2026-08-22T12:00:00Z",
  publisherDeviceId: "macbook"
};

describe("Studio manifest validation", () => {
  it("keeps only the manifest fields the sync protocol accepts", () => {
    expect(parseStudioManifest(JSON.stringify({ ...manifest, accessToken: "never" }))).toEqual({
      manifest
    });
  });

  it("rejects traversal, duplicate target paths, and bad object hashes", () => {
    expect(
      parseStudioManifest(JSON.stringify({ ...manifest, files: [{ ...manifest.files[0], path: "inputs/../db" }] }))
    ).toEqual({ error: "invalid_manifest_path" });
    expect(
      parseStudioManifest(JSON.stringify({ ...manifest, files: [manifest.files[0], manifest.files[0]] }))
    ).toEqual({ error: "invalid_manifest_path" });
    expect(
      parseStudioManifest(JSON.stringify({ ...manifest, database: { sha256: "bad", sizeBytes: 1 } }))
    ).toEqual({ error: "invalid_manifest" });
  });
});
