//! Non-destructive edit documents.
//!
//! A capture is saved as a flat image plus a `.pixcap.json` sidecar holding the
//! canvas settings and the annotation list. Re-opening the sidecar restores a
//! fully editable session — nothing is baked in until export.
//!
//! The schema lives here, in the shared core, so the macOS and Windows shells
//! read and write byte-identical documents.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Schema version written by this build.
pub const DOCUMENT_VERSION: u32 = 1;

/// Extension appended to the image's base name.
pub const SIDECAR_EXTENSION: &str = "pixcap.json";

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported document version {0}")]
    UnsupportedVersion(u32),
    #[error("document is larger than {MAX_DOCUMENT_BYTES} bytes")]
    TooLarge,
}

/// Largest edit file read. An estimate: a document with 1,000 annotations
/// is about 0.5 MB, so this leaves room while bounding a hostile file.
pub const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
/// Most annotations kept from a document (estimate; far above hand-made use).
const MAX_ITEMS: usize = 5_000;
/// Most freehand points kept per annotation (estimate: minutes of drawing at 60 Hz).
const MAX_POINTS_PER_ITEM: usize = 20_000;
/// Longest text kept per annotation, in characters (estimate).
const MAX_TEXT_CHARS: usize = 10_000;

fn default_stroke_width() -> f64 {
    3.0
}

fn default_font_size() -> f64 {
    24.0
}

fn default_blur_intensity() -> f64 {
    20.0
}

fn default_true() -> bool {
    true
}

fn default_color() -> String {
    "#FF3366".to_string()
}

fn default_arrow_head() -> String {
    "filled".to_string()
}

fn default_spotlight_dim() -> f64 {
    0.65
}

fn default_number() -> u32 {
    1
}

/// One annotation, in image-space points with a top-left origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationRecord {
    #[serde(default)]
    pub id: String,
    pub tool: String,
    #[serde(default)]
    pub start: [f64; 2],
    #[serde(default)]
    pub end: [f64; 2],
    #[serde(default)]
    pub points: Vec<[f64; 2]>,
    #[serde(default = "default_color")]
    pub color_hex: String,
    #[serde(default = "default_stroke_width")]
    pub stroke_width: f64,
    #[serde(default)]
    pub filled: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default = "default_font_size")]
    pub font_size: f64,
    #[serde(default = "default_number")]
    pub number: u32,
    #[serde(default)]
    pub blur_style: String,
    #[serde(default = "default_blur_intensity")]
    pub blur_intensity: f64,
    #[serde(default = "default_true")]
    pub curved_arrow: bool,
    #[serde(default = "default_arrow_head")]
    pub arrow_head: String,
    #[serde(default)]
    pub dashed: bool,
    #[serde(default = "default_spotlight_dim")]
    pub spotlight_dim: f64,
}

/// Canvas settings, mirroring the beautifier configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasRecord {
    /// `preset`, `wallpaper`, or `image`.
    #[serde(default = "CanvasRecord::default_kind")]
    pub background_kind: String,
    /// Preset id when `background_kind` is `preset`, otherwise a file path.
    #[serde(default)]
    pub background_value: String,
    #[serde(default = "CanvasRecord::default_padding")]
    pub padding: f64,
    #[serde(default = "CanvasRecord::default_corner_radius")]
    pub corner_radius: f64,
    #[serde(default = "CanvasRecord::default_shadow_blur")]
    pub shadow_blur: f64,
    #[serde(default = "CanvasRecord::default_shadow_opacity")]
    pub shadow_opacity: f64,
    #[serde(default)]
    pub background_blur: f64,
    #[serde(default = "CanvasRecord::default_frame_style")]
    pub frame_style: String,
    #[serde(default = "CanvasRecord::default_aspect_ratio")]
    pub aspect_ratio: String,
    #[serde(default)]
    pub frame_title: Option<String>,
    /// Non-destructive crop as `[x, y, width, height]` in source-image points.
    #[serde(default)]
    pub crop: Option<[f64; 4]>,
    /// Canvas texture: None, Grain, Paper, Linen, Dots, or Grid.
    #[serde(default = "CanvasRecord::default_texture")]
    pub texture: String,
    /// Film grain strength, 0.0 to 1.0.
    #[serde(default)]
    pub noise_intensity: f64,
    /// Perspective tilt in degrees about each axis.
    #[serde(default)]
    pub tilt_x: f64,
    #[serde(default)]
    pub tilt_y: f64,
}

