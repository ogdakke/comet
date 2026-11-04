//! This host's end of file transfers (docs/design/file-transfer.md).
//!
//! Outbound (`send_file`): the engine snapshots the file into uploads (a
//! folder is zipped), names it a transfer, and an outbox job uploads it to
//! the edge in the background — retrying for days, resumed after restarts —
//! so viewers can open it from R2 while this device sleeps.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeron_transfer::{
    Direction, EdgeClient, Manifest, PART_SIZE, PartSource, TransferError, Upload, plan_file,
};

use crate::doc_host::EdgeConfig;
use crate::{EngineError, new_id, now_ms};

/// Snapshots of sent files: `{uploads}/shared/{id}/{name}`.
const SHARED_DIR: &str = "shared";
/// Pending uploads, one job file per transfer.
const OUTBOX_DIR: &str = "outbox";
/// Per-chat artifact versions.
const ARTIFACTS_FILE: &str = "artifacts.json";
/// An upload that hasn't landed in a week is given up.
const OUTBOX_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_CAP: Duration = Duration::from_secs(10 * 60);

/// A file `send_file` stored, ready to fold into the reply.
#[derive(Debug, Clone)]
pub struct SharedFile {
    pub id: String,
    pub path: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub transfer_id: Option<String>,
    pub artifact: String,
    pub version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutboxJob {
    chat_id: String,
    transfer_id: String,
    path: PathBuf,
    manifest: Manifest,
    created_at: i64,
}

#[derive(Clone)]
pub struct Transfers {
    inner: Arc<Inner>,
}

struct Inner {
    uploads: PathBuf,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
    /// Transfers with an upload task alive in this process.
    running: Mutex<HashSet<String>>,
    /// Serializes `artifacts.json` read-modify-writes.
    artifacts: Mutex<()>,
}

struct EdgeBearer(EdgeConfig);

#[async_trait::async_trait]
impl zeron_transfer::Bearer for EdgeBearer {
    async fn bearer(&self) -> Result<String, TransferError> {
        self.0
            .bearer()
            .await
            .map_err(|_| TransferError::Unauthorized)
    }
}

impl Transfers {
    pub fn new(uploads: &Path, edge: Option<EdgeConfig>) -> Self {
        Self {
            inner: Arc::new(Inner {
                uploads: uploads.to_path_buf(),
                edge,
                http: reqwest::Client::new(),
                running: Mutex::new(HashSet::new()),
                artifacts: Mutex::new(()),
            }),
        }
    }

    /// The edge client, when this engine has an edge.
    pub fn client(&self) -> Option<EdgeClient> {
        let edge = self.inner.edge.clone()?;
        Some(EdgeClient::new(
            self.inner.http.clone(),
            edge.url.clone(),
            Arc::new(EdgeBearer(edge)),
        ))
    }

    fn outbox(&self) -> PathBuf {
        self.inner.uploads.join(OUTBOX_DIR)
    }

