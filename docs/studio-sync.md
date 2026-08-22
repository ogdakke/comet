# Studio sync

Studio is profile-local today:

```text
{profile}/studio/
  studio.sqlite3
  artifacts/{artifact-id}.{ext}
  previews/{artifact-id}.{ext}
  inputs/{asset-id}.{ext}
```

This design mirrors that complete, durable state between a user's signed-in
devices. Provider credentials remain device-local and never enter the sync
manifest or R2.

## Scope

This is deliberately a personal, authenticated sync protocol. It optimizes for
one active editor at a time, which is the expected use for a single person's
desktop and phone. It does not pretend concurrent SQLite edits merge safely.

When two devices race, the second writer gets an explicit conflict. It must
pull the remote snapshot before it can publish. No device silently replaces a
newer remote workspace.

## Remote layout

All keys are private in the existing `BLOBS` bucket and require a valid WorkOS
bearer with an `org_id` equal to the URL's workspace.

```text
studio/{org-id}/{user-id}/objects/{sha256}
```

An object is immutable. The hash is the SHA-256 of its bytes. It may be a
SQLite snapshot, artifact, preview, or imported input. A StudioRoom Durable
Object holds the current manifest and monotonically increasing generation.

```json
{
  "version": 1,
  "database": { "sha256": "...", "sizeBytes": 245760 },
  "files": [
    {
      "path": "artifacts/…png",
      "sha256": "...",
      "sizeBytes": 123456,
      "mimeType": "image/png"
    }
  ],
  "publishedAt": "2026-08-22T12:00:00Z",
  "publisherDeviceId": "…"
}
```

The manifest never contains a local absolute path, refresh token, provider API
key, or a temporary import filename.

## HTTP surface

```text
GET  /studio/{orgId}/manifest
PUT  /studio/{orgId}/manifest     If-Match: "{generation}"
GET  /studio/{orgId}/objects/{sha256}
HEAD /studio/{orgId}/objects/{sha256}
PUT  /studio/{orgId}/objects/{sha256}
```

`GET manifest` returns an envelope with the current `generation` and either a
manifest or `null`. `PUT manifest` supplies `If-Match: "{generation}"`; it
only commits when that number equals the current one. The Room increments it
atomically. A missing precondition returns `428`; a mismatch returns
`409 conflict` with the current generation.

Object upload requires the hash in both the path and `X-Studio-SHA256` header.
The Engine verifies every downloaded object before installing it locally. An
object with unexpected bytes is rejected, the manifest remains untouched, and
the sync reports an integrity error.

## Device sequence

### Publish

1. Create a consistent SQLite snapshot with SQLite's backup API. Never copy a
   live database file directly.
2. Hash the database snapshot and all jailed Studio files.
3. Upload any absent objects.
4. Read the current manifest generation.
5. Compare-and-swap the new manifest. If it returns `409`, retain all local
   files and report the conflict.

### Pull

1. Fetch the current manifest.
2. Download missing objects to a staging directory, verifying each hash.
3. Validate the downloaded SQLite snapshot before replacing the local store.
4. Atomically install the database and files, then reopen Studio.

A pull refuses a database whose Studio schema does not match the installed
app. Update that device first. This is stricter than guessing at a migration
during a restore, which could leave a live catalog half-upgraded.

Each profile keeps `.studio-sync-state.json` beside the local Studio store. It
records the last accepted generation and manifest. A normal poll only fetches
the tiny manifest; if its generation matches, no media request is made. If a
remote generation changed, the Engine snapshots and hashes local content before
installing it. Different local content is an explicit conflict.

The WorkOS runtime checks the manifest every 30 seconds and debounces local
Studio writes for two seconds. It sends `HEAD` before every object upload, so a
re-published snapshot transfers metadata and only new bytes. Studio rendering
always reads its local SQLite and files; network transfer stays off that path.

Deletion is represented by omission from a later manifest. Physical R2 garbage
collection is intentionally deferred; an object must stay available while any
older manifest might still be installed on another device.

## Integration boundaries

- `edge/`: authenticated StudioRoom and object routes.
- `crates/engine/src/studio_sync.rs`: snapshot, manifest, R2 transfer,
  integrity verification, and conflict reporting.
- `StudioStore`: exposes a consistent snapshot and a staged atomic restore.
- Engine runtime: starts pull on a signed-in Studio boot and schedules publish
  after durable Studio mutations.
- UI/iOS: consumes the existing Studio watch data. A later status surface can
  show pending upload or conflict, but no generic deployment setup UI is
  introduced.

## One-time local import

Zeron does not automatically promote `profiles/local/studio` when a device
signs in. Import explicitly after signing in to the intended personal account:

```sh
zeron studio import-local
```

The command uses the active runtime's `profiles/local/studio` by default. Use
`--source /path/to/studio` only when intentionally moving another catalog. It
refuses to run while the engine is active, and refuses if either the signed-in
profile or remote Studio manifest has real content. A successful import copies
the source without changing it, then publishes the snapshot.
