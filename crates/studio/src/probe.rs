//! Pure-Rust media probes. No ffmpeg.

use std::io::Cursor;

/// Geometry and duration recovered from file bytes. Missing fields stay `None`
/// when the container cannot prove them (for example a MOV without `moov`).
#[derive(Clone, Debug, PartialEq)]
pub struct MediaProbe {
    pub mime_type: String,
    pub size_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
}

/// Probe `bytes` as `mime_type`. Unknown or unreadable containers still return
/// a probe; only the fields that can be proved are filled in.
pub fn probe_media(bytes: &[u8], mime_type: &str) -> MediaProbe {
    let (width, height, duration_seconds) = match mime_type {
        "image/jpeg" | "image/png" | "image/webp" | "image/gif" => {
            let (width, height) = probe_image(bytes);
            (width, height, None)
        }
        "video/mp4" | "video/quicktime" => probe_iso_media(bytes),
        "audio/wav" => (None, None, probe_wav(bytes)),
        "audio/mpeg" => (None, None, probe_mpeg(bytes)),
        _ => (None, None, None),
    };
    MediaProbe {
        mime_type: mime_type.to_owned(),
        size_bytes: bytes.len() as u64,
        width,
        height,
        duration_seconds,
    }
}

fn probe_image(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .map(|(width, height)| (Some(width), Some(height)))
        .unwrap_or((None, None))
}

fn probe_iso_media(bytes: &[u8]) -> (Option<u32>, Option<u32>, Option<f64>) {
    // The crate needs ftyp + moov. A QuickTime file without a readable moov
    // stays unproved — do not invent duration from mdat or free atoms.
    let Ok(reader) = mp4::Mp4Reader::read_header(Cursor::new(bytes), bytes.len() as u64) else {
        return (None, None, None);
    };
    let duration_seconds = (reader.timescale() > 0).then(|| reader.duration().as_secs_f64());
    let (width, height) = reader
        .tracks()
        .values()
        .find_map(|track| {
            let width = u32::from(track.width());
            let height = u32::from(track.height());
            (width > 0 && height > 0).then_some((width, height))
        })
        .map(|(width, height)| (Some(width), Some(height)))
        .unwrap_or((None, None));
    (width, height, duration_seconds)
}

fn probe_wav(bytes: &[u8]) -> Option<f64> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12usize;
    let mut byte_rate = None;
    let mut data_size = None;
    while offset.saturating_add(8) <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?);
        let body = offset + 8;
        if id == b"fmt " && size >= 16 && body.saturating_add(12) <= bytes.len() {
            byte_rate = Some(u32::from_le_bytes(
                bytes[body + 8..body + 12].try_into().ok()?,
            ));
        } else if id == b"data" {
            data_size = Some(size);
        }
        let padded = size as usize + (size as usize % 2);
        let next = body.saturating_add(padded);
        if next <= offset {
            break;
        }
        offset = next;
    }
    let byte_rate = byte_rate.filter(|rate| *rate > 0)?;
    Some(f64::from(data_size?) / f64::from(byte_rate))
}

fn probe_mpeg(bytes: &[u8]) -> Option<f64> {
    let frame = skip_id3(bytes)?;
    if frame.len() < 4 {
        return None;
    }
    let header = MpegAudioHeader::parse(frame)?;
    if let Some(frames) = xing_frames(frame, &header).or_else(|| vbri_frames(frame)) {
        let sample_rate = header.sample_rate? as f64;
        if sample_rate <= 0.0 {
            return None;
        }
        return Some(f64::from(frames) * f64::from(header.samples_per_frame) / sample_rate);
    }
    None
}

fn skip_id3(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() >= 10 && bytes.starts_with(b"ID3") {
        let size = synchsafe_u32(&bytes[6..10])? as usize;
        let start = 10usize.saturating_add(size);
        return bytes.get(start..);
    }
    Some(bytes)
}

fn synchsafe_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 4 || bytes.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    Some(
        (u32::from(bytes[0]) << 21)
            | (u32::from(bytes[1]) << 14)
            | (u32::from(bytes[2]) << 7)
            | u32::from(bytes[3]),
    )
}

struct MpegAudioHeader {
    samples_per_frame: u32,
    sample_rate: Option<u32>,
    side_info_bytes: usize,
}