    /// Snapshot `source` (a file, or a folder zipped) for `chat_id` and queue
    /// its upload (blocking: copies and hashes; run off the reactor). The
    /// upload starts with [`Self::start`].
    pub fn share(&self, chat_id: &str, source: &Path) -> Result<SharedFile, EngineError> {
        let meta = std::fs::metadata(source)?;
        let id = format!("file:{}", new_id());
        let dir = self
            .inner
            .uploads
            .join(SHARED_DIR)
            .join(id.trim_start_matches("file:"));
        std::fs::create_dir_all(&dir)?;
        let base = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "file".into());
        let stored = if meta.is_dir() {
            let stored = dir.join(format!("{base}.zip"));
            zip_dir(source, &stored).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&dir);
            })?;
            stored
        } else {
            if meta.len() > zeron_transfer::MAX_BYTES {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(too_large());
            }
            let stored = dir.join(&base);
            std::fs::copy(source, &stored).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&dir);
            })?;
            stored
        };
        let (size, parts) = plan_file(&stored, PART_SIZE)?;
        if size > zeron_transfer::MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(too_large());
        }
        let name = stored
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(base);
        let mime_type = mime_for(&stored);
        let artifact = source.to_string_lossy().into_owned();
        let version = self.next_version(chat_id, &artifact)?;
        let transfer_id = self
            .inner
            .edge
            .as_ref()
            .map(|_| transfer_id(&name, &mime_type, &parts));
        if let Some(transfer_id) = &transfer_id {
            let job = OutboxJob {
                chat_id: chat_id.to_owned(),
                transfer_id: transfer_id.clone(),
                path: stored.clone(),
                manifest: Manifest {
                    name: name.clone(),
                    mime: mime_type.clone(),
                    size,
                    part_size: PART_SIZE,
                    parts,
                    direction: Direction::Down,
                    notify_device: None,
                },
                created_at: now_ms(),
            };
            std::fs::create_dir_all(self.outbox())?;
            write_atomic(
                &self.outbox().join(format!("{transfer_id}.json")),
                &serde_json::to_vec(&job).map_err(json_error)?,
            )?;
        }
        Ok(SharedFile {
            id,
            path: stored.to_string_lossy().into_owned(),
            name,
            mime_type,
            size,
            transfer_id,
            artifact,
            version,
        })
    }

    /// Start uploading `transfer_id`'s outbox job (no-op when already running).
    pub fn start(&self, transfer_id: &str) {
        let job = self.outbox().join(format!("{transfer_id}.json"));
        let Some(client) = self.client() else { return };
        if !self
            .inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(transfer_id.to_owned())
        {
            return;
        }
        let this = self.clone();
        let transfer_id = transfer_id.to_owned();
        tokio::spawn(async move {
            this.run_job(&client, &job).await;
            this.inner
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&transfer_id);
        });
    }

    /// Resume every queued upload (engine start).
    pub fn resume_outbox(&self) {
        let Ok(entries) = std::fs::read_dir(self.outbox()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                self.start(stem);
            }
        }
    }

    async fn run_job(&self, client: &EdgeClient, job_path: &Path) {
        let job: OutboxJob = match std::fs::read(job_path)
            .map_err(EngineError::from)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(json_error))
        {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(job = %job_path.display(), %error, "unreadable transfer job; dropping it");
                let _ = std::fs::remove_file(job_path);
                return;
            }
        };
        let source = PartSource::File(job.path.clone());
        let mut delay = RETRY_FIRST;
        loop {
            if now_ms() - job.created_at > OUTBOX_MAX_AGE_MS {
                tracing::warn!(transfer = %job.transfer_id, "file upload never landed; giving up");
                let _ = std::fs::remove_file(job_path);
                return;
            }
            let attempt = zeron_transfer::upload(
                client,
                Upload {
                    chat_id: &job.chat_id,
                    transfer_id: &job.transfer_id,
                    manifest: &job.manifest,
                    source: &source,
                    progress: None,
                },
            )
            .await;
            match attempt {
                Ok(()) => {
                    tracing::info!(transfer = %job.transfer_id, bytes = job.manifest.size, "sent file uploaded");
                    let _ = std::fs::remove_file(job_path);
                    return;
                }
                // The edge will never take this one; viewers read it from here.
                Err(
                    error @ (TransferError::Refused(_)
                    | TransferError::BadId(_)
                    | TransferError::TooLarge
                    | TransferError::Cancelled),
                ) => {
                    tracing::warn!(transfer = %job.transfer_id, %error, "sent file refused by the edge");
                    let _ = std::fs::remove_file(job_path);
                    return;
                }
                Err(TransferError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(transfer = %job.transfer_id, "sent file's snapshot is gone; dropping its upload");
                    let _ = std::fs::remove_file(job_path);
                    return;
                }
                // An edge without `/xfer` yet answers 404: keep trying, slowly.
                Err(error) => {
                    tracing::debug!(transfer = %job.transfer_id, %error, "sent file upload failed; retrying");
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RETRY_CAP);
        }
    }

    /// The next version number of `artifact` in `chat_id` (1 for its first).
    fn next_version(&self, chat_id: &str, artifact: &str) -> Result<u32, EngineError> {
        let _guard = self
            .inner
            .artifacts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = self.inner.uploads.join(SHARED_DIR).join(ARTIFACTS_FILE);
        let mut all: HashMap<String, HashMap<String, u32>> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let slot = all
            .entry(chat_id.to_owned())
            .or_default()
            .entry(artifact.to_owned())
            .or_insert(0);
        *slot += 1;
        let version = *slot;
        write_atomic(&path, &serde_json::to_vec(&all).map_err(json_error)?)?;
        Ok(version)
    }
}

