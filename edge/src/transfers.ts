/**
 * File transfers through R2 (docs/design/file-transfer.md): one file, either
 * direction (a person's attachment to a host, an agent's file to the person),
 * stored as immutable checksum-verified parts under
 * `xfer/{userId}/{chatId}/{transferId}/`:
 *
 *   meta        the manifest (name, type, size, per-part sha256 + size)
 *   p/{n}       part n, exactly `partSize` bytes but the last
 *   done        written once every part is present
 *   cancelled   the sender gave up; the receiver rejects instead of waiting
 *
 * Callers authenticate with their bearer, or with a transfer token: an HMAC
 * capability for one transfer, because an iOS background upload's headers are
 * fixed when it is queued (hours before the bearer may expire) and AVPlayer,
 * QuickLook and the share sheet open URLs, not authenticated requests.
 */
import type { Verified } from "./auth";
import { b64urlDecode, b64urlEncode } from "./cloud/crypto";
import type { Env } from "./env";
import { runnerChatGate } from "./runner-access";

export const TRANSFER_MAX_BYTES = 4 * 1024 * 1024 * 1024;
export const PART_MIN_BYTES = 1024 * 1024;
export const PART_MAX_BYTES = 64 * 1024 * 1024;
export const MAX_PARTS = 1024;
export const WRITE_TOKEN_TTL_MS = 48 * 60 * 60 * 1000;
export const READ_TOKEN_TTL_MS = 24 * 60 * 60 * 1000;
/** Header a transfer token rides in; `?t=` works too, for URL-only openers. */
export const TOKEN_HEADER = "x-zeron-transfer-token";

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const SHA_RE = /^[0-9a-f]{64}$/;
const MIME_RE = /^[a-z0-9][a-z0-9!#$&^_.+-]{0,63}\/[a-z0-9][a-z0-9!#$&^_.+-]{0,63}$/;
/** Types served with their own Content-Type; everything else downloads as
 * opaque bytes, so nothing a transfer holds ever renders on the edge origin. */
const INLINE_RE = /^(image\/(png|jpeg|gif|webp|avif|heic)|video\/[a-z0-9.+-]+|audio\/[a-z0-9.+-]+)$/;
const MAX_MANIFEST_BODY = 256 * 1024;

export interface TransferPart {
  sha256: string;
  size: number;
}

export interface Manifest {
  v: 1;
  name: string;
  mime: string;
  size: number;
  partSize: number;
  parts: TransferPart[];
  /** `up`: a person's attachment to a host; `down`: a host's file to viewers. */
  direction: "up" | "down";
  /** Woken with a DeviceRoom nudge once the transfer is done (the host). */
  notifyDevice?: string;
  createdAt: number;
}

type Scope = "r" | "w";

interface TokenClaims {
  /** user */
  u: string;
  /** org, so a nudge can wake a Cloud host */
  o?: string;
  /** chat */
  c: string;
  /** transfer */
  t: string;
  s: Scope;
  /** expiry, ms since epoch */
  e: number;
}

interface Caller {
  userId: string;
  orgId?: string;
  /** A bearer may do everything; a token only its scope (`w` includes reads). */
  bearer: boolean;
  scope: Scope;
}

export interface TransferHooks {
  /** Wake `deviceId` for `chatId` (DeviceRoom `/nudge`). Best effort. */
  nudge(caller: { userId: string; orgId?: string }, deviceId: string, chatId: string): Promise<void>;
}

const json = (value: unknown, status = 200, headers: HeadersInit = {}): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", ...headers }
  });

// ── tokens ──────────────────────────────────────────────────────────────────

const encoder = new TextEncoder();
const keys = new Map<string, Promise<CryptoKey>>();

const hmacKey = (secret: string): Promise<CryptoKey> => {
  let key = keys.get(secret);
  if (!key) {
    key = crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
      "sign",
      "verify"
    ]);
    keys.set(secret, key);
  }
  return key;
};

export const mintTransferToken = async (secret: string, claims: TokenClaims): Promise<string> => {
  const payload = b64urlEncode(encoder.encode(JSON.stringify(claims)));
  const sig = await crypto.subtle.sign("HMAC", await hmacKey(secret), encoder.encode(payload));
  return `${payload}.${b64urlEncode(new Uint8Array(sig))}`;
};

