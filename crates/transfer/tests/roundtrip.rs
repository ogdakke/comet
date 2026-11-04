//! Upload and download against an in-process fake of the edge's `/xfer`
//! routes (edge/src/transfers.ts), with injected failures.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use zeron_transfer::{
    Bearer, Direction, EdgeClient, Manifest, PartSource, TransferError, Upload, download,
    plan_file, split_into_parts, upload,
};

const PART: u64 = 64 * 1024;

#[derive(Default)]
struct Stored {
    manifest: Option<Manifest>,
    parts: HashMap<usize, Vec<u8>>,
    done: bool,
    cancelled: bool,
}

#[derive(Default)]
struct Fake {
    transfers: Mutex<HashMap<String, Stored>>,
    /// `"PUT 2"` / `"GET 1"`: answer 503 this many more times.
    fail: Mutex<HashMap<String, usize>>,
    /// `"GET 1"`: serve flipped bytes once.
    corrupt: Mutex<HashSet<String>>,
    /// Every part request served, `"PUT 0"`, in order.
    log: Mutex<Vec<String>>,
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn reply(status: u16, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn json(status: u16, value: serde_json::Value) -> Vec<u8> {
    reply(status, value.to_string().as_bytes())
}

impl Fake {
    fn route(&self, method: &str, path: &str, auth: Option<&str>, body: Vec<u8>) -> Vec<u8> {
        if auth != Some("Bearer test") {
            return json(401, serde_json::json!({ "error": "unauthenticated" }));
        }
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let key = format!("{}/{}", segs[1], segs[2]);
        let mut transfers = self.transfers.lock().unwrap();
        let entry = transfers.entry(key).or_default();
        let present = |s: &Stored| {
            let mut p: Vec<usize> = s.parts.keys().copied().collect();
            p.sort();
            p
        };
        match (method, &segs[3..]) {
            ("POST", []) => {
                if entry.cancelled {
                    return json(410, serde_json::json!({ "error": "cancelled" }));
                }
                let asked: Manifest = serde_json::from_slice(&body).unwrap();
                if let Some(existing) = &entry.manifest
                    && existing != &asked
                {
                    return json(409, serde_json::json!({ "error": "conflict" }));
                }
                entry.manifest = Some(asked.clone());
                json(
                    201,
                    serde_json::json!({ "manifest": asked, "present": present(entry), "done": entry.done,
                        "token": "w", "readToken": "r", "expiresAt": 0, "lastPartAt": null }),
                )
            }
            ("GET", []) => match &entry.manifest {
                None if entry.cancelled => json(200, serde_json::json!({ "cancelled": true })),
                None => json(404, serde_json::json!({ "error": "not_found" })),
                Some(m) => json(
                    200,
                    serde_json::json!({ "manifest": m, "present": present(entry), "done": entry.done,
                        "cancelled": entry.cancelled, "lastPartAt": 1 }),
                ),
            },
            ("DELETE", []) => {
                *entry = Stored {
                    cancelled: query == "cancel=1",
                    ..Default::default()
                };
                json(200, serde_json::json!({ "ok": true }))
            }
            ("POST", ["done"]) => {
                let total = entry.manifest.as_ref().unwrap().parts.len();
                if entry.parts.len() != total {
                    return json(409, serde_json::json!({ "error": "incomplete" }));
                }
                entry.done = true;
                json(200, serde_json::json!({ "ok": true }))
            }
            (method, ["p", n]) => {
                let n: usize = n.parse().unwrap();
                let tag = format!("{method} {n}");
                self.log.lock().unwrap().push(tag.clone());
                if let Some(left) = self.fail.lock().unwrap().get_mut(&tag)
                    && *left > 0
                {
                    *left -= 1;
                    return json(503, serde_json::json!({ "error": "injected" }));
                }
                let want = entry.manifest.as_ref().unwrap().parts[n].clone();
                if method == "PUT" {
                    if body.len() as u64 != want.size || sha(&body) != want.sha256 {
                        return json(400, serde_json::json!({ "error": "bad_part" }));
                    }
                    entry.parts.insert(n, body);
                    return json(201, serde_json::json!({ "ok": true }));
                }
                let Some(bytes) = entry.parts.get(&n) else {
                    return json(404, serde_json::json!({ "error": "not_found" }));
                };
                let mut bytes = bytes.clone();
                if self.corrupt.lock().unwrap().remove(&tag) {
                    bytes[0] ^= 0xff;
                }
                reply(200, &bytes)
            }
            _ => json(404, serde_json::json!({ "error": "not_found" })),
        }
    }
}

/// Serve `fake` on a loopback port; one request per connection.
async fn serve(fake: Arc<Fake>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let fake = fake.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let mut words = line.split_whitespace();
                let (method, path) = (
                    words.next().unwrap().to_owned(),
                    words.next().unwrap().to_owned(),
                );
                let (mut length, mut auth) = (0usize, None);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).await.unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap();
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap(),
                        "authorization" => auth = Some(value.trim().to_owned()),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let out = fake.route(&method, &path, auth.as_deref(), body);
                let mut socket = reader.into_inner();
                let _ = socket.write_all(&out).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

struct Static;

#[async_trait::async_trait]
impl Bearer for Static {
    async fn bearer(&self) -> Result<String, TransferError> {
        Ok("test".into())
    }
}

async fn setup() -> (Arc<Fake>, EdgeClient) {
    let fake = Arc::new(Fake::default());
    let base = serve(fake.clone()).await;
    (
        fake,
        EdgeClient::new(reqwest::Client::new(), base, Arc::new(Static)),
    )
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 977) as u8).collect()
}

