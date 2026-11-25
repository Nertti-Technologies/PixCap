use interprocess::local_socket::{
    prelude::*, GenericFilePath, GenericNamespaced, ListenerOptions, Stream,
};
use pixcap_core::{BackgroundFill, CanvasLayout};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IPC Server failure: {0}")]
    ServerFailed(String),
}

/// Request payloads sent from IDE extensions or CLI to PixCap Core
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", content = "payload")]
pub enum IpcMessageRequest {
    /// Render code selection from VS Code / JetBrains
    RenderSnippet {
        code: String,
        language: String,
        theme: Option<String>,
        layout: Option<CanvasLayout>,
        background: Option<BackgroundFill>,
    },
    /// Trigger OS level screen capture
    TriggerScreenCapture {
        mode: String, // "region", "window", "display"
    },
    /// Ping status check
    Ping,
}

/// Response payloads sent from PixCap Core back to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "data")]
pub enum IpcMessageResponse {
    Success {
        svg_content: Option<String>,
        message: String,
    },
    Pong {
        version: String,
    },
    Error {
        error_message: String,
    },
}

/// Endpoint used when none is configured.
///
/// The transport is a Unix domain socket on macOS and a named pipe on
/// Windows. `interprocess` provides both behind one API: filesystem-path names
/// map to Unix sockets, namespaced names map to `\\.\pipe\…` on Windows.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/pixcap.sock";

/// Namespaced name used where the platform supports one (Windows, Linux).
pub const DEFAULT_SOCKET_NAME: &str = "pixcap.sock";

/// Kept for callers that referenced the old macOS-only constant.
#[deprecated(note = "use DEFAULT_SOCKET_PATH or IpcServer::default_endpoint()")]
pub const DEFAULT_MACOS_SOCKET_PATH: &str = DEFAULT_SOCKET_PATH;

/// Largest request line the server reads, in bytes.
///
/// An estimate: 1 MiB holds a snippet far beyond `MAX_CODE_LINES` plus its
/// JSON envelope. Without a cap, a client that never sends a newline made the
/// server buffer without limit.
pub const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Most lines a snippet may have. An estimate: a screenshot of code stops
/// being legible long before this, and highlighting cost grows with length.
pub const MAX_CODE_LINES: usize = 2_000;

/// Handles IPC connection processing
pub struct IpcServer;

impl IpcServer {
    /// The endpoint this platform should use by default.
    pub fn default_endpoint() -> String {
        if GenericNamespaced::is_supported() {
            DEFAULT_SOCKET_NAME.to_string()
        } else {
            DEFAULT_SOCKET_PATH.to_string()
        }
    }

    /// Resolves an endpoint string into a platform-appropriate socket name.
    fn socket_name(endpoint: &str) -> Result<interprocess::local_socket::Name<'_>, IpcError> {
        // A namespaced name on Windows becomes a named pipe; elsewhere the
        // endpoint is a filesystem path.
        if GenericNamespaced::is_supported() && !endpoint.contains(std::path::MAIN_SEPARATOR) {
            Ok(endpoint.to_ns_name::<GenericNamespaced>()?)
        } else {
            Ok(endpoint.to_fs_name::<GenericFilePath>()?)
        }
    }

    pub fn process_request(request: IpcMessageRequest) -> IpcMessageResponse {
        match request {
            IpcMessageRequest::Ping => IpcMessageResponse::Pong {
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            IpcMessageRequest::RenderSnippet {
                code,
                language,
                theme,
                layout,
                background,
            } => {
                if code.lines().count() > MAX_CODE_LINES {
                    return IpcMessageResponse::Error {
                        error_message: format!("snippets are limited to {MAX_CODE_LINES} lines"),
                    };
                }

                let renderer = pixcap_core::CodeRenderer::default();
                let mut options = pixcap_core::RenderOptions::default();
                options.code = code;
                options.language = language;

                if let Some(t) = theme {
                    options.syntax_theme = t;
                }
                if let Some(l) = layout {
                    options.layout = l;
                }
                if let Some(b) = background {
                    options.background = b;
                }

                match renderer.render_svg(&options) {
                    Ok(svg) => IpcMessageResponse::Success {
                        svg_content: Some(svg),
                        message: "Snippet rendered successfully".to_string(),
                    },
                    Err(e) => IpcMessageResponse::Error {
                        error_message: e.to_string(),
                    },
                }
            }
            IpcMessageRequest::TriggerScreenCapture { mode } => IpcMessageResponse::Success {
                svg_content: None,
                message: format!("Triggered OS capture mode: {}", mode),
            },
        }
    }

    /// Binds a listener for `endpoint`, working on macOS and Windows alike.
    pub fn listen(endpoint: &str) -> Result<interprocess::local_socket::Listener, IpcError> {
        // A crashed process leaves the socket file behind; Windows named pipes
        // clean themselves up, so this only applies to path-based endpoints.
        let path_based = !GenericNamespaced::is_supported() || endpoint.contains(std::path::MAIN_SEPARATOR);
        if path_based {
            Self::remove_stale_socket(std::path::Path::new(endpoint));
        }

        let name = Self::socket_name(endpoint)?;
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .map_err(|e| IpcError::ServerFailed(format!("could not bind the IPC endpoint: {e}")))?;

        // Owner-only, whatever the process umask: other local users must not
        // be able to send requests.
        #[cfg(unix)]
        if path_based {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(listener)
    }

    /// Deletes a socket left by a crashed run, and nothing else.
    ///
    /// Uses `symlink_metadata` so a symlink planted at the path is never
    /// followed, and only removes the entry when it really is a socket.
    fn remove_stale_socket(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                if metadata.file_type().is_socket() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    /// Reads one request, writes one response.
    ///
    /// Messages are newline-delimited rather than read-to-EOF: named pipes have
    /// no half-close, so waiting for EOF would deadlock on Windows.
    pub fn handle_stream<S: Read + Write>(stream: S) -> Result<(), IpcError> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        (&mut reader).take(MAX_REQUEST_BYTES).read_line(&mut line)?;
        if !line.ends_with('\n') && line.len() as u64 >= MAX_REQUEST_BYTES {
            return Err(IpcError::ServerFailed("request too large".to_string()));
        }

        let request: IpcMessageRequest = serde_json::from_str(line.trim())?;
        let response = Self::process_request(request);

        let mut response_json = serde_json::to_string(&response)?;
        response_json.push('\n');

        let stream = reader.get_mut();
        stream.write_all(response_json.as_bytes())?;
        stream.flush()?;
        Ok(())
    }

    /// Accepts connections until the listener is dropped.
    pub fn serve_blocking(endpoint: &str) -> Result<(), IpcError> {
        let listener = Self::listen(endpoint)?;
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    if let Err(error) = Self::handle_stream(stream) {
                        eprintln!("pixcap-ipc: request failed: {error}");
                    }
                }
                Err(error) => eprintln!("pixcap-ipc: accept failed: {error}"),
            }
        }
        Ok(())
    }
}

