//! In-lightbox video playback and OS-player fallback.
//!
//! macOS paints AVPlayer frames through GPUI's existing `surface` path
//! (`420f` CVPixelBuffers). Linux/other keep the same chrome and fall back
//! to `xdg-open` / `open` so we do not invent a second viewer.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, Hsla, ObjectFit, SharedString, Window, div, hsla, prelude::*, px};
use zeron_studio::{MediaKind, StudioArtifactId};

use crate::theme::Theme;

use super::page::StudioPage;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Hover must dwell this long before we fetch bytes and spin up a player.
pub(super) const HOVER_AUTOPLAY_DELAY: Duration = Duration::from_millis(200);
/// Matches gallery and thread tile rounding. GPUI surfaces ignore `rounded`
/// (they paint a square), so hover playback covers these corners itself.
/// Linux hover playback does not paint a surface, so the constant is unused
/// outside macOS except in tests.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const TILE_CORNER_RADIUS: f32 = 10.0;

/// Overlay plate for a duration chip sitting on media. Always a dark fill
/// with light type — `ink()` + `theme.text` both go dark in light mode, so
/// a themed plate would paint dark-on-dark over the poster.
pub(super) fn duration_overlay_badge(theme: &Theme, seconds: Option<f64>) -> gpui::Div {
    div()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .bg(if theme.is_glass() {
            theme.glass_overlay()
        } else {
            theme.surface_overlay
        })
        .text_size(px(11.0))
        .line_height(px(14.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(hsla(0.0, 0.0, 1.0, 0.96))
        .child(SharedString::from(format_duration_badge(seconds)))
}

/// Badge / inspector clock for a duration in seconds (`0:06`, `1:05`).
pub(super) fn format_duration_badge(seconds: Option<f64>) -> String {
    crate::video::format_timecode(seconds)
}

/// First-frame JPEG for a feed / filmstrip poster. macOS uses AVFoundation;
/// other platforms return `None` and the tile keeps a play plate.
pub(super) fn poster_jpeg_from_video_bytes(bytes: Vec<u8>) -> Option<Vec<u8>> {
    zeron_engine::studio_preview::poster_jpeg_from_video_bytes(&bytes)
}

pub(super) fn hover_autoplay_supported() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
fn hover_autoplay_ready(hovered_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(hovered_at) >= HOVER_AUTOPLAY_DELAY
}

/// Square-minus-quarter-circle ears at each corner of `bounds`.
///
/// `gpui::surface` paints a rectangle and does not honor `corner_radii`
/// (upstream TODO). Parent `overflow_hidden` is also a rectangular mask,
/// so hover playback would otherwise square off the tile. These fills sit
/// on the video and restore the page color in the four ears.
#[cfg(target_os = "macos")]
fn paint_rounded_corner_covers(
    bounds: gpui::Bounds<gpui::Pixels>,
    radius: f32,
    color: Hsla,
    window: &mut Window,
) {
    let max_r = f32::from(bounds.size.width).min(f32::from(bounds.size.height)) / 2.0;
    let r = px(radius.min(max_r));
    if f32::from(r) <= 0.0 {
        return;
    }
    let origin = bounds.origin;
    let right = origin.x + bounds.size.width;
    let bottom = origin.y + bounds.size.height;
    let radii = gpui::point(r, r);
    let mut builder = gpui::PathBuilder::fill();
    // Each ear: outer corner → along the horizontal toward the tile →
    // short arc along the rounded edge → close.
    builder.move_to(origin);
    builder.line_to(gpui::point(origin.x + r, origin.y));
    builder.arc_to(
        radii,
        px(0.0),
        false,
        false,
        gpui::point(origin.x, origin.y + r),
    );
    builder.close();
    builder.move_to(gpui::point(right, origin.y));
    builder.line_to(gpui::point(right - r, origin.y));
    builder.arc_to(
        radii,
        px(0.0),
        false,
        true,
        gpui::point(right, origin.y + r),
    );
    builder.close();
    builder.move_to(gpui::point(right, bottom));
    builder.line_to(gpui::point(right - r, bottom));
    builder.arc_to(radii, px(0.0), false, false, gpui::point(right, bottom - r));
    builder.close();
    builder.move_to(gpui::point(origin.x, bottom));
    builder.line_to(gpui::point(origin.x + r, bottom));
    builder.arc_to(
        radii,
        px(0.0),
        false,
        true,
        gpui::point(origin.x, bottom - r),
    );
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

pub(super) fn video_file_extension(mime: &str) -> &'static str {
    match mime {
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        _ => "mp4",
    }
}

pub(super) fn video_temp_path(artifact_id: StudioArtifactId, mime: &str) -> PathBuf {
    // Unique suffix so Drop can unlink without racing an OS-player that still
    // holds the previous file for this artifact.
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "zeron-studio-{}-{n}.{}",
        artifact_id.0,
        video_file_extension(mime)
    ))
}