/** The token's claims when its signature holds and it hasn't expired. */
export const verifyTransferToken = async (
  secret: string,
  token: string,
  now = Date.now()
): Promise<TokenClaims | undefined> => {
  const [payload, sig, extra] = token.split(".");
  if (!payload || !sig || extra !== undefined) return undefined;
  const sigBytes = b64urlDecode(sig);
  const payloadBytes = b64urlDecode(payload);
  if (!sigBytes || !payloadBytes) return undefined;
  // subtle.verify compares in constant time.
  const ok = await crypto.subtle.verify("HMAC", await hmacKey(secret), sigBytes, encoder.encode(payload));
  if (!ok) return undefined;
  let claims: TokenClaims;
  try {
    claims = JSON.parse(new TextDecoder().decode(payloadBytes)) as TokenClaims;
  } catch {
    return undefined;
  }
  if (typeof claims.e !== "number" || claims.e <= now) return undefined;
  if (claims.s !== "r" && claims.s !== "w") return undefined;
  return claims;
};

// ── manifests ───────────────────────────────────────────────────────────────

/** A validated manifest from a create body, or why it isn't one. */
export const parseManifest = (body: unknown, now = Date.now()): Manifest | string => {
  if (!body || typeof body !== "object") return "body must be an object";
  const b = body as Record<string, unknown>;
  const { name, mime, size, partSize, parts, direction, notifyDevice } = b;
  if (typeof name !== "string" || name.length === 0 || name.length > 255) return "name must be 1-255 characters";
  if (/[\u0000-\u001f\u007f/\\]/.test(name) || name === "." || name === "..") return "name must be a plain file name";
  if (typeof mime !== "string" || !MIME_RE.test(mime)) return "mime must be a media type";
  if (!Number.isSafeInteger(size) || (size as number) < 0 || (size as number) > TRANSFER_MAX_BYTES) {
    return `size must be 0-${TRANSFER_MAX_BYTES} bytes`;
  }
  if (
    !Number.isSafeInteger(partSize) ||
    (partSize as number) < PART_MIN_BYTES ||
    (partSize as number) > PART_MAX_BYTES
  ) {
    return `partSize must be ${PART_MIN_BYTES}-${PART_MAX_BYTES} bytes`;
  }
  if (direction !== "up" && direction !== "down") return "direction must be up or down";
  if (notifyDevice !== undefined && (typeof notifyDevice !== "string" || !ID_RE.test(notifyDevice))) {
    return "notifyDevice must be a device id";
  }
  if (!Array.isArray(parts)) return "parts must be an array";
  const expected = Math.ceil((size as number) / (partSize as number));
  if (parts.length !== expected) return `a ${size}-byte file in ${partSize}-byte parts has ${expected} parts`;
  if (parts.length > MAX_PARTS) return `at most ${MAX_PARTS} parts`;
  const clean: TransferPart[] = [];
  for (const [i, part] of parts.entries()) {
    const p = part as Record<string, unknown> | null;
    if (!p || typeof p.sha256 !== "string" || !SHA_RE.test(p.sha256)) return `parts[${i}].sha256 must be hex SHA-256`;
    const want = i < parts.length - 1 ? (partSize as number) : (size as number) - i * (partSize as number);
    if (p.size !== want) return `parts[${i}].size must be ${want}`;
    clean.push({ sha256: p.sha256, size: want });
  }
  return {
    v: 1,
    name,
    mime,
    size: size as number,
    partSize: partSize as number,
    parts: clean,
    direction,
    ...(notifyDevice === undefined ? {} : { notifyDevice: notifyDevice as string }),
    createdAt: now
  };
};

/** Whether two manifests describe the same transfer (creation time aside). */
const sameTransfer = (a: Manifest, b: Manifest): boolean =>
  a.name === b.name &&
  a.mime === b.mime &&
  a.size === b.size &&
  a.partSize === b.partSize &&
  a.direction === b.direction &&
  a.notifyDevice === b.notifyDevice &&
  a.parts.every((p, i) => p.sha256 === b.parts[i]?.sha256 && p.size === b.parts[i]?.size);

// ── R2 helpers ──────────────────────────────────────────────────────────────

const hexToBytes = (hex: string): Uint8Array => {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) bytes[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return bytes;
};

const readManifest = async (bucket: R2Bucket, prefix: string): Promise<Manifest | undefined> => {
  const object = await bucket.get(`${prefix}meta`);
  if (!object) return undefined;
  return (await object.json()) as Manifest;
};

