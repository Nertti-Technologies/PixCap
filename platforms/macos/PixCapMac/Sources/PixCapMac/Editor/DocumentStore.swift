import Foundation
import AppKit

/// Reads and writes non-destructive edit documents.
///
/// The exported PNG is flat, but a `.pixcap.json` sidecar keeps the canvas
/// settings and every annotation as data, so a capture can be reopened and
/// re-edited later. The schema is owned by the Rust core, which validates every
/// document on the way in and out.
public enum DocumentStore {

    // MARK: - Wire format (mirrors pixcap_core::document)

    private struct AnnotationRecordDTO: Codable {
        var id: String
        var tool: String
        var start: [Double]
        var end: [Double]
        var points: [[Double]]
        var color_hex: String
        var stroke_width: Double
        var filled: Bool
        var text: String
        var font_size: Double
        var number: Int
        var blur_style: String
        var blur_intensity: Double
        var curved_arrow: Bool
        var arrow_head: String
        var dashed: Bool
        var spotlight_dim: Double
    }

    private struct CanvasRecordDTO: Codable {
        var background_kind: String
        var background_value: String
        var padding: Double
        var corner_radius: Double
        var shadow_blur: Double
        var shadow_opacity: Double
        var background_blur: Double
        var frame_style: String
        var aspect_ratio: String
        var frame_title: String?
        var crop: [Double]?
    }

    private struct DocumentDTO: Codable {
        var version: Int
        var source_image: String
        var canvas: CanvasRecordDTO
        var items: [AnnotationRecordDTO]
    }

    /// A loaded editing session.
    public struct LoadedDocument {
        public let sourceImageURL: URL
        public let config: BeautifyConfig
        public let items: [AnnotationItem]
    }

    // MARK: - Public API

    public static func sidecarURL(forImage url: URL) -> URL {
        URL(fileURLWithPath: PixCapBridge.shared.sidecarPath(forImage: url.path))
    }

    /// Writes the sidecar next to `sourceImageURL`.
    @discardableResult
    public static func save(
        sourceImageURL: URL,
        config: BeautifyConfig,
        items: [AnnotationItem]
    ) -> URL? {
        // Paths are stored relative to the edit file, so a shared file does
        // not reveal the account name or folder layout (ADR-0002).
        let target = sidecarURL(forImage: sourceImageURL)
        let document = DocumentDTO(
            version: 1,
            source_image: PixCapBridge.shared.documentRelativePath(sidecar: target.path, target: sourceImageURL.path),
            canvas: encode(config, sidecar: target),
            items: items.map(encode)
        )

        guard let data = try? JSONEncoder().encode(document),
              let json = String(data: data, encoding: .utf8) else {
            return nil
        }

        guard PixCapBridge.shared.writeDocument(json: json, to: target.path) else {
            NSLog("PixCap: the core rejected an edit document")
            return nil
        }
        return target
    }

    /// Asked before using a background picture from outside the edit file's
    /// folder that this Mac has not approved; true means use it.
    public typealias BackgroundConfirmation = (URL) -> Bool

    /// Loads a sidecar, if one exists for `imageURL`.
    @MainActor
    public static func load(forImage imageURL: URL) -> LoadedDocument? {
        load(sidecar: sidecarURL(forImage: imageURL), confirmBackground: BackgroundTrust.askUser)
    }

    public static func load(sidecar url: URL, confirmBackground: BackgroundConfirmation) -> LoadedDocument? {
        guard let json = PixCapBridge.shared.readDocument(at: url.path),
              let data = json.data(using: .utf8),
              let document = try? JSONDecoder().decode(DocumentDTO.self, from: data) else {
            return nil
        }

        // Relative values (ADR-0002) are taken from the edit file's folder;
        // absolute ones from older builds are used as they are.
        let folder = url.deletingLastPathComponent()
        return LoadedDocument(
            sourceImageURL: URL(fileURLWithPath: document.source_image, relativeTo: folder).absoluteURL,
            config: decode(document.canvas, sidecar: url, confirmBackground: confirmBackground),
            items: document.items.map(decode)
        )
    }

    public static func hasSidecar(forImage url: URL) -> Bool {
        FileManager.default.fileExists(atPath: sidecarURL(forImage: url).path)
    }

    // MARK: - Encoding

    private static func encode(_ config: BeautifyConfig, sidecar: URL) -> CanvasRecordDTO {
        let kind: String
        let value: String

        switch config.background {
        case .preset(let id):
            kind = "preset"
            value = id
        case .desktopWallpaper:
            kind = "wallpaper"
            value = ""
        case .customImage(let url):
            kind = "image"
            value = PixCapBridge.shared.documentRelativePath(sidecar: sidecar.path, target: url.path)
        }

        return CanvasRecordDTO(
            background_kind: kind,
            background_value: value,
            padding: Double(config.padding),
            corner_radius: Double(config.cornerRadius),
            shadow_blur: Double(config.shadowBlur),
            shadow_opacity: Double(config.shadowOpacity),
            background_blur: Double(config.backgroundBlur),
            frame_style: config.frameStyle.rawValue,
            aspect_ratio: config.aspectRatio.rawValue,
            frame_title: config.frameTitle,
            crop: config.crop.map { [Double($0.minX), Double($0.minY), Double($0.width), Double($0.height)] }
        )
    }