/// Client side, used by the CLI and IDE extensions.
pub struct IpcClient;

impl IpcClient {
    /// Sends one request and waits for the response.
    pub fn send(endpoint: &str, request: &IpcMessageRequest) -> Result<IpcMessageResponse, IpcError> {
        let name = IpcServer::socket_name(endpoint)?;
        let stream = Stream::connect(name)?;

        let mut payload = serde_json::to_string(request)?;
        payload.push('\n');

        let mut reader = BufReader::new(stream);
        reader.get_mut().write_all(payload.as_bytes())?;
        reader.get_mut().flush()?;

        let mut line = String::new();
        reader.read_line(&mut line)?;
        Ok(serde_json::from_str(line.trim())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory duplex stream: reads from `input`, collects writes in `output`.
    struct Duplex {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn oversized_requests_are_refused_without_buffering_them() {
        let input = vec![b'x'; (MAX_REQUEST_BYTES as usize) * 2];
        let stream = Duplex { input: std::io::Cursor::new(input), output: Vec::new() };
        assert!(matches!(IpcServer::handle_stream(stream), Err(IpcError::ServerFailed(_))));
    }

    #[test]
    fn overlong_snippets_are_refused() {
        let response = IpcServer::process_request(IpcMessageRequest::RenderSnippet {
            code: "x\n".repeat(MAX_CODE_LINES + 1),
            language: "rust".to_string(),
            theme: None,
            layout: None,
            background: None,
        });
        assert!(matches!(response, IpcMessageResponse::Error { .. }));
    }

    #[test]
    fn round_trips_over_the_local_socket() {
        // Exercises the actual transport — Unix socket here, named pipe on
        // Windows — rather than only the message handling.
        let endpoint = if GenericNamespaced::is_supported() {
            format!("pixcap-test-{}.sock", std::process::id())
        } else {
            format!("/tmp/pixcap-test-{}.sock", std::process::id())
        };

        let listener = IpcServer::listen(&endpoint).expect("listener binds");

        let server = std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                let _ = IpcServer::handle_stream(stream);
            }
        });

        let response = IpcClient::send(&endpoint, &IpcMessageRequest::Ping).expect("ping succeeds");
        match response {
            IpcMessageResponse::Pong { version } => assert_eq!(version, env!("CARGO_PKG_VERSION")),
            other => panic!("expected Pong, got {other:?}"),
        }

        server.join().unwrap();
        let _ = std::fs::remove_file(&endpoint);
    }

    #[test]
    fn renders_a_snippet_over_ipc() {
        let response = IpcServer::process_request(IpcMessageRequest::RenderSnippet {
            code: "fn main() {}".to_string(),
            language: "rs".to_string(),
            theme: None,
            layout: None,
            background: None,
        });

        match response {
            IpcMessageResponse::Success { svg_content, .. } => {
                assert!(svg_content.unwrap_or_default().contains("<svg"));
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn test_ipc_ping_pong() {
        let req = IpcMessageRequest::Ping;
        let res = IpcServer::process_request(req);
        match res {
            IpcMessageResponse::Pong { version } => assert_eq!(version, env!("CARGO_PKG_VERSION")),
            _ => panic!("Expected Pong response"),
        }
    }
}
