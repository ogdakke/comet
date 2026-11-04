import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import type { Verified } from "../../src/auth";
import type { Env } from "../../src/env";
import {
  PART_MIN_BYTES,
  TOKEN_HEADER,
  handleTransferRoute,
  mintTransferToken,
  parseManifest,
  parseRange
} from "../../src/transfers";

// Transfers against Miniflare's real R2: parts are checksum-verified as they
// stream in, so a wrong byte is the runtime's refusal, not a test double's.

const SECRET = "test-transfer-secret";
const edgeEnv = { ...(env as unknown as Env), TRANSFER_TOKEN_SECRET: SECRET };
const alice: Verified = { userId: "alice", orgId: "org", kind: "user" };
const bob: Verified = { userId: "bob", orgId: "org", kind: "user" };
const PART = PART_MIN_BYTES;

const hex = async (bytes: Uint8Array): Promise<string> =>
  [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))].map((b) => b.toString(16).padStart(2, "0")).join("");

/** Deterministic, non-repeating bytes so a misplaced range shows. */
const fileOf = (size: number): Uint8Array => {
  const bytes = new Uint8Array(size);
  let x = 0x9e3779b9;
  for (let i = 0; i < size; i++) {
    x ^= x << 13;
    x ^= x >>> 17;
    x ^= x << 5;
    bytes[i] = x & 0xff;
  }
  return bytes;
};

const manifestFor = async (bytes: Uint8Array, extra: Record<string, unknown> = {}) => {
  const parts = [];
  for (let at = 0; at < bytes.length; at += PART) {
    const slice = bytes.subarray(at, Math.min(at + PART, bytes.length));
    parts.push({ sha256: await hex(slice), size: slice.length });
  }
  return { name: "clip.mp4", mime: "video/mp4", size: bytes.length, partSize: PART, parts, direction: "up", ...extra };
};

const newId = () => crypto.randomUUID().replaceAll("-", "");

type Call = { auth?: Verified; token?: string; query?: string; body?: BodyInit; json?: unknown; headers?: Record<string, string> };

const nudges: { userId: string; deviceId: string; chatId: string }[] = [];

const call = async (method: string, path: string, init: Call = {}) => {
  const url = new URL(`https://edge.test${path}${init.query ?? ""}`);
  const headers = new Headers(init.headers);
  let body = init.body;
  if (init.json !== undefined) {
    body = JSON.stringify(init.json);
    headers.set("content-type", "application/json");
  }
  if (body instanceof Uint8Array) headers.set("content-length", String(body.byteLength));
  if (init.token) headers.set(TOKEN_HEADER, init.token);
  const res = await handleTransferRoute(new Request(url, { method, headers, body }), edgeEnv, url, init.auth, {
    nudge: async (caller, deviceId, chatId) => {
      nudges.push({ userId: caller.userId, deviceId, chatId });
    }
  });
  if (!res) throw new Error(`${path} was not a transfer route`);
  return res;
};

const create = async (chat: string, id: string, bytes: Uint8Array, extra: Record<string, unknown> = {}) => {
  const res = await call("POST", `/xfer/${chat}/${id}`, { auth: alice, json: await manifestFor(bytes, extra) });
  expect(res.status).toBe(201);
  return (await res.json()) as { token: string; readToken: string; present: number[]; done: boolean };
};

const putParts = async (chat: string, id: string, bytes: Uint8Array, token: string) => {
  for (let n = 0, at = 0; at < bytes.length; n++, at += PART) {
    const res = await call("PUT", `/xfer/${chat}/${id}/p/${n}`, {
      token,
      body: bytes.slice(at, Math.min(at + PART, bytes.length))
    });
    expect(res.status).toBe(201);
  }
};

