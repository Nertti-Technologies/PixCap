//! Streaming GIF writer for screen recordings (ADR-0003).
//!
//! Each frame is quantised and written to disk as soon as it arrives, so
//! memory stays at about one frame however long the recording runs. The
//! previous path handed every frame to ImageIO at stop, which peaked at about
//! 4.8 MB per frame (measured 2026-11-25: +1,443 MB for 300 frames of 900x506).
//!
//! The file is written under a temporary name and renamed by [`GifWriter::finish`];
//! a writer dropped without finishing deletes it, so a cancelled or failed
//! recording never leaves a truncated GIF behind.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Largest width or height accepted. An estimate: the app sends frames at most
/// 900 px wide, so this only bounds a misbehaving caller (4096^2 x 4 bytes = 64 MiB per frame).
/// The macOS recorder scales frames to fit it (`gifMaxHeight` in
/// ScreenRecorder.swift); change both together.
pub const MAX_GIF_SIDE: u32 = 4096;

/// Most frames one file may hold: 4 minutes at 15 fps (ADR-0003 §4.1). The
/// app's own cap is lower (60 s); this bounds a misbehaving caller.
pub const MAX_GIF_FRAMES: u32 = 3_600;

/// NeuQuant sampling speed, 1 (best) to 30 (fastest).
///
/// Measured 2026-11-25 with `encoder_speed_bench` (release build, 900x506
/// screen-like frame, the M-series build machine): speed 1 = 195.6 ms,
/// 5 = 47.1 ms, 10 = 28.7 ms, 15 = 24.0 ms, 30 = 17.1 ms per frame, all about
/// 60 KB per frame. 10 is the best quality inside ADR-0003's 40 ms budget
/// (the frame interval at 15 fps is 66 ms).
pub const ENCODER_SPEED: i32 = 10;

#[derive(Debug, Error)]
pub enum GifWriteError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("gif encoding failed: {0}")]
    Encoding(#[from] gif::EncodingError),
    #[error("dimensions {width}x{height} are outside 1..={MAX_GIF_SIDE}")]
    BadDimensions { width: u32, height: u32 },
    #[error("frame delay must be 1..=65535 centiseconds, got {0}")]
    BadDelay(u32),
    #[error("frame is {width}x{height}, the file is {expected_width}x{expected_height}")]
    SizeMismatch {
        width: u32,
        height: u32,
        expected_width: u32,
        expected_height: u32,
    },
    #[error("frame buffer holds {actual} bytes, expected {expected}")]
    BufferLength { actual: u64, expected: u64 },
    #[error("a GIF may hold at most {MAX_GIF_FRAMES} frames")]
    TooManyFrames,
    #[error("no frames were written")]
    NoFrames,
    #[error("an earlier frame failed to write, so the file is incomplete")]
    Broken,
}

/// Writes an infinitely looping GIF one frame at a time.
pub struct GifWriter {
    encoder: Option<gif::Encoder<BufWriter<File>>>,
    temp: PathBuf,
    target: PathBuf,
    width: u16,
    height: u16,
    frames: u32,
    /// Set when a write fails part-way; the file may hold half a frame, so it
    /// is never committed.
    broken: bool,
    committed: bool,
}

impl GifWriter {
    /// Starts a GIF at `path` whose frames are all `width` x `height`.
    pub fn create(path: impl AsRef<Path>, width: u32, height: u32) -> Result<Self, GifWriteError> {
        let in_range = |v: u32| (1..=MAX_GIF_SIDE).contains(&v);
        if !in_range(width) || !in_range(height) {
            return Err(GifWriteError::BadDimensions { width, height });
        }
        // Both fit: MAX_GIF_SIDE < u16::MAX.
        let (w, h) = (width as u16, height as u16);

        let target = path.as_ref().to_path_buf();
        let (temp, file) = crate::fsutil::create_temp_for(&target)?;

        let start = (|| {
            let mut encoder = gif::Encoder::new(BufWriter::new(file), w, h, &[])?;
            encoder.set_repeat(gif::Repeat::Infinite)?;
            Ok::<_, GifWriteError>(encoder)
        })();

        match start {
            Ok(encoder) => Ok(Self {
                encoder: Some(encoder),
                temp,
                target,
                width: w,
                height: h,
                frames: 0,
                broken: false,
                committed: false,
            }),
            Err(error) => {
                let _ = fs::remove_file(&temp);
                Err(error)
            }
        }
    }