impl CanvasRecord {
    fn default_kind() -> String {
        "preset".to_string()
    }
    fn default_padding() -> f64 {
        32.0
    }
    fn default_corner_radius() -> f64 {
        16.0
    }
    fn default_shadow_blur() -> f64 {
        24.0
    }
    fn default_shadow_opacity() -> f64 {
        0.35
    }
    fn default_frame_style() -> String {
        "macOS".to_string()
    }
    fn default_aspect_ratio() -> String {
        "Auto".to_string()
    }
    fn default_texture() -> String {
        "None".to_string()
    }
}

impl Default for CanvasRecord {
    fn default() -> Self {
        Self {
            background_kind: Self::default_kind(),
            background_value: "azure-mesh".to_string(),
            padding: Self::default_padding(),
            corner_radius: Self::default_corner_radius(),
            shadow_blur: Self::default_shadow_blur(),
            shadow_opacity: Self::default_shadow_opacity(),
            background_blur: 0.0,
            frame_style: Self::default_frame_style(),
            aspect_ratio: Self::default_aspect_ratio(),
            frame_title: None,
            crop: None,
            texture: Self::default_texture(),
            noise_intensity: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
        }
    }
}

/// A re-editable capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationDocument {
    #[serde(default)]
    pub version: u32,
    /// Path to the untouched source image.
    #[serde(default)]
    pub source_image: String,
    #[serde(default)]
    pub canvas: CanvasRecord,
    #[serde(default)]
    pub items: Vec<AnnotationRecord>,
}

impl Default for AnnotationDocument {
    fn default() -> Self {
        Self {
            version: DOCUMENT_VERSION,
            source_image: String::new(),
            canvas: CanvasRecord::default(),
            items: Vec::new(),
        }
    }
}

impl AnnotationDocument {
    /// Validates and normalises a document parsed from JSON.
    pub fn from_json(json: &str) -> Result<Self, DocumentError> {
        let mut document: AnnotationDocument = serde_json::from_str(json)?;

        if document.version == 0 {
            document.version = DOCUMENT_VERSION;
        }
        if document.version > DOCUMENT_VERSION {
            return Err(DocumentError::UnsupportedVersion(document.version));
        }

        document.clamp_to_editor_ranges();
        Ok(document)
    }