describe("transfers through R2", () => {
  it("uploads parts, seals, nudges the host, and serves the file whole and by range", async () => {
    const chat = newId();
    const id = newId();
    const bytes = fileOf(PART * 2 + 12_345);
    const created = await create(chat, id, bytes, { notifyDevice: "host-1" });
    expect(created.present).toEqual([]);

    await putParts(chat, id, bytes.subarray(0, PART * 2), created.token);
    const midway = (await (await call("GET", `/xfer/${chat}/${id}`, { token: created.token })).json()) as {
      present: number[];
      done: boolean;
      lastPartAt: number;
    };
    expect(midway.present).toEqual([0, 1]);
    expect(midway.done).toBe(false);
    expect(midway.lastPartAt).toBeGreaterThan(0);

    const early = await call("POST", `/xfer/${chat}/${id}/done`, { token: created.token });
    expect(early.status).toBe(409);
    expect(await early.json()).toMatchObject({ error: "incomplete", missing: [2], missingCount: 1 });

    const last = await call("PUT", `/xfer/${chat}/${id}/p/2`, { token: created.token, body: bytes.slice(PART * 2) });
    expect(last.status).toBe(201);
    const done = await call("POST", `/xfer/${chat}/${id}/done`, { token: created.token });
    expect(await done.json()).toEqual({ ok: true, alreadyDone: false });
    expect(nudges).toContainEqual({ userId: "alice", deviceId: "host-1", chatId: chat });

    const whole = await call("GET", `/xfer/${chat}/${id}/content`, { query: `?t=${created.readToken}` });
    expect(whole.status).toBe(200);
    expect(whole.headers.get("content-type")).toBe("video/mp4");
    expect(whole.headers.get("content-length")).toBe(String(bytes.length));
    expect(new Uint8Array(await whole.arrayBuffer())).toEqual(bytes);

    // A range straddling the first part boundary.
    const from = PART - 100;
    const ranged = await call("GET", `/xfer/${chat}/${id}/content`, {
      token: created.readToken,
      headers: { range: `bytes=${from}-${PART + 99}` }
    });
    expect(ranged.status).toBe(206);
    expect(ranged.headers.get("content-range")).toBe(`bytes ${from}-${PART + 99}/${bytes.length}`);
    expect(new Uint8Array(await ranged.arrayBuffer())).toEqual(bytes.slice(from, PART + 100));

    const tail = await call("GET", `/xfer/${chat}/${id}/content`, { token: created.readToken, headers: { range: "bytes=-10" } });
    expect(new Uint8Array(await tail.arrayBuffer())).toEqual(bytes.slice(-10));

    const beyond = await call("GET", `/xfer/${chat}/${id}/content`, {
      token: created.readToken,
      headers: { range: `bytes=${bytes.length}-` }
    });
    expect(beyond.status).toBe(416);

    const part = await call("GET", `/xfer/${chat}/${id}/p/1`, { auth: alice });
    expect(new Uint8Array(await part.arrayBuffer())).toEqual(bytes.slice(PART, PART * 2));
  });

  it("refuses a part whose bytes or length don't match the manifest", async () => {
    const chat = newId();
    const id = newId();
    const bytes = fileOf(PART + 10);
    const { token } = await create(chat, id, bytes);

    const corrupt = bytes.slice(0, PART);
    corrupt[7] ^= 0xff;
    const wrongBytes = await call("PUT", `/xfer/${chat}/${id}/p/0`, { token, body: corrupt });
    expect(wrongBytes.status).toBe(400);

    const short = await call("PUT", `/xfer/${chat}/${id}/p/0`, { token, body: bytes.slice(0, PART - 1) });
    expect(short.status).toBe(400);

    const status = (await (await call("GET", `/xfer/${chat}/${id}`, { auth: alice })).json()) as { present: number[] };
    expect(status.present).toEqual([]);

    // Retrying the right bytes is an idempotent overwrite.
    await putParts(chat, id, bytes, token);
    const again = await call("PUT", `/xfer/${chat}/${id}/p/0`, { token, body: bytes.slice(0, PART) });
    expect(await again.json()).toEqual({ ok: true, existed: true });
  });

  it("answers a repeated create with the same transfer and refuses a different file under its id", async () => {
    const chat = newId();
    const id = newId();
    const bytes = fileOf(PART + 1);
    const first = await create(chat, id, bytes);
    await putParts(chat, id, bytes.subarray(0, PART), first.token);
    const second = await create(chat, id, bytes);
    expect(second.present).toEqual([0]);

    const other = await call("POST", `/xfer/${chat}/${id}`, { auth: alice, json: await manifestFor(fileOf(PART + 2)) });
    expect(other.status).toBe(409);
  });

  it("scopes tokens to one transfer and to reading or writing", async () => {
    const chat = newId();
    const id = newId();
    const bytes = fileOf(PART);
    const { token, readToken } = await create(chat, id, bytes);

    const readPut = await call("PUT", `/xfer/${chat}/${id}/p/0`, { token: readToken, body: bytes });
    expect(readPut.status).toBe(403);
    const readDone = await call("POST", `/xfer/${chat}/${id}/done`, { token: readToken });
    expect(readDone.status).toBe(403);

    const elsewhere = await call("GET", `/xfer/${chat}/${newId()}`, { token });
    expect(elsewhere.status).toBe(401);

    const expired = await mintTransferToken(SECRET, { u: "alice", c: chat, t: id, s: "w", e: Date.now() - 1 });
    expect((await call("GET", `/xfer/${chat}/${id}`, { token: expired })).status).toBe(401);

    const [payload, sig] = token.split(".");
    const forged = `${payload}.${sig.slice(0, -2)}${sig.endsWith("AA") ? "BB" : "AA"}`;
    expect((await call("GET", `/xfer/${chat}/${id}`, { token: forged })).status).toBe(401);

    // A token can't create or delete; only the account's bearer can.
    expect((await call("DELETE", `/xfer/${chat}/${id}`, { token })).status).toBe(403);
    expect((await call("POST", `/xfer/${chat}/${id}/token`, { token })).status).toBe(403);
    expect((await call("GET", `/xfer/${chat}/${id}`)).status).toBe(401);
  });

  it("keeps each user's transfers apart", async () => {
    const chat = newId();
    const id = newId();
    await create(chat, id, fileOf(10));
    expect((await call("GET", `/xfer/${chat}/${id}`, { auth: bob })).status).toBe(404);
  });

  it("leaves a tombstone when the sender cancels", async () => {
    const chat = newId();
    const id = newId();
    const bytes = fileOf(PART + 5);
    const { token } = await create(chat, id, bytes);
    await putParts(chat, id, bytes.subarray(0, PART), token);

    const cancelled = await call("DELETE", `/xfer/${chat}/${id}`, { auth: alice, query: "?cancel=1" });
    expect(await cancelled.json()).toEqual({ ok: true, deleted: 2 });
    expect(await (await call("GET", `/xfer/${chat}/${id}`, { auth: alice })).json()).toEqual({ cancelled: true });
    expect((await call("POST", `/xfer/${chat}/${id}`, { auth: alice, json: await manifestFor(bytes) })).status).toBe(410);
  });

  it("never serves anything but media with its own content type", async () => {
    const chat = newId();
    const id = newId();
    const bytes = new TextEncoder().encode("<script>alert(1)</script>");
    const { token, readToken } = await create(chat, id, bytes, { name: "page.html", mime: "text/html" });
    await putParts(chat, id, bytes, token);
    await call("POST", `/xfer/${chat}/${id}/done`, { token });

    const page = await call("GET", `/xfer/${chat}/${id}/content`, { token: readToken });
    expect(page.headers.get("content-type")).toBe("application/octet-stream");
    expect(page.headers.get("content-disposition")).toMatch(/^attachment; filename="page.html"/);
    expect(page.headers.get("x-content-type-options")).toBe("nosniff");
    expect(page.headers.get("content-security-policy")).toContain("sandbox");
    expect(await page.text()).toBe("<script>alert(1)</script>");
  });

  it("seals an empty file without parts", async () => {
    const chat = newId();
    const id = newId();
    const { token, readToken } = await create(chat, id, new Uint8Array(0), { name: "empty.txt", mime: "text/plain" });
    expect((await call("POST", `/xfer/${chat}/${id}/done`, { token })).status).toBe(200);
    const content = await call("GET", `/xfer/${chat}/${id}/content`, { token: readToken });
    expect(content.status).toBe(200);
    expect(content.headers.get("content-length")).toBe("0");
  });
});