    /// Quantises one RGBA frame (row-major, 4 bytes per pixel, no padding) and
    /// writes it, shown for `delay_cs` centiseconds.
    ///
    /// The delay is per frame so a caller that skips frames can lengthen the
    /// next one and keep playback at real-time speed.
    pub fn add_frame(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        delay_cs: u32,
    ) -> Result<(), GifWriteError> {
        if self.broken {
            return Err(GifWriteError::Broken);
        }
        if self.frames >= MAX_GIF_FRAMES {
            return Err(GifWriteError::TooManyFrames);
        }
        if width != u32::from(self.width) || height != u32::from(self.height) {
            return Err(GifWriteError::SizeMismatch {
                width,
                height,
                expected_width: self.width.into(),
                expected_height: self.height.into(),
            });
        }
        // u64 so the product cannot overflow, whatever the caller passed.
        let expected = u64::from(width) * u64::from(height) * 4;
        if rgba.len() as u64 != expected {
            return Err(GifWriteError::BufferLength {
                actual: rgba.len() as u64,
                expected,
            });
        }

        let delay_cs = u16::try_from(delay_cs)
            .ok()
            .filter(|d| *d > 0)
            .ok_or(GifWriteError::BadDelay(delay_cs))?;

        // NeuQuant rewrites the buffer in place, so it needs its own copy.
        let mut pixels = rgba.to_vec();
        let mut frame =
            gif::Frame::from_rgba_speed(self.width, self.height, &mut pixels, ENCODER_SPEED);
        frame.delay = delay_cs;

        let encoder = self.encoder.as_mut().ok_or(GifWriteError::NoFrames)?;
        if let Err(error) = encoder.write_frame(&frame) {
            self.broken = true;
            return Err(error.into());
        }
        self.frames += 1;
        Ok(())
    }

    /// Frames written so far.
    pub fn frame_count(&self) -> u32 {
        self.frames
    }

    /// Writes the trailer, flushes to disk, and moves the file into place.
    ///
    /// Returns the number of frames. With no frames the file is discarded and
    /// `NoFrames` returned, since an empty GIF is not a recording.
    pub fn finish(mut self) -> Result<u32, GifWriteError> {
        if self.broken {
            return Err(GifWriteError::Broken);
        }
        if self.frames == 0 {
            return Err(GifWriteError::NoFrames);
        }
        let encoder = self.encoder.take().ok_or(GifWriteError::NoFrames)?;
        // into_inner writes the trailer and reports errors, unlike a drop.
        let buffered = encoder.into_inner()?;
        let file = buffered.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        fs::rename(&self.temp, &self.target)?;
        self.committed = true;
        Ok(self.frames)
    }
}

