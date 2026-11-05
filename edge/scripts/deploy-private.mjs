#!/usr/bin/env node

// Deploys a private personal Worker (docs/personal-deployments.md) whose
// Wrangler config lives only in a secret Cloudflare Builds variable:
//
//   node scripts/deploy-private.mjs production [wrangler deploy args…]
//     reads ZERON_WRANGLER_PRODUCTION_CONFIG_B64
//   node scripts/deploy-private.mjs staging [wrangler deploy args…]
//     reads ZERON_WRANGLER_STAGING_CONFIG_B64

import { spawnSync } from "node:child_process";
import { chmodSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [target, ...wranglerArgs] = process.argv.slice(2);
if (target !== "production" && target !== "staging") {
  console.error("usage: deploy-private.mjs <production|staging> [wrangler deploy args…]");
  process.exit(2);
}

const variable = `ZERON_WRANGLER_${target.toUpperCase()}_CONFIG_B64`;
const encoded = process.env[variable];
if (!encoded) {
  console.error(
    `${variable} is required. Add the base64-encoded ${target} Wrangler config as a secret Cloudflare Builds variable.`
  );
  process.exit(2);
}

const compact = encoded.replaceAll(/\s/g, "");
if (!/^[A-Za-z0-9+/]+={0,2}$/.test(compact) || compact.length % 4 !== 0) {
  console.error(`${variable} is not valid base64.`);
  process.exit(2);
}

const config = Buffer.from(compact, "base64").toString("utf8");
if (!config.includes('"main"') || !config.includes('"name"')) {
  console.error(`The decoded ${target} Wrangler config is missing required fields.`);
  process.exit(2);
}

// The Worker imports node:buffer (@cloudflare/puppeteer, src/card-image.ts),
// which only loads with nodejs_compat. A private config that predates it
// gets the flag here; one that sets its own flags must include it.
const compatArgs = [];
if (!config.includes('"compatibility_flags"')) {
  compatArgs.push("--compatibility-flags", "nodejs_compat");
} else if (!config.includes('"nodejs_compat"')) {
  console.error(`The ${target} Wrangler config sets compatibility_flags without "nodejs_compat".`);
  process.exit(2);
}

// The deployed Worker already ran the template's Durable Object migrations
// through v5 (PreviewRoom). A config missing one of them makes Wrangler
// replay every migration from v1, which Cloudflare refuses (10074: "Cannot
// apply new-sqlite-class migration … already depended on"). Say so plainly.
for (const tag of ["v1", "v2", "v3", "v4", "v5"]) {
  if (!new RegExp(`"tag"\\s*:\\s*"${tag}"`).test(config)) {
    console.error(
      `The ${target} Wrangler config has no "${tag}" migration. Copy the migrations (and the PREVIEW_ROOMS binding) from wrangler.jsonc, then update ${variable}.`
    );
    process.exit(2);
  }
}

const edgeRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const configPath = join(edgeRoot, `.wrangler.${target}.generated.jsonc`);
const wranglerPath = join(edgeRoot, "node_modules", ".bin", "wrangler");

try {
  writeFileSync(configPath, config, { encoding: "utf8", mode: 0o600 });
  chmodSync(configPath, 0o600);
  const result = spawnSync(wranglerPath, ["deploy", "--config", configPath, ...compatArgs, ...wranglerArgs], {
    cwd: edgeRoot,
    stdio: "inherit"
  });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  rmSync(configPath, { force: true });
}