describe("transfer manifests", () => {
  const base = { name: "a.bin", mime: "application/octet-stream", partSize: PART, direction: "up" };
  const sha = "0".repeat(64);

  it("accepts parts that tile the file exactly", () => {
    const parsed = parseManifest({ ...base, size: PART + 1, parts: [{ sha256: sha, size: PART }, { sha256: sha, size: 1 }] });
    expect(typeof parsed).toBe("object");
  });

  it("refuses parts that don't tile, names that aren't plain, and oversized files", () => {
    expect(parseManifest({ ...base, size: PART + 1, parts: [{ sha256: sha, size: PART + 1 }] })).toMatch(/2 parts/);
    expect(parseManifest({ ...base, size: 2, parts: [{ sha256: sha, size: 1 }] })).toMatch(/size must be 2/);
    expect(parseManifest({ ...base, name: "../x", size: 0, parts: [] })).toMatch(/plain file name/);
    expect(parseManifest({ ...base, size: 5 * 1024 ** 3, parts: [] })).toMatch(/size must be/);
    expect(parseManifest({ ...base, partSize: 1024, size: 0, parts: [] })).toMatch(/partSize/);
  });

  it("reads single byte ranges", () => {
    expect(parseRange("bytes=0-9", 100)).toEqual({ start: 0, end: 9 });
    expect(parseRange("bytes=90-", 100)).toEqual({ start: 90, end: 99 });
    expect(parseRange("bytes=-5", 100)).toEqual({ start: 95, end: 99 });
    expect(parseRange("bytes=50-500", 100)).toEqual({ start: 50, end: 99 });
    expect(parseRange("bytes=100-", 100)).toBeNull();
    expect(parseRange("bytes=0-1,5-6", 100)).toBeUndefined();
    expect(parseRange(null, 100)).toBeUndefined();
  });
});