impl Drop for GifWriter {
    fn drop(&mut self) {
        if !self.committed {
            // Close the file before deleting it.
            drop(self.encoder.take());
            let _ = fs::remove_file(&self.temp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pixcap-gif-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A frame with screen-like content: flat panels, a gradient, and text-like noise.
    fn frame(width: u32, height: u32, index: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let text = (x / 3 + y / 7 + index).is_multiple_of(5) && y % 20 < 12 && x > 40;
                let (r, g, b) = if text {
                    (30, 30, 30)
                } else if x < 40 {
                    (40, 44, 52)
                } else {
                    ((x * 255 / width) as u8, (y * 255 / height) as u8, 200)
                };
                pixels.extend_from_slice(&[r, g, b, 255]);
            }
        }
        pixels
    }

    #[test]
    fn writes_a_looping_gif_that_decodes() {
        let dir = scratch("roundtrip");
        let path = dir.join("rec.gif");
        let mut writer = GifWriter::create(&path, 64, 32).unwrap();
        for i in 0..5 {
            writer.add_frame(&frame(64, 32, i), 64, 32, 7).unwrap();
        }
        assert!(!path.exists(), "nothing at the final path until finish");
        assert_eq!(writer.finish().unwrap(), 5);

        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
            File::open(&path).unwrap(),
        ))
        .unwrap();
        let frames: Vec<_> = image::AnimationDecoder::into_frames(decoder)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0].buffer().dimensions(), (64, 32));
        assert_eq!(frames[0].delay().numer_denom_ms(), (70, 1));
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "temp file left behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_without_finish_leaves_nothing() {
        let dir = scratch("cancel");
        let path = dir.join("rec.gif");
        let mut writer = GifWriter::create(&path, 16, 16).unwrap();
        writer.add_frame(&frame(16, 16, 0), 16, 16, 10).unwrap();
        drop(writer);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finishing_with_no_frames_leaves_nothing() {
        let dir = scratch("empty");
        let writer = GifWriter::create(dir.join("rec.gif"), 16, 16).unwrap();
        assert!(matches!(writer.finish(), Err(GifWriteError::NoFrames)));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_input_without_panicking() {
        let dir = scratch("bad");
        let path = dir.join("rec.gif");
        assert!(matches!(
            GifWriter::create(&path, 0, 10),
            Err(GifWriteError::BadDimensions { .. })
        ));
        assert!(matches!(
            GifWriter::create(&path, 10, MAX_GIF_SIDE + 1),
            Err(GifWriteError::BadDimensions { .. })
        ));

        let mut writer = GifWriter::create(&path, 8, 8).unwrap();
        let good = frame(8, 8, 0);
        assert!(matches!(
            writer.add_frame(&good, 8, 8, 0),
            Err(GifWriteError::BadDelay(0))
        ));
        assert!(matches!(
            writer.add_frame(&good, 8, 8, 70_000),
            Err(GifWriteError::BadDelay(_))
        ));
        assert!(matches!(
            writer.add_frame(&frame(8, 4, 0), 8, 4, 10),
            Err(GifWriteError::SizeMismatch { .. })
        ));
        assert!(matches!(
            writer.add_frame(&[0; 10], 8, 8, 10),
            Err(GifWriteError::BufferLength { .. })
        ));
        assert_eq!(writer.frame_count(), 0);
        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_never_committed() {
        let dir = scratch("broken");
        let path = dir.join("rec.gif");
        let mut writer = GifWriter::create(&path, 4, 4).unwrap();
        writer.add_frame(&frame(4, 4, 0), 4, 4, 10).unwrap();
        writer.broken = true; // as after a failed write_frame (e.g. disk full)
        assert!(matches!(
            writer.add_frame(&frame(4, 4, 1), 4, 4, 10),
            Err(GifWriteError::Broken)
        ));
        assert!(matches!(writer.finish(), Err(GifWriteError::Broken)));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stops_at_the_frame_limit() {
        let dir = scratch("limit");
        let mut writer = GifWriter::create(dir.join("rec.gif"), 1, 1).unwrap();
        writer.frames = MAX_GIF_FRAMES; // reach the limit without writing 3,600 frames
        assert!(matches!(
            writer.add_frame(&[0, 0, 0, 255], 1, 1, 10),
            Err(GifWriteError::TooManyFrames)
        ));
        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Times per-frame encoding at several NeuQuant speeds for `ENCODER_SPEED`.
    /// Run: `cargo test --release -p pixcap-core encoder_speed_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn encoder_speed_bench() {
        let (w, h) = (900, 506);
        let frames: Vec<_> = (0..20).map(|i| frame(w, h, i)).collect();
        let dir = scratch("bench");
        for speed in [1, 5, 10, 15, 20, 30] {
            let file = BufWriter::new(File::create(dir.join(format!("s{speed}.gif"))).unwrap());
            let mut encoder = gif::Encoder::new(file, w as u16, h as u16, &[]).unwrap();
            let start = std::time::Instant::now();
            for f in &frames {
                let mut pixels = f.clone();
                let frame = gif::Frame::from_rgba_speed(w as u16, h as u16, &mut pixels, speed);
                encoder.write_frame(&frame).unwrap();
            }
            drop(encoder);
            let per_frame = start.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64;
            let size = fs::metadata(dir.join(format!("s{speed}.gif")))
                .unwrap()
                .len()
                / frames.len() as u64;
            println!(
                "speed {speed:>2}: {per_frame:6.1} ms/frame, {:>4} KB/frame",
                size / 1024
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