/// Launch the OS player. `spawn` must not drop an unreaped child.
pub(super) fn open_path_in_os_player(path: &Path) -> Result<(), String> {
    let mut command = os_player_command(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn os_player_command(path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(path);
        command
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let mut command = Command::new("open");
        command.arg(path);
        command
    }
}

const EOS_EPSILON: f64 = 0.04;

pub(super) fn should_restart_from_start(position: f64, duration: Option<f64>) -> bool {
    duration.is_some_and(|duration| duration > 0.0 && position >= duration - EOS_EPSILON)
}

/// Session for the video currently on the lightbox stage.
pub(super) struct StudioVideoPlayback {
    pub artifact_id: StudioArtifactId,
    pub path: Option<PathBuf>,
    pub loading: bool,
    pub error: Option<String>,
    pub playing: bool,
    pub muted: bool,
    pub duration: Option<f64>,
    pub position: f64,
    pub pending_os_open: bool,
    keep_temp: bool,
    started: Option<Instant>,
    native: Option<NativePlayer>,
    /// Extra frame pulls after a seek so a paused player still shows the new picture.
    seek_settle: u8,
    /// Hover previews restart at the end instead of pausing on the last frame.
    looping: bool,
    _not_send: PhantomData<*const ()>,
}

impl Drop for StudioVideoPlayback {
    fn drop(&mut self) {
        self.pause();
        self.unlink_temp();
    }
}

impl StudioVideoPlayback {
    pub(super) fn loading(artifact_id: StudioArtifactId, duration: Option<f64>) -> Self {
        Self {
            artifact_id,
            path: None,
            loading: true,
            error: None,
            playing: false,
            muted: false,
            duration,
            position: 0.0,
            pending_os_open: false,
            keep_temp: false,
            started: None,
            native: None,
            seek_settle: 0,
            looping: false,
            _not_send: PhantomData,
        }
    }

    pub(super) fn ready_from_file(
        artifact_id: StudioArtifactId,
        path: PathBuf,
        duration: Option<f64>,
    ) -> Self {
        let native = NativePlayer::open(&path);
        let duration = native
            .as_ref()
            .and_then(NativePlayer::duration)
            .or(duration);
        Self {
            artifact_id,
            path: Some(path),
            loading: false,
            error: None,
            playing: false,
            muted: false,
            duration,
            position: 0.0,
            pending_os_open: false,
            keep_temp: false,
            started: None,
            native,
            seek_settle: 0,
            looping: false,
            _not_send: PhantomData,
        }
    }

    #[cfg(test)]
    fn owned_file_without_player(
        artifact_id: StudioArtifactId,
        path: PathBuf,
        duration: Option<f64>,
    ) -> Self {
        Self {
            artifact_id,
            path: Some(path),
            loading: false,
            error: None,
            playing: false,
            muted: false,
            duration,
            position: 0.0,
            pending_os_open: false,
            keep_temp: false,
            started: None,
            native: None,
            seek_settle: 0,
            looping: false,
            _not_send: PhantomData,
        }
    }

    pub(super) fn failed(artifact_id: StudioArtifactId, error: String) -> Self {
        Self {
            artifact_id,
            path: None,
            loading: false,
            error: Some(error),
            playing: false,
            muted: false,
            duration: None,
            position: 0.0,
            pending_os_open: false,
            keep_temp: false,
            started: None,
            native: None,
            seek_settle: 0,
            looping: false,
            _not_send: PhantomData,
        }
    }

    fn unlink_temp(&mut self) {
        if self.keep_temp {
            return;
        }
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(super) fn can_play_in_app(&self) -> bool {
        self.native
            .as_ref()
            .is_some_and(NativePlayer::can_paint_frames)
            && self.path.is_some()
            && self.error.is_none()
    }

    /// In-app only. Never launches the OS player (autoplay / macOS path).
    pub(super) fn autoplay(&mut self) -> PlayOutcome {
        if !self.can_play_in_app() {
            return if self.loading {
                PlayOutcome::Pending
            } else if let Some(error) = &self.error {
                PlayOutcome::Failed(error.clone())
            } else {
                PlayOutcome::Pending
            };
        }
        self.play_in_app()
    }

    fn play_in_app(&mut self) -> PlayOutcome {
        if self.loading {
            return PlayOutcome::Pending;
        }
        if let Some(error) = &self.error {
            return PlayOutcome::Failed(error.clone());
        }
        let Some(native) = self.native.as_mut() else {
            return PlayOutcome::Pending;
        };
        if !native.can_paint_frames() {
            return PlayOutcome::Pending;
        }
        if should_restart_from_start(self.position, self.duration) {
            native.seek_zero();
            self.position = 0.0;
        }
        native.play();
        self.playing = true;
        self.started = Some(Instant::now());
        PlayOutcome::InApp
    }

    /// User play control: in-app when possible, otherwise OS-player fallback.
    pub(super) fn play(&mut self) -> PlayOutcome {
        match self.play_in_app() {
            PlayOutcome::Pending
                if !self.loading && self.path.is_some() && self.error.is_none() =>
            {
                self.open_os_player()
            }
            other => other,
        }
    }

    pub(super) fn pause(&mut self) {
        if let Some(native) = self.native.as_mut() {
            native.pause();
            self.position = native.position().unwrap_or(self.position);
        } else if let Some(started) = self.started.take() {
            self.position = (self.position + started.elapsed().as_secs_f64())
                .min(self.duration.unwrap_or(self.position));
        }
        self.playing = false;
        self.started = None;
    }

    pub(super) fn toggle(&mut self) -> PlayOutcome {
        if self.playing {
            self.pause();
            PlayOutcome::Paused
        } else {
            self.play()
        }
    }

    pub(super) fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Some(native) = self.native.as_mut() {
            native.set_muted(muted);
        }
    }

    pub(super) fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub(super) fn toggle_mute(&mut self) {
        self.set_muted(!self.muted);
    }

    pub(super) fn seek(&mut self, seconds: f64) {
        let seconds = match self.duration {
            Some(duration) if duration > 0.0 => seconds.clamp(0.0, duration),
            _ => seconds.max(0.0),
        };
        self.position = seconds;
        self.seek_settle = crate::video::seek_settle_frames();
        if let Some(native) = self.native.as_mut() {
            native.seek(seconds);
        }
    }

    /// Inspector Open / deferred fetch. Loading is not a user-facing error.
    pub(super) fn request_os_open(&mut self) -> Result<(), String> {
        if self.loading || self.path.is_none() {
            self.pending_os_open = true;
            return Ok(());
        }
        match self.open_os_player() {
            PlayOutcome::OsFallback => Ok(()),
            PlayOutcome::Failed(error) => Err(error),
            _ => Ok(()),
        }
    }

    fn open_os_player(&mut self) -> PlayOutcome {
        let Some(path) = self.path.clone() else {
            self.pending_os_open = true;
            return PlayOutcome::Pending;
        };
        match open_path_in_os_player(&path) {
            Ok(()) => {
                // OS player may still be reading; unique suffix lets us leak
                // this copy instead of unlinking under the child.
                self.keep_temp = true;
                self.pending_os_open = false;
                PlayOutcome::OsFallback
            }
            Err(error) => {
                self.error = Some(error.clone());
                PlayOutcome::Failed(error)
            }
        }
    }

    pub(super) fn needs_frame(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.can_play_in_app()
                && (self.playing || self.frame().is_none() || self.seek_settle > 0)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    pub(super) fn step(&mut self) -> bool {
        let Some(native) = self.native.as_mut() else {
            return false;
        };
        let changed = native.pull_frame();
        if changed {
            self.seek_settle = 0;
        } else if self.seek_settle > 0 {
            self.seek_settle -= 1;
        }
        if self.seek_settle == 0 {
            if let Some(position) = native.position() {
                self.position = position;
            }
        }
        if let Some(duration) = native.duration().or(self.duration) {
            self.duration = Some(duration);
            if self.playing && should_restart_from_start(self.position, Some(duration)) {
                if self.looping {
                    native.seek_zero();
                    native.play();
                    self.position = 0.0;
                    self.seek_settle = crate::video::seek_settle_frames();
                } else {
                    native.pause();
                    self.playing = false;
                    self.started = None;
                    self.position = duration;
                }
            }
        }
        changed || self.playing
    }

    #[cfg(target_os = "macos")]
    pub(super) fn frame(&self) -> Option<core_video::pixel_buffer::CVPixelBuffer> {
        self.native.as_ref().and_then(NativePlayer::frame)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlayOutcome {
    InApp,
    OsFallback,
    Paused,
    Pending,
    Failed(String),
}

struct NativePlayer {
    #[cfg(target_os = "macos")]
    inner: macos::AvPlayer,
    #[cfg(not(target_os = "macos"))]
    _linux: (),
}

impl NativePlayer {
    fn open(path: &Path) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            Some(Self {
                inner: macos::AvPlayer::open(path)?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            None
        }
    }

    fn can_paint_frames(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            true
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn play(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.play();
    }

    fn pause(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.pause();
    }

    fn seek_zero(&mut self) {
        self.seek(0.0);
    }

    fn seek(&mut self, seconds: f64) {
        #[cfg(target_os = "macos")]
        self.inner.seek(seconds);
        #[cfg(not(target_os = "macos"))]
        let _ = seconds;
    }

    fn set_muted(&mut self, muted: bool) {
        #[cfg(target_os = "macos")]
        self.inner.set_muted(muted);
        #[cfg(not(target_os = "macos"))]
        let _ = muted;
    }

    fn duration(&self) -> Option<f64> {
        #[cfg(target_os = "macos")]
        {
            self.inner.duration()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn position(&self) -> Option<f64> {
        #[cfg(target_os = "macos")]
        {
            self.inner.position()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn pull_frame(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.inner.pull_frame()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    #[cfg(target_os = "macos")]
    fn frame(&self) -> Option<core_video::pixel_buffer::CVPixelBuffer> {
        self.inner.frame()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::marker::PhantomData;
    use std::path::Path;
    use std::ptr::null_mut;

    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_video::pixel_buffer::{
        CVPixelBuffer, CVPixelBufferRef, kCVPixelBufferPixelFormatTypeKey,
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    };
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CmTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    #[link(name = "AVFoundation", kind = "framework")]
    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMTimeGetSeconds(time: CmTime) -> f64;
        fn CMTimeMakeWithSeconds(seconds: f64, preferredTimescale: i32) -> CmTime;
    }

    pub(super) struct AvPlayer {
        player: *mut Object,
        output: *mut Object,
        output_attached: bool,
        frame: Option<CVPixelBuffer>,
        _not_send: PhantomData<*const ()>,
    }

    fn assert_ui_thread() {
        unsafe {
            let is_main: bool = msg_send![class!(NSThread), isMainThread];
            debug_assert!(is_main, "AVPlayer must be used on the UI thread");
        }
    }

    impl AvPlayer {
        pub(super) fn open(path: &Path) -> Option<Self> {
            assert_ui_thread();
            let path = path.to_str()?;
            let c_path = CString::new(path).ok()?;
            unsafe {
                let ns_path: *mut Object =
                    msg_send![class!(NSString), stringWithUTF8String: c_path.as_ptr()];
                if ns_path.is_null() {
                    return None;
                }
                let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: ns_path];
                if url.is_null() {
                    return None;
                }
                let player: *mut Object = msg_send![class!(AVPlayer), playerWithURL: url];
                if player.is_null() {
                    return None;
                }
                let player: *mut Object = msg_send![player, retain];
                let _: () = msg_send![player, setActionAtItemEnd: 1u64];

                let format = CFNumber::from(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange as i64);
                let key = CFString::wrap_under_get_rule(kCVPixelBufferPixelFormatTypeKey);
                let attrs: CFDictionary<CFString, CFType> =
                    CFDictionary::from_CFType_pairs(&[(key, format.as_CFType())]);
                let output: *mut Object = msg_send![class!(AVPlayerItemVideoOutput), alloc];
                let output: *mut Object = msg_send![
                    output,
                    initWithPixelBufferAttributes: attrs.as_concrete_TypeRef()
                ];
                if output.is_null() {
                    let _: () = msg_send![player, release];
                    return None;
                }
                let item: *mut Object = msg_send![player, currentItem];
                let mut output_attached = false;
                if !item.is_null() {
                    let _: () = msg_send![item, addOutput: output];
                    output_attached = true;
                }
                Some(Self {
                    player,
                    output,
                    output_attached,
                    frame: None,
                    _not_send: PhantomData,
                })
            }
        }

        pub(super) fn play(&mut self) {
            assert_ui_thread();
            unsafe {
                let _: () = msg_send![self.player, play];
            }
        }

        pub(super) fn pause(&mut self) {
            assert_ui_thread();
            unsafe {
                let _: () = msg_send![self.player, pause];
            }
        }

        pub(super) fn seek(&mut self, seconds: f64) {
            assert_ui_thread();
            unsafe {
                let time = CMTimeMakeWithSeconds(seconds.max(0.0), 600);
                let _: () = msg_send![self.player, seekToTime: time];
            }
        }

        pub(super) fn set_muted(&mut self, muted: bool) {
            assert_ui_thread();
            unsafe {
                let _: () = msg_send![self.player, setMuted: muted];
            }
        }

        pub(super) fn duration(&self) -> Option<f64> {
            unsafe {
                let item: *mut Object = msg_send![self.player, currentItem];
                if item.is_null() {
                    return None;
                }
                let time: CmTime = msg_send![item, duration];
                seconds(time)
            }
        }

        pub(super) fn position(&self) -> Option<f64> {
            unsafe {
                let time: CmTime = msg_send![self.player, currentTime];
                seconds(time)
            }
        }

        pub(super) fn pull_frame(&mut self) -> bool {
            assert_ui_thread();
            unsafe {
                let time: CmTime = msg_send![self.player, currentTime];
                let ready: bool = msg_send![self.output, hasNewPixelBufferForItemTime: time];
                if !ready {
                    return false;
                }
                let buffer: CVPixelBufferRef = msg_send![
                    self.output,
                    copyPixelBufferForItemTime: time
                    itemTimeForDisplay: null_mut::<CmTime>()
                ];
                if buffer.is_null() {
                    return false;
                }
                self.frame = Some(CVPixelBuffer::wrap_under_create_rule(buffer));
                true
            }
        }

        pub(super) fn frame(&self) -> Option<CVPixelBuffer> {
            self.frame.clone()
        }
    }

    impl Drop for AvPlayer {
        fn drop(&mut self) {
            assert_ui_thread();
            unsafe {
                if !self.player.is_null() {
                    let _: () = msg_send![self.player, pause];
                    let item: *mut Object = msg_send![self.player, currentItem];
                    if self.output_attached && !item.is_null() && !self.output.is_null() {
                        let _: () = msg_send![item, removeOutput: self.output];
                        self.output_attached = false;
                    }
                    let _: () = msg_send![
                        self.player,
                        replaceCurrentItemWithPlayerItem: null_mut::<Object>()
                    ];
                    let _: () = msg_send![self.player, release];
                    self.player = null_mut();
                }
                if !self.output.is_null() {
                    let _: () = msg_send![self.output, release];
                    self.output = null_mut();
                }
            }
        }
    }

    fn seconds(time: CmTime) -> Option<f64> {
        if time.timescale == 0 || time.flags & 1 == 0 {
            return None;
        }
        let value = unsafe { CMTimeGetSeconds(time) };
        value
            .is_finite()
            .then_some(value)
            .filter(|value| *value >= 0.0)
    }
}

impl StudioPage {
    pub(super) fn artifact_is_video(&self, id: StudioArtifactId) -> bool {
        self.lineage.media_kind(id) == Some(MediaKind::Video)
            || self
                .artifact_frame(id)
                .is_some_and(super::artifact::ArtifactFrame::is_video)
            || self
                .gallery_item(id)
                .is_some_and(|item| item.media_kind == MediaKind::Video)
            || self.conversation.as_ref().is_some_and(|view| {
                view.turns.iter().any(|turn| {
                    turn.runs.iter().any(|run| {
                        run.artifacts.iter().any(|artifact| {
                            artifact.id == id && artifact.media_kind == MediaKind::Video
                        })
                    })
                })
            })
    }

    pub(super) fn selected_is_video(&self) -> bool {
        self.selected_frame
            .and_then(|key| self.frame_by_key(key))
            .is_some_and(super::artifact::ArtifactFrame::is_video)
    }

    pub fn leave(&mut self) {
        self.stop_hover_playback();
        self.stop_video_playback();
    }

    /// Always drop playback when the lightbox goes away — even if
    /// `selected_frame` was already taken by `request_close_artifact`.
    pub(super) fn close_lightbox_session(video: &mut Option<StudioVideoPlayback>) {
        *video = None;
    }

    pub(super) fn stop_video_playback(&mut self) {
        self.video_task = None;
        self.video_frame_scheduled = false;
        Self::close_lightbox_session(&mut self.video);
    }

    pub(super) fn stop_hover_playback(&mut self) {
        self.hover_generation = self.hover_generation.saturating_add(1);
        self.hover_target = None;
        self.hover_task = None;
        self.hover_play = None;
    }

    pub(super) fn arm_hover_autoplay(&mut self, id: StudioArtifactId, cx: &mut Context<Self>) {
        if !hover_autoplay_supported() || crate::motion::reduced_motion(cx) {
            return;
        }
        if self.selected_frame.is_some() {
            return;
        }
        if self.hover_target == Some(id) {
            return;
        }
        self.stop_hover_playback();
        self.hover_target = Some(id);
        self.hover_generation = self.hover_generation.saturating_add(1);
        let generation = self.hover_generation;
        self.hover_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOVER_AUTOPLAY_DELAY).await;
            this.update(cx, |page, cx| {
                if page.hover_generation != generation || page.hover_target != Some(id) {
                    return;
                }
                if page.selected_frame.is_some() || crate::motion::reduced_motion(cx) {
                    return;
                }
                page.start_hover_playback(id, cx);
            })
            .ok();
        }));
    }

    pub(super) fn disarm_hover_autoplay(&mut self, id: StudioArtifactId, cx: &mut Context<Self>) {
        if self.hover_target == Some(id) {
            self.stop_hover_playback();
            cx.notify();
        }
    }

    fn start_hover_playback(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        if self
            .hover_play
            .as_ref()
            .is_some_and(|player| player.artifact_id == artifact_id && player.error.is_none())
        {
            if let Some(player) = self.hover_play.as_mut()
                && !player.playing
                && !player.loading
            {
                player.set_muted(true);
                player.set_looping(true);
                let _ = player.autoplay();
            }
            cx.notify();
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let (mime, duration) = self.hover_video_meta(artifact_id);
        let mut loading = StudioVideoPlayback::loading(artifact_id, duration);
        loading.set_muted(true);
        loading.set_looping(true);
        self.hover_play = Some(loading);
        self.hover_task = Some(cx.spawn(async move |this, cx| {
            let loaded = super::artifact::read_artifact_bytes(&engine, artifact_id).await;
            this.update(cx, |page, cx| {
                if page.hover_target != Some(artifact_id) {
                    return;
                }
                match loaded {
                    Ok((_, mime_type, bytes)) => {
                        let mime = if mime_type.is_empty() {
                            mime
                        } else {
                            mime_type
                        };
                        let path = video_temp_path(artifact_id, &mime);
                        match std::fs::write(&path, bytes) {
                            Ok(()) => {
                                let mut session = StudioVideoPlayback::ready_from_file(
                                    artifact_id,
                                    path,
                                    duration,
                                );
                                session.set_muted(true);
                                session.set_looping(true);
                                page.hover_play = Some(session);
                                if let Some(player) = page.hover_play.as_mut() {
                                    let _ = player.autoplay();
                                }
                            }
                            Err(_) => {
                                page.hover_play = None;
                            }
                        }
                    }
                    Err(_) => {
                        page.hover_play = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn hover_video_meta(&self, id: StudioArtifactId) -> (String, Option<f64>) {
        if let Some(item) = self.gallery_item(id) {
            return (item.mime_type.clone(), item.duration_seconds);
        }
        if let Some(frame) = self.artifact_frame(id) {
            return (frame.mime_type.clone(), frame.duration_seconds);
        }
        let from_conversation = self.conversation.as_ref().and_then(|view| {
            view.turns
                .iter()
                .flat_map(|turn| &turn.runs)
                .flat_map(|run| &run.artifacts)
                .find(|artifact| artifact.id == id)
        });
        match from_conversation {
            Some(artifact) => (artifact.mime_type.clone(), artifact.duration_seconds),
            None => ("video/mp4".into(), None),
        }
    }

    pub(super) fn hover_video_layer(
        &self,
        id: StudioArtifactId,
        fit: ObjectFit,
        cover: Hsla,
    ) -> Option<AnyElement> {
        let player = self.hover_play.as_ref()?;
        if player.artifact_id != id {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            let buffer = player.frame()?;
            let radius = px(TILE_CORNER_RADIUS);
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .overflow_hidden()
                    .rounded(radius)
                    .child(
                        gpui::surface(buffer)
                            .object_fit(fit)
                            .size_full()
                            .rounded(radius),
                    )
                    .child(
                        gpui::canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                paint_rounded_corner_covers(
                                    bounds,
                                    TILE_CORNER_RADIUS,
                                    cover,
                                    window,
                                );
                            },
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    )
                    .into_any_element(),
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (fit, cover);
            None
        }
    }

    pub(super) fn sync_video_playback(&mut self, cx: &mut Context<Self>) {
        let Some(frame) = self
            .selected_frame
            .and_then(|key| self.frame_by_key(key))
            .cloned()
        else {
            self.stop_video_playback();
            return;
        };
        if !frame.is_video() {
            self.stop_video_playback();
            return;
        }
        let Some(artifact_id) = frame.artifact_id() else {
            self.stop_video_playback();
            return;
        };
        if self
            .video
            .as_ref()
            .is_some_and(|player| player.artifact_id == artifact_id && player.error.is_none())
        {
            if self.video.as_ref().is_some_and(|player| {
                player.can_play_in_app() && !player.playing && !player.loading
            }) {
                self.autoplay_selected_video(cx);
            }
            return;
        }
        self.ensure_video_ready(artifact_id, frame.duration_seconds, frame.mime_type, cx);
    }

    fn ensure_video_ready(
        &mut self,
        artifact_id: StudioArtifactId,
        duration: Option<f64>,
        mime: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(player) = self.video.as_mut()
            && player.artifact_id == artifact_id
            && (player.path.is_some() || player.loading)
        {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            self.video = Some(StudioVideoPlayback::failed(
                artifact_id,
                "engine is unavailable".into(),
            ));
            return;
        };
        let pending_os_open = self
            .video
            .as_ref()
            .is_some_and(|player| player.artifact_id == artifact_id && player.pending_os_open);
        let muted = self
            .video
            .as_ref()
            .is_some_and(|player| player.artifact_id == artifact_id && player.muted);
        let mut loading = StudioVideoPlayback::loading(artifact_id, duration);
        loading.pending_os_open = pending_os_open;
        loading.set_muted(muted);
        self.video = Some(loading);
        self.video_task = Some(cx.spawn(async move |this, cx| {
            let loaded = super::artifact::read_artifact_bytes(&engine, artifact_id).await;
            this.update(cx, |page, cx| {
                page.video_task = None;
                if page
                    .video
                    .as_ref()
                    .is_none_or(|player| player.artifact_id != artifact_id)
                {
                    return;
                }
                match loaded {
                    Ok((_, mime_type, bytes)) => {
                        let mime = if mime_type.is_empty() {
                            mime
                        } else {
                            mime_type
                        };
                        let path = video_temp_path(artifact_id, &mime);
                        match std::fs::write(&path, bytes) {
                            Ok(()) => {
                                let pending = page
                                    .video
                                    .as_ref()
                                    .is_some_and(|player| player.pending_os_open);
                                let muted = page.video.as_ref().is_some_and(|player| player.muted);
                                let mut session = StudioVideoPlayback::ready_from_file(
                                    artifact_id,
                                    path,
                                    duration,
                                );
                                session.set_muted(muted);
                                page.video = Some(session);
                                if pending {
                                    page.open_ready_os_player(cx);
                                } else {
                                    page.autoplay_selected_video(cx);
                                }
                            }
                            Err(error) => {
                                page.video = Some(StudioVideoPlayback::failed(
                                    artifact_id,
                                    error.to_string(),
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        page.video = Some(StudioVideoPlayback::failed(artifact_id, error));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn autoplay_selected_video(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.video.as_mut() else {
            return;
        };
        match player.autoplay() {
            PlayOutcome::Failed(error) => self.error = Some(error.into()),
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn toggle_selected_video(&mut self, cx: &mut Context<Self>) {
        if !self.selected_is_video() {
            return;
        }
        if self.video.is_none() {
            self.sync_video_playback(cx);
            return;
        }
        if let Some(player) = self.video.as_mut() {
            match player.toggle() {
                PlayOutcome::Failed(error) => self.error = Some(error.into()),
                PlayOutcome::Paused
                | PlayOutcome::InApp
                | PlayOutcome::OsFallback
                | PlayOutcome::Pending => {}
            }
        }
        cx.notify();
    }

    pub(super) fn toggle_selected_video_mute(&mut self, cx: &mut Context<Self>) {
        if !self.selected_is_video() {
            return;
        }
        if let Some(player) = self.video.as_mut() {
            player.toggle_mute();
        }
        cx.notify();
    }

    pub(super) fn seek_selected_video(&mut self, seconds: f64, cx: &mut Context<Self>) {
        if !self.selected_is_video() {
            return;
        }
        if let Some(player) = self.video.as_mut() {
            player.seek(seconds);
        }
        cx.notify();
    }

    fn open_ready_os_player(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.video.as_mut() else {
            return;
        };
        if let Err(error) = player.request_os_open() {
            self.error = Some(error.into());
        }
        cx.notify();
    }

    pub(super) fn open_video_in_os_player(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        if let Some(player) = self.video.as_mut()
            && player.artifact_id == artifact_id
        {
            if let Err(error) = player.request_os_open() {
                self.error = Some(error.into());
                cx.notify();
            }
            if player.path.is_some() {
                return;
            }
            // Still fetching — pending_os_open is set; do not error.
            cx.notify();
            return;
        }
        let mime = self
            .artifact_frame(artifact_id)
            .map(|frame| frame.mime_type.clone())
            .unwrap_or_else(|| "video/mp4".into());
        let duration = self
            .artifact_frame(artifact_id)
            .and_then(|frame| frame.duration_seconds);
        self.ensure_video_ready(artifact_id, duration, mime, cx);
        if let Some(player) = self.video.as_mut()
            && let Err(error) = player.request_os_open()
        {
            self.error = Some(error.into());
        }
        cx.notify();
    }

    pub(super) fn video_needs_frame(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(StudioVideoPlayback::needs_frame)
            || self
                .hover_play
                .as_ref()
                .is_some_and(StudioVideoPlayback::needs_frame)
    }

    pub(super) fn step_video_frame(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let lightbox = self.video.as_mut().is_some_and(StudioVideoPlayback::step);
        let hover = self
            .hover_play
            .as_mut()
            .is_some_and(StudioVideoPlayback::step);
        if lightbox || hover {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_badge_is_clock_style() {
        assert_eq!(format_duration_badge(Some(0.0)), "0:00");
        assert_eq!(format_duration_badge(Some(6.0)), "0:06");
        assert_eq!(format_duration_badge(Some(65.4)), "1:05");
        assert_eq!(format_duration_badge(Some(3605.0)), "1:00:05");
        assert_eq!(format_duration_badge(None), "0:00");
    }

    #[test]
    fn temp_path_uses_the_artifact_id_and_mime() {
        let id = StudioArtifactId::new();
        let mp4 = video_temp_path(id, "video/mp4");
        let name = mp4.file_name().and_then(|name| name.to_str()).unwrap();
        assert!(name.contains(&id.0.to_string()));
        assert!(name.ends_with(".mp4"));
        assert!(name.contains("zeron-studio-"));
        let other = video_temp_path(id, "video/mp4");
        assert_ne!(mp4, other, "each write gets a unique suffix");
        assert!(
            video_temp_path(id, "video/quicktime")
                .to_str()
                .is_some_and(|path| path.ends_with(".mov"))
        );
    }

    #[test]
    fn os_player_command_points_at_the_file() {
        let path = PathBuf::from("/tmp/zeron-studio-test.mp4");
        let command = os_player_command(&path);
        let program = command.get_program().to_string_lossy();
        #[cfg(target_os = "macos")]
        assert_eq!(program, "open");
        #[cfg(target_os = "linux")]
        assert_eq!(program, "xdg-open");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["/tmp/zeron-studio-test.mp4"]);
    }

    #[test]
    fn close_after_open_clears_video() {
        // request_close_artifact takes selected_frame, then close_artifact
        // runs with it already None — playback must still be dropped.
        let mut selected = Some(0);
        let mut video = Some(StudioVideoPlayback::failed(
            StudioArtifactId::new(),
            "x".into(),
        ));
        selected.take();
        StudioPage::close_lightbox_session(&mut video);
        assert!(selected.is_none());
        assert!(video.is_none());
        let mut video = Some(StudioVideoPlayback::failed(
            StudioArtifactId::new(),
            "x".into(),
        ));
        if selected.take().is_some() {
            panic!("selection already taken must not be the only stop path");
        }
        StudioPage::close_lightbox_session(&mut video);
        assert!(video.is_none());
    }

    #[test]
    fn drop_unlinks_temp_unless_kept_for_os_player() {
        let path = std::env::temp_dir().join(format!(
            "zeron-studio-drop-test-{}.mp4",
            StudioArtifactId::new().0
        ));
        std::fs::write(&path, b"not-a-video").unwrap();
        {
            let session = StudioVideoPlayback::owned_file_without_player(
                StudioArtifactId::new(),
                path.clone(),
                Some(1.0),
            );
            assert!(path.exists());
            drop(session);
        }
        assert!(!path.exists());

        std::fs::write(&path, b"not-a-video").unwrap();
        {
            let mut session = StudioVideoPlayback::owned_file_without_player(
                StudioArtifactId::new(),
                path.clone(),
                Some(1.0),
            );
            session.keep_temp = true;
            drop(session);
        }
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn autoplay_stays_in_lightbox_without_in_app_player() {
        let path = std::env::temp_dir().join(format!(
            "zeron-studio-autoplay-test-{}.mp4",
            StudioArtifactId::new().0
        ));
        std::fs::write(&path, b"not-a-video").unwrap();
        let mut session = StudioVideoPlayback::owned_file_without_player(
            StudioArtifactId::new(),
            path.clone(),
            Some(6.0),
        );
        assert!(!session.can_play_in_app());
        assert_eq!(session.autoplay(), PlayOutcome::Pending);
        assert!(!session.playing);
        drop(session);
        let _ = std::fs::remove_file(&path);

        let mut loading = StudioVideoPlayback::loading(StudioArtifactId::new(), Some(6.0));
        assert_eq!(loading.autoplay(), PlayOutcome::Pending);
        assert!(!loading.playing);
        assert_eq!(loading.toggle(), PlayOutcome::Pending);
        loading.playing = true;
        assert_eq!(loading.toggle(), PlayOutcome::Paused);
        assert!(!loading.playing);
    }

    #[test]
    fn os_open_while_loading_is_deferred_not_an_error() {
        let mut session = StudioVideoPlayback::loading(StudioArtifactId::new(), Some(6.0));
        assert!(session.request_os_open().is_ok());
        assert!(session.pending_os_open);
        assert!(session.error.is_none());
    }

    #[test]
    fn end_of_stream_restarts_from_the_start() {
        assert!(!should_restart_from_start(0.0, Some(6.0)));
        assert!(!should_restart_from_start(5.9, Some(6.0)));
        assert!(should_restart_from_start(6.0, Some(6.0)));
        assert!(should_restart_from_start(5.97, Some(6.0)));
        assert!(!should_restart_from_start(1.0, None));
    }

    #[test]
    fn mute_toggles_without_a_native_player() {
        let mut session = StudioVideoPlayback::loading(StudioArtifactId::new(), Some(6.0));
        assert!(!session.muted);
        session.toggle_mute();
        assert!(session.muted);
        session.toggle_mute();
        assert!(!session.muted);
    }

    #[test]
    fn seek_clamps_to_duration() {
        let mut session = StudioVideoPlayback::loading(StudioArtifactId::new(), Some(6.0));
        session.seek(2.5);
        assert_eq!(session.position, 2.5);
        session.seek(-1.0);
        assert_eq!(session.position, 0.0);
        session.seek(9.0);
        assert_eq!(session.position, 6.0);
        session.seek(0.0);
        assert_eq!(session.position, 0.0);
    }

    #[test]
    fn hover_autoplay_waits_200ms() {
        let start = Instant::now();
        assert!(!hover_autoplay_ready(start, start));
        assert!(!hover_autoplay_ready(
            start,
            start + Duration::from_millis(199)
        ));
        assert!(hover_autoplay_ready(
            start,
            start + Duration::from_millis(200)
        ));
        assert_eq!(HOVER_AUTOPLAY_DELAY, Duration::from_millis(200));
        assert_eq!(hover_autoplay_supported(), cfg!(target_os = "macos"));
        assert_eq!(TILE_CORNER_RADIUS, 10.0);
    }

    #[test]
    fn hover_preview_is_muted_and_looping() {
        let mut session = StudioVideoPlayback::loading(StudioArtifactId::new(), Some(6.0));
        assert!(!session.muted);
        assert!(!session.looping);
        session.set_muted(true);
        session.set_looping(true);
        assert!(session.muted);
        assert!(session.looping);
    }
}
