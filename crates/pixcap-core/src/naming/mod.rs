use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureContext {
    pub mode: String,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub width: u32,
    pub height: u32,
    pub counter: u32,
}

/// Resolves a given pattern string using the provided context.
/// 
/// Supported variables:
/// - `{date}`: YYYY-MM-DD
/// - `{time}`: HH-MM-SS
/// - `{datetime}`: YYYY-MM-DD_HH-MM-SS
/// - `{timestamp}`: UNIX timestamp
/// - `{mode}`: Capture mode
/// - `{counter}`: Counter
/// - `{app}`: Application name (or 'App' if not present)
/// - `{title}`: Window title (or 'Window' if not present)
/// - `{width}`: Width
/// - `{height}`: Height
///
/// Substitution is a single pass, so a window titled `{width}` stays literal.
/// `{app}`, `{title}` and `{mode}` are cleaned with [`file_name_safe`], because
/// window titles come from web pages and documents: a title such as `../x` or
/// `a/b` would otherwise put a path into the file name.
pub fn resolve_filename(pattern: &str, context: &CaptureContext) -> String {
    let now: DateTime<Utc> = Utc::now();

    let mut result = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after = &rest[open..];
        let Some(close) = after.find('}') else {
            result.push_str(after);
            return result;
        };

        let token = &after[1..close];
        let value = match token {
            "date" => Some(now.format("%Y-%m-%d").to_string()),
            "time" => Some(now.format("%H-%M-%S").to_string()),
            "datetime" => Some(now.format("%Y-%m-%d_%H-%M-%S").to_string()),
            "timestamp" => Some(now.timestamp().to_string()),
            "mode" => Some(file_name_safe(&context.mode)),
            "counter" => Some(context.counter.to_string()),
            "app" => Some(file_name_safe(context.app_name.as_deref().unwrap_or("App"))),
            "title" => Some(file_name_safe(context.window_title.as_deref().unwrap_or("Window"))),
            "width" => Some(context.width.to_string()),
            "height" => Some(context.height.to_string()),
            _ => None,
        };

        match value {
            Some(value) => result.push_str(&value),
            None => result.push_str(&after[..=close]), // unknown token stays as typed
        }
        rest = &after[close + 1..];
    }
    result.push_str(rest);
    result
}

/// Longest value one placeholder may contribute, in characters.
///
/// An estimate: file names are capped at 255 bytes on APFS and NTFS, and a
/// pattern can hold several placeholders plus a date and an extension.
const MAX_VALUE_CHARS: usize = 80;

/// Makes text from outside PixCap safe to embed in a file name.
///
/// Path separators, `:` (a separator in macOS Finder), characters Windows
/// forbids, and control characters become `-`; leading dots and spaces are
/// dropped so the result can be neither hidden nor `.`/`..`.
pub fn file_name_safe(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .take(MAX_VALUE_CHARS)
        .collect();

    let trimmed = cleaned.trim_start_matches(['.', ' ']).trim_end();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn default_pattern() -> &'static str {
    "PixCap_{date}_{time}_{counter}"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_filename() {
        let ctx = CaptureContext {
            mode: "area".to_string(),
            app_name: Some("Safari".to_string()),
            window_title: Some("Github".to_string()),
            width: 1920,
            height: 1080,
            counter: 42,
        };
        
        let pattern = "Test_{app}_{mode}_{counter}_{width}x{height}";
        let resolved = resolve_filename(pattern, &ctx);
        assert_eq!(resolved, "Test_Safari_area_42_1920x1080");
    }

    fn context_with_title(title: &str) -> CaptureContext {
        CaptureContext {
            mode: "window".to_string(),
            app_name: Some("App".to_string()),
            window_title: Some(title.to_string()),
            width: 10,
            height: 20,
            counter: 1,
        }
    }

    #[test]
    fn titles_cannot_inject_paths() {
        let resolved = resolve_filename("{title}", &context_with_title("../../etc/passwd"));
        assert!(!resolved.contains('/'), "got {resolved}");
        assert!(!resolved.starts_with('.'), "got {resolved}");

        let resolved = resolve_filename("{title}", &context_with_title("C:\\x\ny"));
        assert_eq!(resolved, "C--x-y");

        assert_eq!(resolve_filename("{title}", &context_with_title("..")), "Untitled");
    }

    #[test]
    fn substitution_is_single_pass() {
        let resolved = resolve_filename("{title}_{width}", &context_with_title("{height}"));
        assert_eq!(resolved, "{height}_10");
    }

    #[test]
    fn unknown_and_unclosed_tokens_stay_literal() {
        let ctx = context_with_title("T");
        assert_eq!(resolve_filename("a{nope}b{counter}", &ctx), "a{nope}b1");
        assert_eq!(resolve_filename("a{counter", &ctx), "a{counter");
    }

    #[test]
    fn long_titles_are_capped() {
        let resolved = resolve_filename("{title}", &context_with_title(&"x".repeat(500)));
        assert_eq!(resolved.chars().count(), MAX_VALUE_CHARS);
    }
}
