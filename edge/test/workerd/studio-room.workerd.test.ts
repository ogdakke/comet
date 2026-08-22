import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";

const hash = "a".repeat(64);
const manifest = {
  version: 1,
  database: { sha256: hash, sizeBytes: 10 },
  files: [{ path: "artifacts/example.png", sha256: hash, sizeBytes: 1, mimeType: "image/png" }],
  publishedAt: "2026-08-22T12:00:00Z",
  publisherDeviceId: "test-device"
};

const request = (method: string, body?: unknown, generation?: number): Request => {
  const text = body === undefined ? undefined : JSON.stringify(body);
  const headers = new Headers({ "x-zeron-auth-user": "user" });
  if (text !== undefined) headers.set("content-length", String(new TextEncoder().encode(text).byteLength));
  if (generation !== undefined) headers.set("if-match", `"${generation}"`);
  return new Request("https://studio.test/manifest", { method, headers, body: text });
};

describe("Studio manifest CAS on real DO SQLite", () => {
  it("accepts the first snapshot and refuses a stale publisher", async () => {
    const stub = env.TEST_STUDIO.get(env.TEST_STUDIO.idFromName("cas"));
    expect(await (await stub.fetch(request("GET"))).json()).toEqual({ generation: 0, manifest: null });

    const first = await stub.fetch(request("PUT", manifest, 0));
    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({ generation: 1, manifest });

    const stale = await stub.fetch(request("PUT", manifest, 0));
    expect(stale.status).toBe(409);
    expect(await stale.json()).toEqual({ error: "conflict", generation: 1 });
  });
});
