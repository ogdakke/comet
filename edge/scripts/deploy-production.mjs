#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { chmodSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const encoded = process.env.ZERON_WRANGLER_PRODUCTION_CONFIG_B64;
if (!encoded) {
  console.error(
    "ZERON_WRANGLER_PRODUCTION_CONFIG_B64 is required. Add the base64-encoded production Wrangler config as a secret Cloudflare Builds variable."
  );
  process.exit(2);
}

const compact = encoded.replaceAll(/\s/g, "");
if (!/^[A-Za-z0-9+/]+={0,2}$/.test(compact) || compact.length % 4 !== 0) {
  console.error("ZERON_WRANGLER_PRODUCTION_CONFIG_B64 is not valid base64.");
  process.exit(2);
}

const config = Buffer.from(compact, "base64").toString("utf8");
if (!config.includes('"main"') || !config.includes('"name"')) {
  console.error("The decoded production Wrangler config is missing required fields.");
  process.exit(2);
}

const edgeRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const configPath = join(edgeRoot, ".wrangler.production.generated.jsonc");
const wranglerPath = join(edgeRoot, "node_modules", ".bin", "wrangler");

try {
  writeFileSync(configPath, config, { encoding: "utf8", mode: 0o600 });
  chmodSync(configPath, 0o600);
  const result = spawnSync(
    wranglerPath,
    ["deploy", "--config", configPath, ...process.argv.slice(2)],
    { cwd: edgeRoot, stdio: "inherit" }
  );
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  rmSync(configPath, { force: true });
}