    pub fn to_json(&self) -> Result<String, DocumentError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Writes the document to `path`, creating parent directories as needed.
    pub fn write(&self, path: &str) -> Result<(), DocumentError> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        crate::fsutil::write_atomic(path, self.to_json()?.as_bytes())?;
        Ok(())
    }

    pub fn read(path: &str) -> Result<Self, DocumentError> {
        use std::io::Read;

        // `take` bounds the read even if the file grows after any size check.
        let mut json = String::new();
        fs::File::open(path)?
            .take(MAX_DOCUMENT_BYTES + 1)
            .read_to_string(&mut json)?;
        if json.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::TooLarge);
        }
        Self::from_json(&json)
    }

    /// Pulls every number into the range the editors can produce.
    ///
    /// A sidecar is a file anyone can hand the user (for example inside a
    /// zip), and these values size buffers and drive loops in both renderers:
    /// `padding: 1e6` asked for a canvas of about 16 TB and aborted the app.
    /// Ranges are the editor slider limits (macOS `EditorWindowView.swift` /
    /// `AnnotationToolsView.swift`, Windows `MainWindow.xaml`), taking the wider
    /// of the two where they differ, so no document either app saved changes.
    /// Non-finite values fall back to the field's default.
    fn clamp_to_editor_ranges(&mut self) {
        let defaults = CanvasRecord::default();
        let c = &mut self.canvas;
        c.padding = bounded(c.padding, 0.0, 160.0, defaults.padding);
        c.corner_radius = bounded(c.corner_radius, 0.0, 48.0, defaults.corner_radius);
        c.shadow_blur = bounded(c.shadow_blur, 0.0, 80.0, defaults.shadow_blur);
        c.shadow_opacity = bounded(c.shadow_opacity, 0.0, 1.0, defaults.shadow_opacity);
        c.background_blur = bounded(c.background_blur, 0.0, 40.0, 0.0);
        c.noise_intensity = bounded(c.noise_intensity, 0.0, 1.0, 0.0);
        c.tilt_x = bounded(c.tilt_x, -45.0, 45.0, 0.0);
        c.tilt_y = bounded(c.tilt_y, -45.0, 45.0, 0.0);
        if let Some([x, y, w, h]) = c.crop {
            // A crop lies inside a source image, which `render::open_image` caps per side.
            let max = crate::render::MAX_IMAGE_SIDE as f64;
            let valid = [x, y, w, h].iter().all(|v| v.is_finite() && (0.0..=max).contains(v));
            if !valid {
                c.crop = None;
            }
        }

        // Counts as well as values: millions of points would hang both renderers.
        self.items.truncate(MAX_ITEMS);
        for item in &mut self.items {
            item.points.truncate(MAX_POINTS_PER_ITEM);
            if let Some((cut, _)) = item.text.char_indices().nth(MAX_TEXT_CHARS) {
                item.text.truncate(cut);
            }
            // Slider maximums: stroke 16 (Windows), text 72 pt, blur 60.
            item.stroke_width = bounded(item.stroke_width, 0.0, 16.0, default_stroke_width());
            item.font_size = bounded(item.font_size, 1.0, 72.0, default_font_size());
            item.blur_intensity = bounded(item.blur_intensity, 0.0, 60.0, default_blur_intensity());
            item.spotlight_dim = bounded(item.spotlight_dim, 0.0, 1.0, default_spotlight_dim());
            // Annotations may sit outside the image, but never beyond the largest canvas.
            let limit = crate::render::MAX_IMAGE_SIDE as f64;
            for point in std::iter::once(&mut item.start)
                .chain(std::iter::once(&mut item.end))
                .chain(item.points.iter_mut())
            {
                for value in point.iter_mut() {
                    *value = bounded(*value, -limit, limit, 0.0);
                }
            }
        }
    }
}

/// `value` clamped to `min..=max`, or `fallback` when it is NaN or infinite.
fn bounded(value: f64, min: f64, max: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

/// A path field of an edit document, resolved against the document's folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPath {
    /// Absolute path. For a file beside the document it is canonical (checked
    /// on disk); anything else is only normalised as text and has not been
    /// touched, so the caller must ask the user before reading it.
    pub path: String,
    /// True when the file sits directly in the edit document's folder (not in
    /// a subfolder) and is not a symlink pointing elsewhere.
    pub in_document_folder: bool,
}

/// The value to store for `target` in the edit document at `sidecar` (ADR-0002).
///
/// Paths are stored relative to the document's folder, joined with `/` so a
/// document reads the same on macOS and Windows, which keeps the account name
/// and folder layout out of shared edit files. A target with no common root
/// (another drive on Windows), or input that is not a plain absolute path,
/// is stored as given.
pub fn relative_to_sidecar(sidecar: &str, target: &str) -> String {
    use std::path::Component;

    let (Some(base), target_path) = (Path::new(sidecar).parent(), Path::new(target)) else {
        return target.to_string();
    };
    if !base.is_absolute() || !target_path.is_absolute() {
        return target.to_string();
    }

    let base: Vec<Component> = base.components().collect();
    let goal: Vec<Component> = target_path.components().collect();
    if base.first() != goal.first() {
        return target.to_string();
    }

    let common = base.iter().zip(&goal).take_while(|(a, b)| a == b).count();
    let plain = |c: &Component| matches!(c, Component::Normal(_));
    if !base[common..].iter().all(plain) || !goal[common..].iter().all(plain) || common == goal.len() {
        return target.to_string();
    }

    let mut parts: Vec<&str> = vec![".."; base.len() - common];
    for component in &goal[common..] {
        match component.as_os_str().to_str() {
            Some(part) => parts.push(part),
            None => return target.to_string(),
        }
    }
    parts.join("/")
}