fn manifest_for(path: &Path) -> Manifest {
    let (size, parts) = plan_file(path, PART).unwrap();
    Manifest {
        name: "clip.mov".into(),
        mime: "video/quicktime".into(),
        size,
        part_size: PART,
        parts,
        direction: Direction::Up,
        notify_device: None,
    }
}

fn job<'a>(manifest: &'a Manifest, source: &'a PartSource) -> Upload<'a> {
    Upload {
        chat_id: "chat",
        transfer_id: "t1",
        manifest,
        source,
        progress: None,
    }
}

#[tokio::test]
async fn a_file_goes_up_and_comes_back_byte_for_byte() {
    let (fake, client) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("clip.mov");
    let bytes = data(PART as usize * 3 + 1234);
    std::fs::write(&src, &bytes).unwrap();
    let manifest = manifest_for(&src);

    let progress = Arc::new(Mutex::new(Vec::new()));
    let seen = progress.clone();
    let source = PartSource::File(src.clone());
    let mut up = job(&manifest, &source);
    up.progress = Some(Arc::new(move |done, total| {
        seen.lock().unwrap().push((done, total))
    }));
    upload(&client, up).await.unwrap();
    assert_eq!(
        progress.lock().unwrap().last(),
        Some(&(bytes.len() as u64, bytes.len() as u64))
    );

    let dest = dir.path().join("out/clip.mov");
    let work = dir.path().join("work");
    let got = download(&client, "chat", "t1", &dest, &work, None)
        .await
        .unwrap();
    assert_eq!(got, manifest);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert!(
        !work.exists(),
        "the work dir goes once the file is in place"
    );

    // Sending it again finds it sealed and sends nothing.
    let before = fake.log.lock().unwrap().len();
    upload(&client, job(&manifest, &source)).await.unwrap();
    assert_eq!(fake.log.lock().unwrap().len(), before);
}

#[tokio::test]
async fn a_split_file_uploads_from_its_part_files() {
    let (_fake, client) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("clip.mov");
    let bytes = data(PART as usize * 2 + 7);
    std::fs::write(&src, &bytes).unwrap();
    let manifest = manifest_for(&src);
    let parts = dir.path().join("parts");
    assert_eq!(
        split_into_parts(&src, &parts, PART).unwrap().1,
        manifest.parts
    );

    upload(&client, job(&manifest, &PartSource::Split(parts)))
        .await
        .unwrap();
    let dest = dir.path().join("back");
    download(&client, "chat", "t1", &dest, &dir.path().join("w"), None)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), bytes);
}

#[tokio::test]
async fn an_interrupted_upload_resumes_with_only_the_missing_parts() {
    let (fake, client) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("clip.mov");
    std::fs::write(&src, data(PART as usize * 4)).unwrap();
    let manifest = manifest_for(&src);
    let source = PartSource::File(src);

    // Part 2 keeps failing past the in-call retries: the upload gives up.
    fake.fail.lock().unwrap().insert("PUT 2".into(), 100);
    let failed = upload(&client, job(&manifest, &source)).await.unwrap_err();
    assert!(failed.is_transient(), "{failed}");

    fake.fail.lock().unwrap().clear();
    fake.log.lock().unwrap().clear();
    upload(&client, job(&manifest, &source)).await.unwrap();
    assert_eq!(*fake.log.lock().unwrap(), vec!["PUT 2".to_owned()]);
}

#[tokio::test]
async fn a_download_survives_corruption_and_resumes_after_failing() {
    let (fake, client) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("clip.mov");
    let bytes = data(PART as usize * 3 + 99);
    std::fs::write(&src, &bytes).unwrap();
    let manifest = manifest_for(&src);
    upload(&client, job(&manifest, &PartSource::File(src)))
        .await
        .unwrap();

    let dest = dir.path().join("dest");
    let work = dir.path().join("work");
    fake.corrupt.lock().unwrap().insert("GET 1".into());
    fake.fail.lock().unwrap().insert("GET 3".into(), 100);
    let failed = download(&client, "chat", "t1", &dest, &work, None)
        .await
        .unwrap_err();
    assert!(failed.is_transient(), "{failed}");
    assert!(!dest.exists(), "nothing lands until every part is verified");

    fake.fail.lock().unwrap().clear();
    fake.log.lock().unwrap().clear();
    download(&client, "chat", "t1", &dest, &work, None)
        .await
        .unwrap();
    assert_eq!(*fake.log.lock().unwrap(), vec!["GET 3".to_owned()]);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
}

#[tokio::test]
async fn a_download_waits_for_the_seal_and_stops_on_a_cancel() {
    let (_fake, client) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("clip.mov");
    std::fs::write(&src, data(PART as usize + 1)).unwrap();
    let manifest = manifest_for(&src);
    client.create("chat", "t1", &manifest).await.unwrap();

    let dest = dir.path().join("dest");
    let early = download(&client, "chat", "t1", &dest, &dir.path().join("w"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            early,
            TransferError::NotReady {
                present: 0,
                total: 2,
                ..
            }
        ),
        "{early}"
    );

    client.delete("chat", "t1", true).await.unwrap();
    let gone = download(&client, "chat", "t1", &dest, &dir.path().join("w"), None)
        .await
        .unwrap_err();
    assert!(matches!(gone, TransferError::Cancelled), "{gone}");
    let missing = download(&client, "chat", "nope", &dest, &dir.path().join("w"), None)
        .await
        .unwrap_err();
    assert!(matches!(missing, TransferError::NotFound), "{missing}");
}
