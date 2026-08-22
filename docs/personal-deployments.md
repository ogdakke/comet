# Separate personal dev and production deployments

Keep development and production as two independent personal deployments. They
must not share a Worker, R2 bucket, Durable Object namespace, WorkOS client,
or local Zeron data directory.

## Cloudflare

The repository tracks `edge/wrangler.jsonc` only as a template. Keep two
ignored files beside it:

```text
edge/wrangler.dev.personal.jsonc
edge/wrangler.prod.personal.jsonc
```

Use a different Worker name and different `BLOBS` and `RELEASES` R2 buckets in
each. A new Worker provisions independent Durable Object namespaces when its
migrations run.

Cloudflare permits one account Secrets Store. Bind the same Worker-facing names
to different stored secrets in the two ignored configs:

```jsonc
"secrets_store_secrets": [
  {
    "binding": "WORKOS_CLIENT_ID",
    "store_id": "<private-store-id>",
    "secret_name": "WORKOS_CLIENT_ID" // dev
  },
  {
    "binding": "WORKOS_API_KEY",
    "store_id": "<private-store-id>",
    "secret_name": "WORKOS_API_KEY" // dev
  }
]
```

Production instead uses `PROD_WORKOS_CLIENT_ID` and `PROD_WORKOS_API_KEY`.
The source never contains a Worker URL, Cloudflare account or resource ID, or
WorkOS credential.

Deploy explicitly:

```sh
cd edge
npm exec wrangler -- deploy --config wrangler.dev.personal.jsonc
npm exec wrangler -- deploy --config wrangler.prod.personal.jsonc
```

## Devices

Zeron reads the private deployment file beside the active runtime data:

```text
~/.zeron-dev/env  # cargo / target/debug/zeron: dev Worker and dev WorkOS client
~/.zeron/env      # installed Zeron: prod Worker and prod WorkOS client
```

Each file contains only:

```sh
ZERON_EDGE_URL=https://<private-worker>.workers.dev
ZERON_WORKOS_CLIENT_ID=client_<private-client-id>
```

Use the dev file on every machine that should share the dev workspace. Use the
production file on devices that should share production. Sign out and log in
again after moving a machine to the other deployment because WorkOS refresh
tokens and Studio snapshots are intentionally separate.

For Linux systemd, `zeron daemon install` now points the unit at the active
data directory's `env` file. Re-run it after changing that file.