/// Resolves a stored path field of the edit document at `sidecar` (ADR-0002).
///
/// Relative values are taken from the document's folder; absolute values,
/// written by builds before ADR-0002, are used as they are. Returns `None`
/// when the value is empty or unusable, so the caller falls back to its default.
///
/// Guard before touch: a shared document must not make the app stat or
/// follow anything before the user agrees (a network automount path makes
/// the system contact a remote host, telling its owner the file was opened).
/// So the path is first normalised as text, and only a file directly beside
/// the document is examined on disk. A sibling that is a symlink is not
/// followed (it could point at a network share too) and counts as outside.
/// Anything outside is returned untouched with `in_document_folder: false`,
/// even if it does not exist, so the caller can ask before looking.
pub fn resolve_from_sidecar(sidecar: &str, stored: &str) -> Option<ResolvedPath> {
    if stored.is_empty() || stored.contains('\0') {
        return None;
    }

    let folder = normalize_lexically(Path::new(sidecar).parent()?)?;
    let candidate = Path::new(stored);
    let path = if candidate.is_absolute() {
        normalize_lexically(candidate)?
    } else {
        normalize_lexically(&folder.join(candidate))?
    };

    let outside = || {
        Some(ResolvedPath {
            path: path.to_str()?.to_string(),
            in_document_folder: false,
        })
    };
    if path.parent() != Some(folder.as_path()) {
        return outside();
    }
    // lstat reads the entry in the document's own folder without following it.
    if fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
        return outside();
    }

    let real = fs::canonicalize(&path).ok()?;
    let real_folder = fs::canonicalize(&folder).ok()?;
    Some(ResolvedPath {
        in_document_folder: real.parent() == Some(real_folder.as_path()),
        path: real.to_str()?.to_string(),
    })
}

/// `path` with `.` and `..` resolved as text, without touching the disk.
/// `None` for a relative path or one that climbs above its root.
fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.has_root() {
                    return None;
                }
            }
        }
    }
    Some(out)
}

