//! Hashing a file into parts, and splitting one into a file per part.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Part, hex};

/// Extension of a split part file: `{dir}/{n}.part`.
pub const PART_FILE_EXT: &str = "part";
const BUF: usize = 1024 * 1024;
/// Records the source's size before the first truncation, so an interrupted
/// split resumes knowing where every part begins.
const SPLIT_STATE: &str = "split-size";

pub(crate) fn part_path(dir: &Path, n: usize) -> PathBuf {
    dir.join(format!("{n}.{PART_FILE_EXT}"))
}

fn part_count(size: u64, part_size: u64) -> usize {
    size.div_ceil(part_size) as usize
}

/// Hash `len` bytes from `reader`, copying them to `copy` when given.
fn hash_region(
    reader: &mut impl Read,
    len: u64,
    mut copy: Option<&mut File>,
) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF.min(len.max(1) as usize)];
    let mut left = len;
    while left > 0 {
        let want = buf.len().min(left as usize);
        reader.read_exact(&mut buf[..want])?;
        hasher.update(&buf[..want]);
        if let Some(out) = copy.as_deref_mut() {
            out.write_all(&buf[..want])?;
        }
        left -= want as u64;
    }
    Ok(hex(&hasher.finalize()))
}

/// The file's size and its parts, read in one streaming pass (blocking).
pub fn plan_file(path: &Path, part_size: u64) -> io::Result<(u64, Vec<Part>)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut parts = Vec::with_capacity(part_count(size, part_size));
    for n in 0..part_count(size, part_size) {
        let len = part_size.min(size - n as u64 * part_size);
        parts.push(Part {
            sha256: hash_region(&mut file, len, None)?,
            size: len,
        });
    }
    Ok((size, parts))
}

/// Move `src` into `dir` as one file per part (`{n}.part`), hashing each, and
/// remove `src` (blocking). Parts are cut from the end and the source is
/// truncated after each, so the split needs one part of free space, not a
/// second copy of the file. Interrupted, it resumes where it stopped.
pub fn split_into_parts(src: &Path, dir: &Path, part_size: u64) -> io::Result<(u64, Vec<Part>)> {
    std::fs::create_dir_all(dir)?;
    let state = dir.join(SPLIT_STATE);
    let size = match std::fs::read_to_string(&state) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "corrupt split state"))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let size = std::fs::metadata(src)?.len();
            write_synced(&state, size.to_string().as_bytes())?;
            size
        }
        Err(e) => return Err(e),
    };
    let count = part_count(size, part_size);
    let mut parts = vec![None; count];
    // A source already gone means every part was cut before the interruption.
    let mut source = match std::fs::OpenOptions::new().read(true).write(true).open(src) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    for n in (0..count).rev() {
        let offset = n as u64 * part_size;
        let len = part_size.min(size - offset);
        let path = part_path(dir, n);
        let remaining = match &source {
            Some(file) => file.metadata()?.len(),
            None => 0,
        };
        if remaining <= offset {
            // Cut before the interruption: the part file holds it.
            let mut part = File::open(&path)?;
            if part.metadata()?.len() != len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("part {n} is truncated"),
                ));
            }
            parts[n] = Some(Part {
                sha256: hash_region(&mut part, len, None)?,
                size: len,
            });
            continue;
        }
        let file = source.as_mut().expect("a source longer than offset exists");
        file.seek(SeekFrom::Start(offset))?;
        let mut out = File::create(&path)?;
        let sha256 = hash_region(file, len, Some(&mut out))?;
        // The part must be durable before the bytes it copies are dropped.
        out.sync_all()?;
        file.set_len(offset)?;
        parts[n] = Some(Part { sha256, size: len });
    }
    if source.is_some() {
        drop(source);
        std::fs::remove_file(src)?;
    }
    let _ = std::fs::remove_file(&state);
    Ok((
        size,
        parts
            .into_iter()
            .map(|p| p.expect("every part was cut"))
            .collect(),
    ))
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn split_matches_plan_and_reassembles() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("video.mov");
        let data = bytes(10 * 1024 + 17);
        std::fs::write(&src, &data).unwrap();
        let planned = plan_file(&src, 4096).unwrap();

        let parts_dir = dir.path().join("parts");
        let split = split_into_parts(&src, &parts_dir, 4096).unwrap();
        assert_eq!(split, planned);
        assert_eq!(split.1.len(), 3);
        assert!(!src.exists());
        let joined: Vec<u8> = (0..3)
            .flat_map(|n| std::fs::read(part_path(&parts_dir, n)).unwrap())
            .collect();
        assert_eq!(joined, data);
    }

    #[test]
    fn split_resumes_after_an_interruption() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip");
        let data = bytes(3 * 4096 + 5);
        std::fs::write(&src, &data).unwrap();
        let planned = plan_file(&src, 4096).unwrap();

        // Simulate a crash after cutting the last part.
        let parts_dir = dir.path().join("parts");
        std::fs::create_dir_all(&parts_dir).unwrap();
        std::fs::write(parts_dir.join(SPLIT_STATE), data.len().to_string()).unwrap();
        std::fs::write(part_path(&parts_dir, 3), &data[3 * 4096..]).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
        f.set_len(3 * 4096).unwrap();

        assert_eq!(split_into_parts(&src, &parts_dir, 4096).unwrap(), planned);
    }

    #[test]
    fn empty_file_has_no_parts() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty");
        std::fs::write(&src, b"").unwrap();
        assert_eq!(plan_file(&src, 4096).unwrap(), (0, vec![]));
        assert_eq!(
            split_into_parts(&src, &dir.path().join("p"), 4096).unwrap(),
            (0, vec![])
        );
    }
}