/** Every object under `prefix`, across listing pages. */
const listAll = async (bucket: R2Bucket, prefix: string): Promise<R2Object[]> => {
  const out: R2Object[] = [];
  let cursor: string | undefined;
  do {
    // Listings leave customMetadata (each part's sha256) out unless asked.
    const page = await bucket.list({ prefix, cursor, limit: 1000, include: ["customMetadata"] });
    out.push(...page.objects);
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  return out;
};

/** Parts already in R2 that match the manifest, and when the last landed. */
const presentParts = async (
  bucket: R2Bucket,
  prefix: string,
  manifest: Manifest
): Promise<{ present: number[]; lastPartAt: number | null }> => {
  const present: number[] = [];
  let lastPartAt: number | null = null;
  for (const object of await listAll(bucket, `${prefix}p/`)) {
    const n = Number(object.key.slice(prefix.length + 2));
    const want = manifest.parts[n];
    if (!Number.isInteger(n) || !want || object.size !== want.size) continue;
    if (object.customMetadata?.sha256 !== want.sha256) continue;
    present.push(n);
    const at = object.uploaded.getTime();
    if (lastPartAt === null || at > lastPartAt) lastPartAt = at;
  }
  present.sort((a, b) => a - b);
  return { present, lastPartAt };
};

/** `bytes=a-b`, `bytes=a-`, `bytes=-n` (one range); `null` = unsatisfiable. */
export const parseRange = (header: string | null, size: number): { start: number; end: number } | null | undefined => {
  if (!header) return undefined;
  const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!m || (m[1] === "" && m[2] === "")) return undefined;
  if (size === 0) return null;
  if (m[1] === "") {
    const suffix = Number(m[2]);
    if (suffix === 0) return null;
    return { start: Math.max(0, size - suffix), end: size - 1 };
  }
  const start = Number(m[1]);
  const end = m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
  if (start >= size || end < start) return null;
  return { start, end };
};

/** Bytes `start..=end` of the file, read part by part. */
const streamRange = (
  bucket: R2Bucket,
  prefix: string,
  manifest: Manifest,
  start: number,
  end: number
): ReadableStream<Uint8Array> => {
  const length = end - start + 1;
  const { readable, writable } = new FixedLengthStream(length);
  (async () => {
    let offset = 0;
    for (const [n, part] of manifest.parts.entries()) {
      const partStart = offset;
      offset += part.size;
      if (offset <= start) continue;
      if (partStart > end) break;
      const from = Math.max(start, partStart) - partStart;
      const to = Math.min(end, offset - 1) - partStart;
      const object = await bucket.get(`${prefix}p/${n}`, { range: { offset: from, length: to - from + 1 } });
      if (!object) throw new Error(`part ${n} is missing`);
      await object.body.pipeTo(writable, { preventClose: true });
    }
    await writable.close();
  })().catch((error: unknown) => writable.abort(error).catch(() => undefined));
  return readable;
};