/// Sidecar path for an image: `shot.png` becomes `shot.pixcap.json`.
///
/// The sidecar always sits in the image's folder. The Windows shell relies on
/// this: it stores `source_image` as the bare file name (`PixCapCore.cs`,
/// ADR-0002) instead of calling `relative_to_sidecar`. Moving sidecars to
/// another folder must update that writer too.
pub fn sidecar_path(image_path: &str) -> String {
    let path = PathBuf::from(image_path);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "capture".to_string());

    path.with_file_name(format!("{stem}.{SIDECAR_EXTENSION}"))
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn paths_are_stored_relative_to_the_edit_file() {
        let sidecar = "/Users/alex/Pictures/PixCap/shot.pixcap.json";
        assert_eq!(relative_to_sidecar(sidecar, "/Users/alex/Pictures/PixCap/shot.png"), "shot.png");
        assert_eq!(
            relative_to_sidecar(sidecar, "/Users/alex/Pictures/Wallpapers/sea.jpg"),
            "../Wallpapers/sea.jpg"
        );
        assert_eq!(relative_to_sidecar(sidecar, "/Library/Desktop/a.heic"), "../../../../Library/Desktop/a.heic");
        // Anything that is not a plain absolute path is kept as given.
        assert_eq!(relative_to_sidecar(sidecar, "relative.png"), "relative.png");
        assert_eq!(relative_to_sidecar(sidecar, "/Users/alex/../x.png"), "/Users/alex/../x.png");
        assert_eq!(relative_to_sidecar(sidecar, ""), "");
    }

    #[cfg(windows)]
    #[test]
    fn another_drive_stays_absolute() {
        assert_eq!(relative_to_sidecar(r"C:\shots\a.pixcap.json", r"D:\bg.png"), r"D:\bg.png");
    }

    #[test]
    fn oversized_documents_are_bounded() {
        let mut document = AnnotationDocument::default();
        let small: AnnotationRecord = serde_json::from_str(r#"{"id":"a","tool":"pen"}"#).unwrap();
        let mut big = small.clone();
        big.points = vec![[1.0, 1.0]; MAX_POINTS_PER_ITEM + 10];
        big.text = "é".repeat(MAX_TEXT_CHARS + 10);
        document.items = vec![small; MAX_ITEMS + 3];
        document.items[0] = big;

        let loaded = AnnotationDocument::from_json(&document.to_json().unwrap()).unwrap();
        assert_eq!(loaded.items.len(), MAX_ITEMS);
        assert_eq!(loaded.items[0].points.len(), MAX_POINTS_PER_ITEM);
        assert_eq!(loaded.items[0].text.chars().count(), MAX_TEXT_CHARS);

        let path = std::env::temp_dir().join("pixcap-oversized.pixcap.json");
        fs::write(&path, vec![b' '; (MAX_DOCUMENT_BYTES + 1) as usize]).unwrap();
        assert!(matches!(
            AnnotationDocument::read(path.to_str().unwrap()),
            Err(DocumentError::TooLarge)
        ));
        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn resolving_checks_the_real_location() {
        let root = std::env::temp_dir().join("pixcap-resolve-test");
        let _ = fs::remove_dir_all(&root);
        let folder = root.join("captures");
        let outside = root.join("elsewhere");
        fs::create_dir_all(&folder).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(folder.join("bg.png"), b"x").unwrap();
        fs::write(outside.join("secret.png"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.png"), folder.join("link.png")).unwrap();

        let sidecar = folder.join("shot.pixcap.json");
        let sidecar = sidecar.to_str().unwrap();

        let inside = resolve_from_sidecar(sidecar, "bg.png").unwrap();
        assert!(inside.in_document_folder);

        let escaped = resolve_from_sidecar(sidecar, "../elsewhere/secret.png").unwrap();
        assert!(!escaped.in_document_folder);

        // Only direct siblings are trusted; a subfolder of the document's folder is not.
        fs::create_dir_all(folder.join("old")).unwrap();
        fs::write(folder.join("old/passport.jpg"), b"x").unwrap();
        assert!(!resolve_from_sidecar(sidecar, "old/passport.jpg").unwrap().in_document_folder);

        // Paths outside the folder are not touched, so one that does not exist still resolves as text.
        let untouched = resolve_from_sidecar(sidecar, "/pixcap-no-such-root/share/x.png").unwrap();
        assert_eq!(untouched.path, "/pixcap-no-such-root/share/x.png");
        assert!(!untouched.in_document_folder);
        assert!(resolve_from_sidecar(sidecar, "../../../../../../../../../../../../x.png").is_none());

        let via_link = resolve_from_sidecar(sidecar, "link.png").unwrap();
        assert!(!via_link.in_document_folder, "a symlink must not count as inside");
        assert!(via_link.path.ends_with("captures/link.png"), "the link is not followed");

        let legacy = outside.join("secret.png");
        let absolute = resolve_from_sidecar(sidecar, legacy.to_str().unwrap()).unwrap();
        assert!(!absolute.in_document_folder);

        assert!(resolve_from_sidecar(sidecar, "missing.png").is_none(), "a missing sibling falls back");
        assert!(resolve_from_sidecar(sidecar, "").is_none());

        // What is written resolves back to the same file.
        let stored = relative_to_sidecar(sidecar, outside.join("secret.png").to_str().unwrap());
        assert_eq!(resolve_from_sidecar(sidecar, &stored).unwrap().path, absolute.path);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hostile_numbers_are_pulled_into_editor_ranges() {
        let json = r#"{
            "canvas": { "padding": 1e300, "shadow_blur": -5, "noise_intensity": 9,
                        "tilt_x": 1000, "crop": [0, 0, 1e12, 10] },
            "items": [{ "tool": "text", "font_size": 1e9, "stroke_width": 1e6,
                        "start": [1e300, -1e300], "points": [[1e20, 3]] }]
        }"#;
        let document = AnnotationDocument::from_json(json).unwrap();
        assert_eq!(document.canvas.padding, 160.0);
        assert_eq!(document.canvas.shadow_blur, 0.0);
        assert_eq!(document.canvas.noise_intensity, 1.0);
        assert_eq!(document.canvas.tilt_x, 45.0);
        assert!(document.canvas.crop.is_none());
        let item = &document.items[0];
        assert_eq!(item.font_size, 72.0);
        assert_eq!(item.stroke_width, 16.0);
        assert_eq!(item.start, [32_768.0, -32_768.0]);
        assert_eq!(item.points[0], [32_768.0, 3.0]);
    }

    #[test]
    fn non_finite_numbers_fall_back_to_defaults() {
        assert_eq!(bounded(f64::NAN, 0.0, 10.0, 4.0), 4.0);
        assert_eq!(bounded(f64::INFINITY, 0.0, 10.0, 4.0), 4.0);
        assert_eq!(bounded(5.0, 0.0, 10.0, 4.0), 5.0);
    }

    fn sample() -> AnnotationDocument {
        AnnotationDocument {
            version: DOCUMENT_VERSION,
            source_image: "/tmp/shot.png".to_string(),
            canvas: CanvasRecord {
                background_value: "amethyst".to_string(),
                padding: 48.0,
                frame_title: Some("Terminal".to_string()),
                ..CanvasRecord::default()
            },
            items: vec![AnnotationRecord {
                id: "abc".to_string(),
                tool: "arrow".to_string(),
                start: [10.0, 20.0],
                end: [110.0, 90.0],
                points: vec![],
                color_hex: "#00E5FF".to_string(),
                stroke_width: 5.0,
                filled: false,
                text: String::new(),
                font_size: 24.0,
                number: 1,
                blur_style: "gaussian".to_string(),
                blur_intensity: 20.0,
                curved_arrow: true,
                arrow_head: "filled".to_string(),
                dashed: false,
                spotlight_dim: 0.65,
            }],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let document = sample();
        let json = document.to_json().unwrap();
        let parsed = AnnotationDocument::from_json(&json).unwrap();

        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].tool, "arrow");
        assert_eq!(parsed.canvas.background_value, "amethyst");
        assert_eq!(parsed.canvas.frame_title.as_deref(), Some("Terminal"));
    }

    #[test]
    fn canvas_extensions_default_for_older_documents() {
        // A sidecar written before textures existed must still load.
        let json = r#"{"canvas":{"background_value":"amethyst","padding":40.0}}"#;
        let parsed = AnnotationDocument::from_json(json).unwrap();

        assert_eq!(parsed.canvas.texture, "None");
        assert_eq!(parsed.canvas.noise_intensity, 0.0);
        assert_eq!(parsed.canvas.tilt_x, 0.0);
        assert_eq!(parsed.canvas.background_value, "amethyst");
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let json = r#"{"items":[{"tool":"rectangle"}]}"#;
        let parsed = AnnotationDocument::from_json(json).unwrap();

        assert_eq!(parsed.version, DOCUMENT_VERSION);
        assert_eq!(parsed.items[0].stroke_width, 3.0);
        assert_eq!(parsed.items[0].color_hex, "#FF3366");
        assert_eq!(parsed.canvas.frame_style, "macOS");
    }

    #[test]
    fn future_versions_are_rejected() {
        let json = format!(r#"{{"version":{},"items":[]}}"#, DOCUMENT_VERSION + 1);
        assert!(matches!(
            AnnotationDocument::from_json(&json),
            Err(DocumentError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn writes_and_reads_from_disk() {
        let dir = std::env::temp_dir().join("pixcap-document-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("shot.pixcap.json");
        let path_str = path.to_string_lossy().to_string();

        sample().write(&path_str).unwrap();
        let loaded = AnnotationDocument::read(&path_str).unwrap();
        assert_eq!(loaded.source_image, "/tmp/shot.png");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derives_sidecar_paths() {
        // Asserted through `Path` rather than against a literal string: the
        // function builds the result with `std::path`, so on Windows a POSIX
        // input yields "/a/b\\shot.pixcap.json". Those mixed separators are
        // valid there, and real Windows inputs are native paths anyway, so the
        // contract worth testing is "same directory, correct file name", not
        // the byte sequence.
        let nested = sidecar_path("/a/b/shot.png");
        let nested_path = Path::new(&nested);
        assert_eq!(
            nested_path.file_name().unwrap().to_string_lossy(),
            "shot.pixcap.json"
        );
        assert_eq!(nested_path.parent(), Path::new("/a/b/shot.png").parent());

        // A bare file name has no directory to preserve.
        assert_eq!(sidecar_path("shot.jpg"), "shot.pixcap.json");

        // The extension is replaced, not appended.
        assert!(!sidecar_path("/tmp/a.png").contains(".png"));
    }
}
