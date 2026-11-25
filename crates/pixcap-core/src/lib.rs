pub mod annotations;
pub mod canvas;
pub mod document;
pub mod export;
pub mod fsutil;
pub mod history;
pub mod naming;
pub mod redaction;
pub mod render;
pub mod stitch;
pub mod syntax;
pub mod themes;

pub use annotations::{Annotation, AnnotationLayer, ArrowHeadStyle, BlurStyle};
pub use document::{
    relative_to_sidecar, resolve_from_sidecar, sidecar_path, AnnotationDocument, AnnotationRecord,
    CanvasRecord, DocumentError, ResolvedPath,
};
pub use render::{layout as render_layout, render_document, render_to_pixmap, Layout, RenderError};
pub use stitch::{stitch_files, stitch_vertical, StitchError, StitchOptions};
pub use canvas::{CanvasLayout, ContainerSizing, DropShadowConfig, PaddingConfig, WindowFrameStyle};
pub use fsutil::write_atomic;
pub use export::{CodeRenderer, ExportError, RenderOptions};
pub use history::{HistoryDb, ScreenshotRecord};
pub use naming::{default_pattern, resolve_filename, CaptureContext};
pub use redaction::{RedactionEngine, RedactionType};
pub use syntax::{HighlightedLine, SyntaxError, SyntaxHighlighter};
pub use themes::{BackgroundFill, BackgroundPreset, ThemePresets};
