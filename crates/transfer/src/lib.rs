//! File transfers through the edge's R2 store (docs/design/file-transfer.md).
//!
//! A transfer is one file stored as immutable, checksum-verified parts under
//! `/xfer/{chatId}/{transferId}` (edge/src/transfers.ts). This crate is both
//! ends of it, shared by the engine (a host fetching a person's attachment,
//! publishing an agent's file) and the client core (a phone sending one):
//!
//! - [`plan_file`] hashes a file into parts in place; [`split_into_parts`]
//!   moves it into one file per part, shrinking the source as it goes, for
//!   uploaders that need a file per request (iOS background sessions).
//! - [`upload`] creates the transfer, sends only the parts the edge lacks, and
//!   seals it; run it again after any failure and it resumes.
//! - [`download`] writes parts straight into place in a work file, verifying
//!   each, records progress so a restart resumes, and renames the finished
//!   file into place atomically.
//!
//! Neither direction holds more than a network buffer of the file in memory.

mod download;
mod http;
mod plan;
mod upload;

use serde::{Deserialize, Serialize};

pub use download::{download, work_dir};
pub use http::{Bearer, Created, EdgeClient, Status};
pub use plan::{PART_FILE_EXT, plan_file, split_into_parts};
pub use upload::{PartSource, Upload, upload};

/// Every part but the last is this long (the edge accepts 1–64 MiB).
pub const PART_SIZE: u64 = 8 * 1024 * 1024;
/// The edge's cap on one transfer.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Parts in flight at once, each way.
pub const PARALLEL_PARTS: usize = 4;

/// One part of a file: its SHA-256 (lowercase hex) and length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// A person's attachment on its way to the chat's host.
    Up,
    /// A host's file on its way to the people viewing the chat.
    Down,
}

/// What the edge stores as a transfer's `meta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub part_size: u64,
    pub parts: Vec<Part>,
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_device: Option<String>,
}

impl Manifest {
    /// Byte offset of part `n`.
    pub fn offset(&self, n: usize) -> u64 {
        n as u64 * self.part_size
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("the sign-in expired or was refused")]
    Unauthorized,
    #[error("there is no such transfer")]
    NotFound,
    #[error("the sender cancelled this file")]
    Cancelled,
    #[error("the file hasn't finished uploading ({present} of {total} parts)")]
    NotReady {
        present: usize,
        total: usize,
        /// When the newest part landed (ms since epoch): a stalled upload
        /// stops advancing this.
        last_part_at: Option<i64>,
    },
    #[error("part {0} doesn't match its checksum")]
    Integrity(usize),
    #[error("{0} is not a valid transfer id")]
    BadId(String),
    #[error("the file is over the {} GiB limit", MAX_BYTES / (1024 * 1024 * 1024))]
    TooLarge,
    /// The edge refused the request for good (a 4xx other than the above).
    #[error("{0}")]
    Refused(String),
    /// A 5xx or 429: worth retrying.
    #[error("the edge answered {status}: {message}")]
    Unavailable { status: u16, message: String },
    #[error(transparent)]
    Network(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl TransferError {
    /// Whether trying the same request again later might succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Unavailable { .. } | Self::Network(_) => true,
            // A part that arrived corrupted in flight is fetched again.
            Self::Integrity(_) => true,
            _ => false,
        }
    }
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Whether `id` can name a chat or transfer on the edge (its `ID_RE`).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