    private static func encode(_ item: AnnotationItem) -> AnnotationRecordDTO {
        AnnotationRecordDTO(
            id: item.id.uuidString,
            tool: item.tool.rawValue,
            start: [Double(item.start.x), Double(item.start.y)],
            end: [Double(item.end.x), Double(item.end.y)],
            points: item.points.map { [Double($0.x), Double($0.y)] },
            color_hex: item.colorHex,
            stroke_width: Double(item.strokeWidth),
            filled: item.filled,
            text: item.text,
            font_size: Double(item.fontSize),
            number: item.number,
            blur_style: item.blurStyle == .pixelate ? "pixelate" : "gaussian",
            blur_intensity: Double(item.blurIntensity),
            curved_arrow: item.curvedArrow,
            arrow_head: item.arrowHead.rawValue,
            dashed: item.dashed,
            spotlight_dim: Double(item.spotlightDim)
        )
    }

    // MARK: - Decoding

    /// Preset used when a document names none, or its picture cannot be used.
    private static let defaultPresetID = "azure-mesh"

    private static func decode(
        _ record: CanvasRecordDTO,
        sidecar: URL,
        confirmBackground: BackgroundConfirmation
    ) -> BeautifyConfig {
        let background: BackgroundSelection
        switch record.background_kind {
        case "wallpaper":
            background = .desktopWallpaper
        case "image":
            background = usableBackground(record.background_value, sidecar: sidecar, confirm: confirmBackground)
                .map { .customImage(url: $0) } ?? .preset(id: defaultPresetID)
        default:
            background = .preset(id: record.background_value.isEmpty ? defaultPresetID : record.background_value)
        }

        var crop: CGRect?
        if let values = record.crop, values.count == 4 {
            crop = CGRect(x: values[0], y: values[1], width: values[2], height: values[3])
        }

        return BeautifyConfig(
            background: background,
            padding: CGFloat(record.padding),
            cornerRadius: CGFloat(record.corner_radius),
            shadowBlur: CGFloat(record.shadow_blur),
            shadowOpacity: CGFloat(record.shadow_opacity),
            frameStyle: WindowFrameStyle(rawValue: record.frame_style) ?? .macOS,
            aspectRatio: AspectRatioPreset(rawValue: record.aspect_ratio) ?? .auto,
            frameTitle: record.frame_title,
            backgroundBlur: CGFloat(record.background_blur),
            crop: crop
        )
    }

    /// The background picture a document names, if it may be used (ADR-0002).
    ///
    /// Inside the edit file's folder, or approved on this Mac before: used.
    /// Elsewhere: only after `confirm` says yes, which is then remembered.
    /// Missing file or a no: nil, and the caller falls back to the default preset.
    private static func usableBackground(_ stored: String, sidecar: URL, confirm: BackgroundConfirmation) -> URL? {
        guard let resolved = PixCapBridge.shared.resolveDocumentPath(sidecar: sidecar.path, stored: stored) else {
            return nil
        }

        let url = URL(fileURLWithPath: resolved.path)
        if resolved.in_document_folder || BackgroundTrust.isTrusted(resolved.path) {
            return url
        }
        guard confirm(url) else { return nil }
        BackgroundTrust.remember(url)
        return url
    }

    private static func decode(_ record: AnnotationRecordDTO) -> AnnotationItem {
        func point(_ values: [Double]) -> CGPoint {
            CGPoint(x: values.first ?? 0, y: values.count > 1 ? values[1] : 0)
        }

        return AnnotationItem(
            id: UUID(uuidString: record.id) ?? UUID(),
            tool: AnnotationTool(rawValue: record.tool) ?? .rectangle,
            start: point(record.start),
            end: point(record.end),
            points: record.points.map(point),
            colorHex: record.color_hex,
            strokeWidth: CGFloat(record.stroke_width),
            filled: record.filled,
            text: record.text,
            fontSize: CGFloat(record.font_size),
            number: record.number,
            blurStyle: record.blur_style == "pixelate" ? .pixelate : .gaussian,
            blurIntensity: CGFloat(record.blur_intensity),
            curvedArrow: record.curved_arrow,
            arrowHead: ArrowHead(rawValue: record.arrow_head) ?? .filled,
            dashed: record.dashed,
            spotlightDim: CGFloat(record.spotlight_dim)
        )
    }
}
