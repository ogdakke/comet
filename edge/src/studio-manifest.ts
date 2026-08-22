/**
 * Wire validation for the personal Studio snapshot manifest. Keep this
 * intentionally strict: a manifest is the authority for installing local
 * files, so it must never be a way to smuggle paths, credentials, or an
 * unbounded payload onto a device.
 */
export const STUDIO_MANIFEST_VERSION = 1;
export const MAX_STUDIO_MANIFEST_BYTES = 2 * 1024 * 1024;
export const SHA256_RE = /^[a-f0-9]{64}$/;

export interface StudioObjectRef {
  sha256: string;
  sizeBytes: number;
}

export interface StudioFileRef extends StudioObjectRef {
  path: string;
  mimeType?: string;
}

export interface StudioManifest {
  version: number;
  database: StudioObjectRef;
  files: StudioFileRef[];
  publishedAt: string;
  publisherDeviceId: string;
}

const FILE_PATH_RE = /^(?:artifacts|previews|inputs)\/[A-Za-z0-9][A-Za-z0-9._-]{0,254}$/;
const DEVICE_ID_RE = /^[A-Za-z0-9._-]{1,128}$/;

const validObjectRef = (value: unknown): value is StudioObjectRef => {
  if (!isRecord(value)) return false;
  return (
    typeof value.sha256 === "string" &&
    SHA256_RE.test(value.sha256) &&
    typeof value.sizeBytes === "number" &&
    Number.isSafeInteger(value.sizeBytes) &&
    value.sizeBytes >= 0
  );
};

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/** Parse an untrusted JSON request body into the only manifest shape we use. */
export const parseStudioManifest = (body: string): { manifest: StudioManifest } | { error: string } => {
  if (new TextEncoder().encode(body).byteLength > MAX_STUDIO_MANIFEST_BYTES) {
    return { error: "manifest_too_large" };
  }

  let value: unknown;
  try {
    value = JSON.parse(body);
  } catch {
    return { error: "invalid_manifest_json" };
  }
  if (!isRecord(value)) return { error: "invalid_manifest" };
  if (value.version !== STUDIO_MANIFEST_VERSION || !validObjectRef(value.database)) {
    return { error: "invalid_manifest" };
  }
  if (
    !Array.isArray(value.files) ||
    typeof value.publishedAt !== "string" ||
    Number.isNaN(Date.parse(value.publishedAt)) ||
    typeof value.publisherDeviceId !== "string" ||
    !DEVICE_ID_RE.test(value.publisherDeviceId)
  ) {
    return { error: "invalid_manifest" };
  }

  const paths = new Set<string>();
  const files: StudioFileRef[] = [];
  for (const entry of value.files) {
    if (!isRecord(entry) || !validObjectRef(entry) || typeof entry.path !== "string") {
      return { error: "invalid_manifest" };
    }
    if (!FILE_PATH_RE.test(entry.path) || paths.has(entry.path)) {
      return { error: "invalid_manifest_path" };
    }
    if (entry.mimeType !== undefined && (typeof entry.mimeType !== "string" || entry.mimeType.length > 255)) {
      return { error: "invalid_manifest" };
    }
    paths.add(entry.path);
    files.push({
      path: entry.path,
      sha256: entry.sha256,
      sizeBytes: entry.sizeBytes,
      ...(typeof entry.mimeType === "string" ? { mimeType: entry.mimeType } : {})
    });
  }

  return {
    manifest: {
      version: STUDIO_MANIFEST_VERSION,
      database: { sha256: value.database.sha256, sizeBytes: value.database.sizeBytes },
      files,
      publishedAt: value.publishedAt,
      publisherDeviceId: value.publisherDeviceId
    }
  };
};
