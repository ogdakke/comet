/**
 * StudioRoom owns one user's current Studio manifest. It deliberately stores
 * only compact JSON and a generation counter. SQLite snapshots and media live
 * as immutable R2 objects, so a large gallery cannot make a DO slow to start.
 */
import { AUTH_USER_HEADER } from "./env";
import {
  MAX_STUDIO_MANIFEST_BYTES,
  parseStudioManifest,
  type StudioManifest
} from "./studio-manifest";

interface StoredManifest {
  generation: number;
  body: string;
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

const parseGeneration = (header: string | null): number | undefined => {
  const match = header?.match(/^"([0-9]+)"$/);
  if (!match) return undefined;
  const generation = Number(match[1]);
  return Number.isSafeInteger(generation) ? generation : undefined;
};

export class StudioRoom implements DurableObject {
  private readonly ctx: DurableObjectState;

  constructor(ctx: DurableObjectState) {
    this.ctx = ctx;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS manifest (id INTEGER PRIMARY KEY CHECK(id = 1), generation INTEGER NOT NULL, body TEXT NOT NULL, updated_at INTEGER NOT NULL)"
    );
  }

  private current(): StoredManifest | undefined {
    const rows = [
      ...this.ctx.storage.sql.exec("SELECT generation, body FROM manifest WHERE id = 1")
    ];
    const row = rows[0];
    return row
      ? { generation: row.generation as number, body: row.body as string }
      : undefined;
  }

  async fetch(request: Request): Promise<Response> {
    if (!request.headers.get(AUTH_USER_HEADER)) return json({ error: "unauthenticated" }, 401);
    const url = new URL(request.url);
    if (url.pathname !== "/manifest") return json({ error: "not_found" }, 404);

    if (request.method === "GET") {
      const current = this.current();
      if (!current) return json({ generation: 0, manifest: null });
      return json({ generation: current.generation, manifest: JSON.parse(current.body) as StudioManifest });
    }

    if (request.method !== "PUT") return json({ error: "method_not_allowed" }, 405);
    const expectedGeneration = parseGeneration(request.headers.get("if-match"));
    if (expectedGeneration === undefined) {
      return json({ error: "precondition_required", message: 'use If-Match: "<generation>"' }, 428);
    }
    const declaredLength = Number(request.headers.get("content-length") ?? "");
    if (!Number.isSafeInteger(declaredLength) || declaredLength < 0) {
      return json({ error: "length_required" }, 411);
    }
    if (declaredLength > MAX_STUDIO_MANIFEST_BYTES) return json({ error: "manifest_too_large" }, 413);

    const parsed = parseStudioManifest(await request.text());
    if ("error" in parsed) return json({ error: parsed.error }, 400);

    // A DO serializes requests for this key. Keep the read and write
    // synchronous so the compare-and-swap cannot be interleaved.
    const current = this.current();
    const generation = current?.generation ?? 0;
    if (generation !== expectedGeneration) {
      return json({ error: "conflict", generation }, 409);
    }
    const nextGeneration = generation + 1;
    this.ctx.storage.sql.exec(
      "INSERT INTO manifest (id, generation, body, updated_at) VALUES (1, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET generation = excluded.generation, body = excluded.body, updated_at = excluded.updated_at",
      nextGeneration,
      JSON.stringify(parsed.manifest),
      Date.now()
    );
    return json({ generation: nextGeneration, manifest: parsed.manifest });
  }
}