fn json_error(error: serde_json::Error) -> EngineError {
    EngineError::Other(error.to_string())
}

fn too_large() -> EngineError {
    EngineError::Other(format!(
        "files are at most {} GiB",
        zeron_transfer::MAX_BYTES / (1024 * 1024 * 1024)
    ))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", new_id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Deterministic per content and name, so sending identical bytes again
/// finds the transfer already sealed and uploads nothing.
fn transfer_id(name: &str, mime: &str, parts: &[zeron_transfer::Part]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(mime.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.sha256.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Zip `dir` into `out`, skipping `.git` and links (blocking).
fn zip_dir(dir: &Path, out: &Path) -> Result<(), EngineError> {
    let file = std::fs::File::create(out)?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&current)?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink() || entry.file_name() == ".git" {
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .map_err(|e| EngineError::Other(e.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            if kind.is_dir() {
                zip.add_directory(format!("{rel}/"), options)
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                stack.push(path);
            } else if kind.is_file() {
                total += entry.metadata()?.len();
                if total > zeron_transfer::MAX_BYTES {
                    return Err(too_large());
                }
                zip.start_file(rel, options)
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                std::io::copy(&mut std::fs::File::open(&path)?, &mut zip)?;
            }
        }
    }
    zip.finish()
        .map_err(|e| EngineError::Other(e.to_string()))?
        .flush()?;
    Ok(())
}

/// The file's media type: its content where that's unambiguous, else its
/// extension, else text or bytes by whether its head is UTF-8.
pub fn mime_for(path: &Path) -> String {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(4096);
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(4096).read_to_end(&mut head);
    }
    if let Some(sniffed) = sniff(&head) {
        return sniffed.to_owned();
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if let Some(by_ext) = by_extension(&ext) {
        return by_ext.to_owned();
    }
    // Text if the head is UTF-8 (allowing a code point cut at the boundary).
    let text = match std::str::from_utf8(&head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && e.valid_up_to() + 4 >= head.len(),
    };
    if text && !head.contains(&0) {
        "text/plain".into()
    } else {
        "application/octet-stream".into()
    }
}

fn sniff(head: &[u8]) -> Option<&'static str> {
    Some(match head {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'A',
            b'V',
            b'E',
            ..,
        ] => "audio/wav",
        [b'%', b'P', b'D', b'F', b'-', ..] => "application/pdf",
        [_, _, _, _, b'f', b't', b'y', b'p', b'q', b't', ..] => "video/quicktime",
        [
            _,
            _,
            _,
            _,
            b'f',
            b't',
            b'y',
            b'p',
            b'h',
            b'e',
            b'i',
            b'c',
            ..,
        ] => "image/heic",
        [_, _, _, _, b'f', b't', b'y', b'p', b'M', b'4', b'A', ..] => "audio/mp4",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "video/mp4",
        [0x1A, 0x45, 0xDF, 0xA3, ..] => "video/webm",
        [b'I', b'D', b'3', ..] => "audio/mpeg",
        [b'O', b'g', b'g', b'S', ..] => "audio/ogg",
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "md" | "markdown" | "mdx" => "text/markdown",
        "html" | "htm" => "text/html",
        "svg" => "image/svg+xml",
        "css" => "text/css",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "js" | "mjs" | "cjs" => "text/javascript",
        "txt" | "log" | "rs" | "ts" | "tsx" | "jsx" | "py" | "rb" | "go" | "swift" | "kt"
        | "java" | "c" | "h" | "cpp" | "hpp" | "cs" | "sh" | "toml" | "ini" | "sql" | "luau"
        | "lua" => "text/plain",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => return None,
    })
}

/// Whether the agent may send `path` without asking: inside the chat's
/// folder or the system temp directory (both resolved, so `..` and links
/// can't step outside).
pub fn sendable_without_asking(path: &Path, chat_cwd: Option<&Path>) -> bool {
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    let mut roots: Vec<PathBuf> = vec![std::env::temp_dir()];
    #[cfg(unix)]
    roots.push(PathBuf::from("/tmp"));
    roots.extend(chat_cwd.map(Path::to_path_buf));
    roots
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        // Never the filesystem root or home itself, whatever the chat's cwd.
        .filter(|root| root.parent().is_some() && Some(root.as_path()) != dirs_home().as_deref())
        .any(|root| path.starts_with(&root))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .and_then(|home| std::fs::canonicalize(home).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_beats_extension_and_text_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        assert_eq!(
            mime_for(&write("shot.txt", b"\x89PNG\r\n\x1a\nrest")),
            "image/png"
        );
        assert_eq!(mime_for(&write("notes.md", b"# Title")), "text/markdown");
        assert_eq!(mime_for(&write("page.html", b"<html>")), "text/html");
        assert_eq!(
            mime_for(&write("Makefile", b"all:\n\techo hi")),
            "text/plain"
        );
        assert_eq!(
            mime_for(&write("blob", &[0, 159, 146, 150])),
            "application/octet-stream"
        );
        assert_eq!(
            mime_for(&write("clip", b"\0\0\0\x18ftypqt  ")),
            "video/quicktime"
        );
    }

    #[test]
    fn sends_versions_and_zips_folders() {
        let uploads = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(uploads.path(), None);
        let report = work.path().join("report.md");
        std::fs::write(&report, "# v1").unwrap();
        let first = transfers.share("chat", &report).unwrap();
        assert_eq!(
            (first.version, first.mime_type.as_str(), first.size),
            (1, "text/markdown", 4)
        );
        assert!(first.transfer_id.is_none(), "no edge, no transfer");
        std::fs::write(&report, "# v2!").unwrap();
        let second = transfers.share("chat", &report).unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            "# v1",
            "a snapshot, not a link"
        );
        assert_eq!(transfers.share("other", &report).unwrap().version, 1);

        let site = work.path().join("site");
        std::fs::create_dir_all(site.join("assets")).unwrap();
        std::fs::create_dir_all(site.join(".git")).unwrap();
        std::fs::write(site.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(site.join("assets/app.css"), "h1{}").unwrap();
        std::fs::write(site.join(".git/HEAD"), "ref").unwrap();
        let zipped = transfers.share("chat", &site).unwrap();
        assert_eq!(
            (zipped.name.as_str(), zipped.mime_type.as_str()),
            ("site.zip", "application/zip")
        );
        let archive = zip::ZipArchive::new(std::fs::File::open(&zipped.path).unwrap()).unwrap();
        let mut names: Vec<_> = archive.file_names().map(str::to_owned).collect();
        names.sort();
        assert_eq!(names, ["assets/", "assets/app.css", "index.html"]);
    }

    #[test]
    fn identical_files_name_the_same_transfer() {
        let parts = vec![zeron_transfer::Part {
            sha256: "ab".repeat(32),
            size: 3,
        }];
        assert_eq!(
            transfer_id("a.md", "text/markdown", &parts),
            transfer_id("a.md", "text/markdown", &parts)
        );
        assert_ne!(
            transfer_id("a.md", "text/markdown", &parts),
            transfer_id("b.md", "text/markdown", &parts)
        );
        assert!(zeron_transfer::valid_id(&transfer_id(
            "a.md",
            "text/markdown",
            &parts
        )));
    }

    #[test]
    fn only_the_chat_folder_and_temp_send_freely() {
        let chat = tempfile::tempdir().unwrap();
        let inside = chat.path().join("out.txt");
        std::fs::write(&inside, "x").unwrap();
        assert!(sendable_without_asking(&inside, Some(chat.path())));
        let temp = tempfile::NamedTempFile::new().unwrap();
        assert!(sendable_without_asking(temp.path(), None));
        let elsewhere = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let outside = elsewhere.path().join("secret");
        std::fs::write(&outside, "x").unwrap();
        assert!(!sendable_without_asking(&outside, Some(chat.path())));
    }
}