impl MpegAudioHeader {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 4 {
            return None;
        }
        if bytes[0] != 0xff || bytes[1] & 0xe0 != 0xe0 {
            return None;
        }
        let version_id = (bytes[1] >> 3) & 0x03;
        let layer_id = (bytes[1] >> 1) & 0x03;
        if version_id == 0x01 || layer_id == 0x00 {
            return None;
        }
        let bitrate_index = bytes[2] >> 4;
        let sample_index = (bytes[2] >> 2) & 0x03;
        if bitrate_index == 0x0f || sample_index == 0x03 {
            return None;
        }
        let channel_mode = bytes[3] >> 6;
        let mpeg1 = version_id == 0x03;
        let mpeg25 = version_id == 0x00;
        let sample_rate = match (sample_index, mpeg1, mpeg25) {
            (0, true, _) => Some(44_100),
            (1, true, _) => Some(48_000),
            (2, true, _) => Some(32_000),
            (0, false, false) => Some(22_050),
            (1, false, false) => Some(24_000),
            (2, false, false) => Some(16_000),
            (0, false, true) => Some(11_025),
            (1, false, true) => Some(12_000),
            (2, false, true) => Some(8_000),
            _ => None,
        };
        let samples_per_frame = match (layer_id, mpeg1) {
            (0x03, _) => 384,      // Layer I
            (0x02, _) => 1_152,    // Layer II
            (0x01, true) => 1_152, // Layer III MPEG1
            (0x01, false) => 576,  // Layer III MPEG2/2.5
            _ => return None,
        };
        let mono = channel_mode == 0x03;
        let side_info_bytes = match (mpeg1, mono) {
            (true, true) => 17,
            (true, false) => 32,
            (false, true) => 9,
            (false, false) => 17,
        };
        Some(Self {
            samples_per_frame,
            sample_rate,
            side_info_bytes,
        })
    }
}

fn xing_frames(frame: &[u8], header: &MpegAudioHeader) -> Option<u32> {
    let start = 4usize.saturating_add(header.side_info_bytes);
    let tag = frame.get(start..start.saturating_add(8))?;
    if !tag.starts_with(b"Xing") && !tag.starts_with(b"Info") {
        return None;
    }
    let flags = u32::from_be_bytes(tag[4..8].try_into().ok()?);
    if flags & 0x0001 == 0 {
        return None;
    }
    let frames = frame.get(start + 8..start + 12)?;
    Some(u32::from_be_bytes(frames.try_into().ok()?))
}

fn vbri_frames(frame: &[u8]) -> Option<u32> {
    // VBRI sits 32 bytes after the 4-byte header.
    let start = 36usize;
    let tag = frame.get(start..start.saturating_add(16))?;
    if !tag.starts_with(b"VBRI") {
        return None;
    }
    Some(u32::from_be_bytes(tag[14..18].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{probe_media, probe_mpeg, probe_wav};

    fn rgb_png(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([10, 20, 30]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut raw), image::ImageFormat::Png)
        .unwrap();
        raw
    }

    fn pcm_wav(samples: u32, sample_rate: u32) -> Vec<u8> {
        let data_size = samples * 2;
        let byte_rate = sample_rate * 2;
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0u8, data_size as usize));
        bytes
    }

    #[test]
    fn probes_png_dimensions() {
        let bytes = rgb_png(32, 24);
        let probe = probe_media(&bytes, "image/png");
        assert_eq!(probe.width, Some(32));
        assert_eq!(probe.height, Some(24));
        assert_eq!(probe.duration_seconds, None);
        assert_eq!(probe.size_bytes, bytes.len() as u64);
    }

    #[test]
    fn probes_wav_duration_from_data_chunk() {
        let bytes = pcm_wav(8_000, 8_000);
        assert_eq!(probe_wav(&bytes), Some(1.0));
        let probe = probe_media(&bytes, "audio/wav");
        assert_eq!(probe.duration_seconds, Some(1.0));
    }

    #[test]
    fn mpeg_without_xing_or_vbri_is_unproved() {
        let header = [0xff, 0xfb, 0x90, 0x00];
        assert_eq!(probe_mpeg(&header), None);
        assert_eq!(probe_media(&header, "audio/mpeg").duration_seconds, None);
    }

    #[test]
    fn mpeg_xing_frame_count_yields_duration() {
        let mut frame = vec![0xff, 0xfb, 0x90, 0x00];
        frame.extend(std::iter::repeat_n(0u8, 32));
        frame.extend_from_slice(b"Xing");
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&100u32.to_be_bytes());
        let duration = probe_mpeg(&frame).unwrap();
        let expected = 100.0 * 1152.0 / 44_100.0;
        assert!((duration - expected).abs() < 1e-9);
    }

    #[test]
    fn mp4_without_moov_has_no_duration() {
        let mut bytes = vec![0, 0, 0, 16];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let probe = probe_media(&bytes, "video/mp4");
        assert_eq!(probe.duration_seconds, None);
        assert_eq!(probe.width, None);
        let mov = probe_media(&bytes, "video/quicktime");
        assert_eq!(mov.duration_seconds, None);
    }
}
