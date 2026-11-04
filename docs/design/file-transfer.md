# File transfer and artifacts

Status: design, phase 1 in progress (2026-11-03).

## Product shape

**Any file, either direction, any size up to 4 GiB.** A person attaches videos, PDFs, zips
or anything else to a message from any device; an agent sends files back with `send_file`, and
they render inline in the transcript like `show_image` images do today. A file the agent sends
again from the same path is a new **version** of the same artifact, not a new attachment.

- Uploads survive the app leaving the foreground (iOS background `URLSession`) and resume
  after a crash, relaunch or network drop, at 8 MiB granularity.
- Neither end ever holds a whole file in memory.
- Files the agent sends open while the host is asleep: they are served from R2, not the host.
- Bytes rest in R2 only as long as they are useful: an upload to a host is deleted the moment
  it lands; a file sent to the person lives 30 days (bucket lifecycle on `xfer/`), and the host
  keeps the original and can publish it again.

What it replaces: attachments are base64 slices over the device relay (≤ 24 MB, phone
foreground only, no resume, whole file in RAM on both ends), and agents can show only raster
images ≤ 10 MB, read back over the relay in 45 KB chunks with the host online.

## Transfers

A **transfer** is one file in R2 under `xfer/{userId}/{chatId}/{transferId}/`:

| Key | Contents |
|---|---|
| `meta` | The manifest: `{v:1, name, mime, size, partSize, parts:[{sha256,size}], direction, notifyDevice?, createdAt}` |
| `p/{n}` | Part `n`: every part is exactly `partSize` bytes except the last. |
| `done` | Written once every part is present. |
| `cancelled` | Tombstone: the sender gave up; the receiver rejects instead of waiting. |

