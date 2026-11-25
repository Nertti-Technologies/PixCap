import AppKit
import Foundation

/// Decides whether an edit document may use a background picture (ADR-0002).
///
/// An edit file can come from someone else, and its background path could
/// name any picture on this Mac, which would then be drawn into the export.
/// A picture is used without asking only when it sits directly in the edit
/// file's own folder or the user picked it on this Mac before; anything else
/// needs a yes.
///
/// Paths are compared as normalised text, never looked up on disk: checking
/// a path from a shared file before the user agrees would already touch it
/// (a network path makes the system contact that host). The core resolves
/// document paths the same way (`resolve_from_sidecar`).
@available(macOS 14.0, *)
enum BackgroundTrust {

    /// Most pictures remembered; the oldest is dropped first. An estimate
    /// (ADR-0002): a person rarely uses more than a few dozen backgrounds.
    static let limit = 200

    /// Where the list is kept. The self-test swaps in a scratch suite.
    static var defaults: UserDefaults = .standard

    /// Records a picture the user chose, so their own edits reopen without a prompt.
    static func remember(_ url: URL) {
        let path = normalizedPath(url.path)
        var paths = stored.filter { $0 != path }
        paths.append(path)
        defaults.set(Array(paths.suffix(limit)), forKey: SettingsKey.trustedBackgroundImages)
    }

    /// True when `path` was picked or approved on this Mac before.
    static func isTrusted(_ path: String) -> Bool {
        stored.contains(normalizedPath(path))
    }

    /// Asks before using a picture from outside the edit file's folder.
    ///
    /// "Use Default Background" is the first button, so Return picks the safe choice.
    @MainActor
    static func askUser(_ url: URL) -> Bool {
        let alert = NSAlert()
        alert.messageText = "Use a background picture from another folder?"
        alert.informativeText = """
            This edit file uses the picture “\(url.lastPathComponent)” from outside its own folder:

            \((url.path as NSString).abbreviatingWithTildeInPath)

            Use it only if you trust where this file came from, because the picture becomes \
            part of anything you export.
            """
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Use Default Background")
        alert.addButton(withTitle: "Use Picture")
        return alert.runModal() == .alertSecondButtonReturn
    }

    /// `.` and `..` resolved as text, without touching the disk.
    static func normalizedPath(_ path: String) -> String {
        URL(fileURLWithPath: path).standardizedFileURL.path
    }

    private static var stored: [String] {
        defaults.stringArray(forKey: SettingsKey.trustedBackgroundImages) ?? []
    }
}