const dispositionName = (name: string): string => {
  const ascii = name.replace(/[^\x20-\x7e]|["\\]/g, "_");
  return `filename="${ascii}"; filename*=UTF-8''${encodeURIComponent(name)}`;
};

// ── the route ───────────────────────────────────────────────────────────────

/**
 * `/xfer/:chatId/:transferId[/…]`, or `undefined` for any other path.
 * `auth` is the verified bearer when the request carried one (the caller has
 * already refused a bad one); without it a transfer token must authorize.
 */
export const handleTransferRoute = async (
  request: Request,
  env: Pick<Env, "BLOBS" | "REGISTRY_ROOMS" | "TRANSFER_TOKEN_SECRET">,
  url: URL,
  auth: Verified | undefined,
  hooks: TransferHooks
): Promise<Response | undefined> => {
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts[0] !== "xfer") return undefined;
  const [, chatId, transferId, ...rest] = parts;
  if (!chatId || !transferId || !ID_RE.test(chatId) || !ID_RE.test(transferId)) {
    return json({ error: "not_found" }, 404);
  }
  const secret = env.TRANSFER_TOKEN_SECRET;

  let caller: Caller;
  if (auth) {
    const notHost = await runnerChatGate(env, auth, chatId);
    if (notHost) return notHost;
    caller = { userId: auth.userId, orgId: auth.orgId, bearer: true, scope: "w" };
  } else {
    const token = request.headers.get(TOKEN_HEADER) ?? url.searchParams.get("t");
    if (!token) return json({ error: "unauthenticated" }, 401);
    if (!secret) return json({ error: "transfers_unavailable" }, 503);
    const claims = await verifyTransferToken(secret, token);
    if (!claims || claims.c !== chatId || claims.t !== transferId) return json({ error: "unauthenticated" }, 401);
    caller = { userId: claims.u, orgId: claims.o, bearer: false, scope: claims.s };
  }

  const bucket = env.BLOBS;
  const prefix = `xfer/${caller.userId}/${chatId}/${transferId}/`;
  const method = request.method;
  const bearerOnly = () => json({ error: "forbidden", message: "This needs the account's own sign-in." }, 403);
  const writeOnly = () => json({ error: "forbidden", message: "This token can only read." }, 403);
  const tokens = async () => {
    if (!secret) return undefined;
    const now = Date.now();
    const base = { u: caller.userId, ...(caller.orgId ? { o: caller.orgId } : {}), c: chatId, t: transferId };
    return {
      token: await mintTransferToken(secret, { ...base, s: "w", e: now + WRITE_TOKEN_TTL_MS }),
      readToken: await mintTransferToken(secret, { ...base, s: "r", e: now + READ_TOKEN_TTL_MS }),
      expiresAt: now + READ_TOKEN_TTL_MS
    };
  };

  // ── create / status / delete ─────────────────────────────────────────────
  if (rest.length === 0) {
    if (method === "POST") {
      if (!caller.bearer) return bearerOnly();
      const minted = await tokens();
      if (!minted) return json({ error: "transfers_unavailable" }, 503);
      const declared = Number(request.headers.get("content-length") ?? "0");
      if (declared > MAX_MANIFEST_BODY) return json({ error: "too_large" }, 413);
      const text = await request.text();
      if (text.length > MAX_MANIFEST_BODY) return json({ error: "too_large" }, 413);
      let body: unknown;
      try {
        body = JSON.parse(text);
      } catch {
        return json({ error: "bad_manifest", message: "body is not JSON" }, 400);
      }
      const asked = parseManifest(body);
      if (typeof asked === "string") return json({ error: "bad_manifest", message: asked }, 400);
      if (await bucket.head(`${prefix}cancelled`)) return json({ error: "cancelled" }, 410);
      let manifest = await readManifest(bucket, prefix);
      if (!manifest) {
        const put = await bucket.put(`${prefix}meta`, JSON.stringify(asked), {
          onlyIf: { etagDoesNotMatch: "*" },
          httpMetadata: { contentType: "application/json" }
        });
        // Lost a create race: answer whichever manifest won.
        manifest = put ? asked : await readManifest(bucket, prefix);
      }
      if (!manifest || !sameTransfer(manifest, asked)) {
        return json({ error: "conflict", message: "this transfer id already names another file" }, 409);
      }
      const { present, lastPartAt } = await presentParts(bucket, prefix, manifest);
      const done = (await bucket.head(`${prefix}done`)) !== null;
      return json({ manifest, present, lastPartAt, done, ...minted }, 201);
    }
    if (method === "GET") {
      const cancelled = (await bucket.head(`${prefix}cancelled`)) !== null;
      const manifest = await readManifest(bucket, prefix);
      if (!manifest) return cancelled ? json({ cancelled: true }) : json({ error: "not_found" }, 404);
      const { present, lastPartAt } = await presentParts(bucket, prefix, manifest);
      const done = (await bucket.head(`${prefix}done`)) !== null;
      return json({ manifest, present, lastPartAt, done, cancelled });
    }
    if (method === "DELETE") {
      if (!caller.bearer) return bearerOnly();
      const keys = (await listAll(bucket, prefix)).map((o) => o.key);
      for (let i = 0; i < keys.length; i += 1000) await bucket.delete(keys.slice(i, i + 1000));
      if (url.searchParams.get("cancel") === "1") await bucket.put(`${prefix}cancelled`, "");
      return json({ ok: true, deleted: keys.length });
    }
    return json({ error: "method_not_allowed" }, 405);
  }

  // ── tokens ───────────────────────────────────────────────────────────────
  if (rest.length === 1 && rest[0] === "token") {
    if (method !== "POST") return json({ error: "method_not_allowed" }, 405);
    if (!caller.bearer) return bearerOnly();
    if (!(await bucket.head(`${prefix}meta`))) return json({ error: "not_found" }, 404);
    const minted = await tokens();
    if (!minted) return json({ error: "transfers_unavailable" }, 503);
    return json(minted);
  }

  // ── done ─────────────────────────────────────────────────────────────────
  if (rest.length === 1 && rest[0] === "done") {
    if (method !== "POST") return json({ error: "method_not_allowed" }, 405);
    if (caller.scope !== "w") return writeOnly();
    const manifest = await readManifest(bucket, prefix);
    if (!manifest) return json({ error: "not_found" }, 404);
    if (await bucket.head(`${prefix}cancelled`)) return json({ error: "cancelled" }, 410);
    const { present } = await presentParts(bucket, prefix, manifest);
    if (present.length !== manifest.parts.length) {
      const have = new Set(present);
      const missing = manifest.parts.map((_, n) => n).filter((n) => !have.has(n));
      return json({ error: "incomplete", missing: missing.slice(0, 64), missingCount: missing.length }, 409);
    }
    const already = (await bucket.head(`${prefix}done`)) !== null;
    if (!already) await bucket.put(`${prefix}done`, "");
    if (manifest.notifyDevice) {
      // The host also polls while it waits; the nudge only makes it prompt.
      await hooks
        .nudge({ userId: caller.userId, orgId: caller.orgId }, manifest.notifyDevice, chatId)
        .catch((error: unknown) => console.warn("transfer nudge failed", String(error)));
    }
    return json({ ok: true, alreadyDone: already });
  }

  // ── parts ────────────────────────────────────────────────────────────────
  if (rest.length === 2 && rest[0] === "p") {
    const n = /^\d{1,5}$/.test(rest[1]) ? Number(rest[1]) : -1;
    const manifest = await readManifest(bucket, prefix);
    if (!manifest) return json({ error: "not_found" }, 404);
    const want = manifest.parts[n];
    if (!want) return json({ error: "not_found", message: `no part ${rest[1]}` }, 404);
    const key = `${prefix}p/${n}`;
    if (method === "PUT") {
      if (caller.scope !== "w") return writeOnly();
      if (await bucket.head(`${prefix}cancelled`)) return json({ error: "cancelled" }, 410);
      const declared = Number(request.headers.get("content-length") ?? "");
      if (!Number.isSafeInteger(declared)) return json({ error: "length_required" }, 411);
      if (declared !== want.size) {
        return json({ error: "bad_part", message: `part ${n} is ${want.size} bytes, not ${declared}` }, 400);
      }
      const existing = await bucket.head(key);
      if (existing && existing.size === want.size && existing.customMetadata?.sha256 === want.sha256) {
        return json({ ok: true, existed: true });
      }
      if (!request.body) return json({ error: "body_required" }, 400);
      try {
        // R2 verifies the checksum as the body streams in.
        await bucket.put(key, request.body, {
          sha256: hexToBytes(want.sha256),
          customMetadata: { sha256: want.sha256 },
          httpMetadata: { contentType: "application/octet-stream" }
        });
      } catch (error) {
        return json({ error: "bad_part", message: `part ${n} was refused: ${String(error)}` }, 400);
      }
      return json({ ok: true, existed: false }, 201);
    }
    if (method === "GET") {
      const object = await bucket.get(key);
      if (!object) return json({ error: "not_found", message: `part ${n} hasn't arrived` }, 404);
      return new Response(object.body, {
        headers: {
          "content-type": "application/octet-stream",
          "content-length": String(object.size),
          "x-zeron-sha256": want.sha256,
          "cache-control": "private, max-age=86400, immutable"
        }
      });
    }
    return json({ error: "method_not_allowed" }, 405);
  }

  // ── the whole file ───────────────────────────────────────────────────────
  if (rest.length === 1 && rest[0] === "content") {
    if (method !== "GET" && method !== "HEAD") return json({ error: "method_not_allowed" }, 405);
    const manifest = await readManifest(bucket, prefix);
    if (!manifest) return json({ error: "not_found" }, 404);
    if (!(await bucket.head(`${prefix}done`))) return json({ error: "not_ready" }, 409);
    const inline = INLINE_RE.test(manifest.mime);
    const headers = new Headers({
      "content-type": inline ? manifest.mime : "application/octet-stream",
      "content-disposition": `${inline ? "inline" : "attachment"}; ${dispositionName(manifest.name)}`,
      "accept-ranges": "bytes",
      etag: `"${transferId}"`,
      "cache-control": "private, max-age=86400, immutable",
      "x-content-type-options": "nosniff",
      "content-security-policy": "sandbox; default-src 'none'"
    });
    const range = parseRange(request.headers.get("range"), manifest.size);
    if (range === null) {
      headers.set("content-range", `bytes */${manifest.size}`);
      return new Response(null, { status: 416, headers });
    }
    const { start, end } = range ?? { start: 0, end: manifest.size - 1 };
    const length = manifest.size === 0 ? 0 : end - start + 1;
    headers.set("content-length", String(length));
    if (range) headers.set("content-range", `bytes ${start}-${end}/${manifest.size}`);
    const status = range ? 206 : 200;
    if (method === "HEAD" || length === 0) return new Response(null, { status, headers });
    return new Response(streamRange(bucket, prefix, manifest, start, end), { status, headers });
  }

  return json({ error: "not_found" }, 404);
};