Parts are separate immutable objects rather than an R2 multipart upload: each PUT is
checksum-verified by R2 against the manifest's SHA-256 while it streams, retrying a part is an
idempotent overwrite, resuming is a listing, and nothing needs part ETags (which an iOS
background session can't reliably hand back). The manifest's per-part `(sha256, size)` list
leaves room for content-defined chunking and cross-version dedup later without a format change.

Chats belong to one user (registry rooms are `reg1/{org}/{user}`, chat rooms claim on first
join), so the user id scopes every key; Cloud runners act as their user and are additionally
gated to the chats they host (`runnerChatGate`).

### Edge routes (`edge/src/transfers.ts`)

All under `/xfer/:chatId/:transferId`. **Bearer** is the normal user/runner JWT; **token** is
a transfer token (below).

| Route | Auth | |
|---|---|---|
| `POST` | bearer | Create or refresh. Body is the manifest. Idempotent: an identical manifest answers the existing one (409 if it differs). Returns `{token, readToken, expiresAt, present}`. |
| `GET` | bearer, token | Status: manifest, present parts, last part time, `done`, `cancelled`. |
| `PUT /p/:n` | bearer, write token | Streams part `n` into R2 with its SHA-256 enforced; `Content-Length` must match the manifest. |
| `POST /done` | bearer, write token | Verifies every part is present, writes `done`, nudges `notifyDevice` (the DeviceRoom `nudge`, so a cold host wakes). |
| `GET /p/:n` | bearer, token | One part (the receiving engine downloads parts in parallel). |
| `GET /content` | bearer, token (`?t=`) | The whole file, `Range`-aware across parts, so `AVPlayer` streams a video before it has downloaded. Only raster images, audio and video get their real `Content-Type`; everything else is `application/octet-stream` + `attachment`. Always `nosniff` and `CSP: sandbox`. |
| `POST /token` | bearer | Mint a fresh read or write token. |
| `DELETE` | bearer | Remove the transfer (`?cancel=1` leaves the `cancelled` tombstone). |

**Transfer tokens** exist because an iOS background task's headers are fixed when it is
created and it may run hours later, after the WorkOS bearer expired; and because `AVPlayer`,
QuickLook and the share sheet open URLs, not authenticated requests. A token is
`base64url(payload).base64url(HMAC-SHA256)` with `TRANSFER_TOKEN_SECRET`; the payload binds the
user, chat, transfer, scope (`r` or `w`) and expiry (48 h write, 24 h read). It authorizes that
one transfer and nothing else.

## You → agent (attachments)

1. The client moves the picked file into its stash (`uploads/{id}/`), splits it into part
   files **from the end, truncating the source as it goes** (peak extra disk: one part) and
   hashes each part. iOS background upload tasks need a file per part; every platform shares
   the layout.
2. The message is written at once with a `pending://{id}/{name}` ref, as today.
3. `POST` creates the transfer (`notifyDevice` = host), every missing part is PUT, then `done`.
   The stash's `meta.json` records progress; on relaunch the client reconciles against `GET`
   and re-sends only missing parts. 401 refreshes the token (`POST` again); a rejected size or
   checksum fails the attachment visibly instead of retrying forever.
4. The host, holding a command on a missing ref, asks `GET`; once every part is there it
   downloads them four at a time into `tmp/{id}/data.partial` at their offsets (verifying each
   part's hash, resumable across restarts), renames to the ref's target, `kick_drains()`, and
   `DELETE`s the transfer.
5. The host waits on an R2-backed ref while parts keep arriving (any part in the last 30 min),
   up to 48 h, instead of a flat 15 min; a `cancelled` tombstone rejects the command at once.

Hosts advertise `attachments-r2-v1`; clients fall back to the relay path (24 MB) for hosts
without it.

## Agent → you (`send_file`, artifacts)

`send_file {path, caption?}` sends any file (a folder is zipped) and renders it inline by type:

| Type | Inline | Opened |
|---|---|---|
| Markdown | Rendered with the transcript's renderer, first screen with a fade | Full document |
| HTML | Live, scaled preview | Full screen, interactive |
| SVG, images | Inline | Zoomable |
| PDF | First page | PDFKit |
| Video, audio | Player streaming `/content` | Full-screen player, save to Photos |
| Code, text | Highlighted preview | Full file |
| Anything else | Card: icon, name, size | QuickLook, share sheet |

- The engine snapshots the file into uploads (so later edits don't change what was sent),
  hashes it, puts a `File` part in the reply immediately, and uploads in the background; the
  card shows progress until it is available. Re-sending identical bytes reuses the transfer.
- **Versions:** a later `send_file` of the same path in the same chat is the next version of
  that artifact. The newest renders in full; older ones collapse to "v1 · updated below" and
  stay openable; text artifacts can show what changed.
- **Paths:** the chat's working folder and the system temp directory send freely. Anything else
  asks the person (the same input flow as other approvals); the tool description says so, and a
  denial comes back as a plain refusal the agent can act on.
- **Rendering safety:** HTML runs in its own `WKWebView` with a non-persistent data store, no
  script bridge and a unique per-artifact origin served by a custom scheme handler; scripts
  and CDN loads work, and the viewer chrome marks it as agent content. SVG renders as an image
  (no script). What renders inline is decided by sniffed content, not extension.
- `show_image` stays and keeps working.

**Byte-level deltas between versions are deliberately not built.** Text artifacts are small
(compression beats deltas), and re-encoded video or regenerated PDFs share almost no bytes with
the previous version. The manifest format leaves room for chunk dedup if usage ever shows it
matters; doing it right needs manifest-walking garbage collection under the 30-day lifecycle.

## Phases

1. **Foundation:** edge transfer routes, tokens, `Range` reads, tests; the shared Rust transfer
   crate (manifest, splitting, hashing, resumable upload and download).
2. **Agent → you:** `send_file`, the `File` part, engine publishing, iOS cards, viewers,
   player, QuickLook, share, background download.
3. **You → agent:** file-backed `OutgoingAttachment`, stash v2, the client upload state
   machine with an iOS background transport, host fetch and wait window, any-file pickers.
4. Later: Markdown links to local files, desktop and Android transports.

## Status (first cut)

Built: the edge routes and the transfer crate; `send_file` with versions and the path approval;
the mobile `File` card, read-URL streaming and resumable fetch with the host as fallback; the
client sending a picked file by path through R2 and the host fetching it while parts arrive.

Not yet as designed:

- The phone uploads in-process (the Rust escort, resumed on every launch from the stash) rather
  than through an iOS background `URLSession`, so an upload pauses while the app is suspended.
  The stash is one file read by offset (`PartSource::File`); the split-from-the-end layout
  (`split_into_parts`) is ready for when the background transport lands.
- The host's wait is "while its fetcher still expects the transfer" (it appeared within 15 min,
  and parts arrived within the last 2 h, up to 48 h) rather than a fixed 30-minute progress window.
- Older versions aren't collapsed in the transcript yet; every version renders as its own card.

## Deployment

- `wrangler secret put TRANSFER_TOKEN_SECRET` (32+ random bytes).
- R2 lifecycle on the blobs bucket: delete objects under `xfer/` after 30 days
  (`wrangler r2 bucket lifecycle add <bucket> xfer-30d xfer/ --expire-days 30`).
