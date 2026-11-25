import Foundation
import AppKit
import AVFoundation
import ScreenCaptureKit
import CoreMedia
import ImageIO
import UniformTypeIdentifiers

/// Records the screen to MP4 (AVAssetWriter) or animated GIF (ImageIO).
///
/// Frames arrive from a `SCStream`; MP4 recordings append sample buffers
/// directly, while GIF recordings sample frames down to the configured GIF rate
/// and encode on stop.
@available(macOS 14.0, *)
public final class ScreenRecorder: NSObject, ObservableObject, SCStreamOutput, SCStreamDelegate {
    public static let shared = ScreenRecorder()

    public enum RecordingFormat: String, CaseIterable {
        case mp4 = "MP4"
        case gif = "GIF"

        var fileExtension: String { self == .mp4 ? "mp4" : "gif" }
    }

    public enum RecorderError: LocalizedError {
        case alreadyRecording
        case noDisplay
        case writerSetupFailed(String)
        case nothingRecorded

        public var errorDescription: String? {
            switch self {
            case .alreadyRecording: return "A recording is already in progress."
            case .noDisplay: return "No display available to record."
            case .writerSetupFailed(let reason): return "Could not start the recorder: \(reason)"
            case .nothingRecorded: return "No frames were captured."
            }
        }
    }

    @Published public private(set) var isRecording = false
    @Published public private(set) var isPaused = false
    @Published public private(set) var elapsed: TimeInterval = 0

    private let queue = DispatchQueue(label: "app.pixcap.recorder")

    /// True from the moment `start` claims the recorder until `stop` releases it.
    ///
    /// Read and written only on `queue`, so start/stop are each one atomic
    /// check-and-set. `isRecording` (main actor, for the UI) flips only after
    /// the stream is running, which left a window where a double hotkey press
    /// started two streams, or a user stop and a stream-error stop both called
    /// `finishWriting` (an AVFoundation exception that kills the app).
    private var isActive = false

    /// Called on the main actor when the stream stops on its own (for example
    /// the recorded display was unplugged), with the finished file or nil.
    public var onStoppedUnexpectedly: ((URL?) -> Void)?

    /// One Core Image context for every GIF frame; building one per frame is expensive.
    private let gifContext = CIContext()

    private var stream: SCStream?
    private var writer: AVAssetWriter?
    private var videoInput: AVAssetWriterInput?
    private var audioInput: AVAssetWriterInput?
    private var sessionStarted = false

    private var format: RecordingFormat = .mp4
    private var outputURL: URL?
    private var startedAt: Date?
    private var elapsedTimer: Timer?

    // Pause bookkeeping: paused time is subtracted from presentation stamps so
    // the output has no gap.
    private var pausedAt: CMTime?
    private var pausedOffset: CMTime = .zero
    private var lastPresentationTime: CMTime = .zero

    // GIF accumulation
    private var gifFrames: [CGImage] = []
    private var gifFrameInterval: Double = 1.0 / 15.0
    private var lastGIFCapture: CMTime = .negativeInfinity
    private var gifMaxWidth: CGFloat = 900
    /// Hard cap so a forgotten recording cannot exhaust memory.
    private let gifFrameLimit = 900

    private override init() {
        super.init()
    }

    // MARK: - Lifecycle

    /// Starts recording a display, optionally limited to `region` (global AppKit coordinates).
    public func start(
        format: RecordingFormat,
        screen: NSScreen? = nil,
        region: CGRect? = nil
    ) async throws {
        guard ScreenCaptureEngine.shared.ensurePermission() else { throw CaptureError.permissionDenied }
        let claimed = queue.sync { () -> Bool in
            guard !isActive else { return false }
            isActive = true
            return true
        }
        guard claimed else { throw RecorderError.alreadyRecording }

        do {
            try await startClaimed(format: format, screen: screen, region: region)
        } catch {
            discardFailedStart()
            throw error
        }
    }

    private func startClaimed(format: RecordingFormat, screen: NSScreen?, region: CGRect?) async throws {
        // Cleared first so a failed start never deletes the previous recording's file.
        outputURL = nil

        // Record the display the user is working on, as full-screen capture does.
        let targetScreen = screen ?? NSScreen.underCursor ?? NSScreen.main
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        guard let display = content.displays.first(where: { display in
            guard let displayID = targetScreen?.displayID else { return true }
            return display.displayID == displayID
        }) ?? content.displays.first else {
            throw RecorderError.noDisplay
        }

        self.format = format
        gifFrames.removeAll()
        lastGIFCapture = .negativeInfinity
        pausedOffset = .zero
        pausedAt = nil
        sessionStarted = false

        let fps = Double(Settings.string(format == .gif ? SettingsKey.gifFPS : SettingsKey.videoFPS, default: format == .gif ? "15" : "60")) ?? 30
        gifFrameInterval = 1.0 / max(fps, 1)

        let scale = Settings.renderScale(for: targetScreen)
        let configuration = SCStreamConfiguration()
        configuration.showsCursor = Settings.bool(SettingsKey.showCursorInRecording)
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(max(fps, 1)))
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        configuration.queueDepth = 6

        var pixelWidth = Int(CGFloat(display.width) * scale)
        var pixelHeight = Int(CGFloat(display.height) * scale)

        if let region, let targetScreen {
            // ScreenCaptureKit's sourceRect is display-local with a top-left origin.
            let sourceRect = CGRect(
                x: region.minX - targetScreen.frame.minX,
                y: targetScreen.frame.maxY - region.maxY,
                width: region.width,
                height: region.height
            )
            configuration.sourceRect = sourceRect
            pixelWidth = Int((sourceRect.width * scale).rounded())
            pixelHeight = Int((sourceRect.height * scale).rounded())
        }

        // H.264 requires even dimensions.
        configuration.width = max(2, pixelWidth - (pixelWidth % 2))
        configuration.height = max(2, pixelHeight - (pixelHeight % 2))

        let wantsAudio = format == .mp4 && audioRequested
        configuration.capturesAudio = wantsAudio

        let url = makeOutputURL(for: format)
        outputURL = url

        if format == .mp4 {
            try setUpWriter(url: url, width: configuration.width, height: configuration.height, withAudio: wantsAudio)
        } else {
            gifMaxWidth = min(900, CGFloat(configuration.width))
        }

        // PixCap's own windows stay out of the recording.
        let ownApps = content.applications.filter { $0.bundleIdentifier == Bundle.main.bundleIdentifier }
        let filter = SCContentFilter(display: display, excludingApplications: ownApps, exceptingWindows: [])

        let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        if wantsAudio {
            try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue)
        }
        try await stream.startCapture()
        self.stream = stream

        await MainActor.run {
            self.isRecording = true
            self.isPaused = false
            self.elapsed = 0
            self.startedAt = Date()
            self.startElapsedTimer()
        }
    }

    /// Undoes a start that failed part-way: no stream left running, no empty file left behind.
    private func discardFailedStart() {
        // `stream` is only stored once capture is running, which is the last
        // step that can throw, so there is never a live stream to stop here.
        writer?.cancelWriting()
        writer = nil
        videoInput = nil
        audioInput = nil
        if let outputURL {
            try? FileManager.default.removeItem(at: outputURL)
            self.outputURL = nil
        }
        queue.sync { isActive = false }
    }

    /// Atomically releases the recorder. True only for the one caller that should finish it.
    private func claimStop() -> Bool {
        queue.sync {
            let wasActive = isActive
            isActive = false
            return wasActive
        }
    }

    /// Stops recording and finalises the file. Safe to call more than once:
    /// only the first call does the work, later ones return nil.
    @discardableResult
    public func stop() async -> URL? {
        guard claimStop() else { return nil }
        return await finish()
    }

    private func finish() async -> URL? {
        try? await stream?.stopCapture()
        stream = nil
        // Let any sample callback already running on the queue finish before
        // the inputs are marked finished; new ones now return at `isActive`.
        queue.sync {}

        await MainActor.run {
            self.isRecording = false
            self.isPaused = false
            self.elapsedTimer?.invalidate()
            self.elapsedTimer = nil
        }

        switch format {
        case .mp4:
            return await finishVideo()
        case .gif:
            return finishGIF()
        }
    }

    public func togglePause() {
        guard isRecording else { return }
        queue.sync {
            if isPaused {
                if let pausedAt {
                    pausedOffset = CMTimeAdd(pausedOffset, CMTimeSubtract(lastPresentationTime, pausedAt))
                }
                pausedAt = nil
            } else {
                pausedAt = lastPresentationTime
            }
        }
        isPaused.toggle()
    }

    /// True when the user asked for any audio source.
    private var audioRequested: Bool {
        Settings.string(SettingsKey.audioSource, default: "System") != "None"
    }

    // MARK: - Writer

    private func setUpWriter(url: URL, width: Int, height: Int, withAudio: Bool) throws {
        do {
            let writer = try AVAssetWriter(outputURL: url, fileType: .mp4)

            let videoSettings: [String: Any] = [
                AVVideoCodecKey: AVVideoCodecType.h264,
                AVVideoWidthKey: width,
                AVVideoHeightKey: height,
                AVVideoCompressionPropertiesKey: [
                    AVVideoAverageBitRateKey: max(2_000_000, width * height * 6),
                    AVVideoMaxKeyFrameIntervalKey: 60
                ]
            ]

            let videoInput = AVAssetWriterInput(mediaType: .video, outputSettings: videoSettings)
            videoInput.expectsMediaDataInRealTime = true
            guard writer.canAdd(videoInput) else {
                throw RecorderError.writerSetupFailed("video input rejected")
            }
            writer.add(videoInput)

            if withAudio {
                let audioSettings: [String: Any] = [
                    AVFormatIDKey: kAudioFormatMPEG4AAC,
                    AVNumberOfChannelsKey: 2,
                    AVSampleRateKey: 48_000,
                    AVEncoderBitRateKey: 128_000
                ]
                let audioInput = AVAssetWriterInput(mediaType: .audio, outputSettings: audioSettings)
                audioInput.expectsMediaDataInRealTime = true
                if writer.canAdd(audioInput) {
                    writer.add(audioInput)
                    self.audioInput = audioInput
                }
            }

            guard writer.startWriting() else {
                throw RecorderError.writerSetupFailed(writer.error?.localizedDescription ?? "unknown")
            }

            self.writer = writer
            self.videoInput = videoInput
        } catch let error as RecorderError {
            throw error
        } catch {
            throw RecorderError.writerSetupFailed(error.localizedDescription)
        }
    }

    private func finishVideo() async -> URL? {
        guard let writer, let videoInput else { return nil }

        videoInput.markAsFinished()
        audioInput?.markAsFinished()
        await writer.finishWriting()

        self.writer = nil
        self.videoInput = nil
        self.audioInput = nil

        guard writer.status == .completed else {
            NSLog("PixCap: recording failed — \(writer.error?.localizedDescription ?? "unknown")")
            return nil
        }
        return outputURL
    }

    private func finishGIF() -> URL? {
        let frames = queue.sync { gifFrames }
        guard !frames.isEmpty, let url = outputURL else { return nil }

        let written = Self.writeGIF(frames: frames, to: url, frameDelay: gifFrameInterval)
        queue.sync { gifFrames.removeAll() }
        return written ? url : nil
    }

    /// Encodes frames into a looping GIF.
    static func writeGIF(frames: [CGImage], to url: URL, frameDelay: Double) -> Bool {
        guard let destination = CGImageDestinationCreateWithURL(
            url as CFURL,
            UTType.gif.identifier as CFString,
            frames.count,
            nil
        ) else { return false }

        let fileProperties = [
            kCGImagePropertyGIFDictionary: [kCGImagePropertyGIFLoopCount: 0]
        ] as CFDictionary
        CGImageDestinationSetProperties(destination, fileProperties)

        let frameProperties = [
            kCGImagePropertyGIFDictionary: [kCGImagePropertyGIFDelayTime: frameDelay]
        ] as CFDictionary

        for frame in frames {
            CGImageDestinationAddImage(destination, frame, frameProperties)
        }

        return CGImageDestinationFinalize(destination)
    }

    // MARK: - SCStreamOutput

    public func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard isActive, CMSampleBufferDataIsReady(sampleBuffer) else { return }

        switch type {
        case .screen:
            handleVideo(sampleBuffer)
        case .audio:
            handleAudio(sampleBuffer)
        default:
            break
        }
    }

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        NSLog("PixCap: capture stream stopped — \(error.localizedDescription)")
        // A user stop that already ran owns the UI update; only report if this call did the stop.
        guard claimStop() else { return }
        Task {
            let url = await finish()
            await MainActor.run { self.onStoppedUnexpectedly?(url) }
        }
    }

    private func handleVideo(_ sampleBuffer: CMSampleBuffer) {
        // Skip frames the compositor marked as idle or blank.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
           let raw = attachments.first?[.status] as? Int,
           let status = SCFrameStatus(rawValue: raw),
           status != .complete {
            return
        }

        lastPresentationTime = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        guard !isPaused else { return }

        switch format {
        case .gif:
            appendGIFFrame(sampleBuffer)
        case .mp4:
            appendVideoSample(sampleBuffer)
        }
    }

    private func appendVideoSample(_ sampleBuffer: CMSampleBuffer) {
        guard let writer, let videoInput else { return }

        let presentationTime = CMTimeSubtract(CMSampleBufferGetPresentationTimeStamp(sampleBuffer), pausedOffset)

        if !sessionStarted {
            writer.startSession(atSourceTime: presentationTime)
            sessionStarted = true
        }

        guard videoInput.isReadyForMoreMediaData else { return }

        // Re-stamp so paused stretches do not appear as frozen frames.
        var timing = CMSampleTimingInfo(
            duration: CMSampleBufferGetDuration(sampleBuffer),
            presentationTimeStamp: presentationTime,
            decodeTimeStamp: .invalid
        )

        var adjusted: CMSampleBuffer?
        let status = CMSampleBufferCreateCopyWithNewTiming(
            allocator: kCFAllocatorDefault,
            sampleBuffer: sampleBuffer,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleBufferOut: &adjusted
        )

        if status == noErr, let adjusted {
            videoInput.append(adjusted)
        } else {
            videoInput.append(sampleBuffer)
        }
    }

    private func appendAudioSample(_ sampleBuffer: CMSampleBuffer) {
        guard sessionStarted, let audioInput, audioInput.isReadyForMoreMediaData else { return }
        audioInput.append(sampleBuffer)
    }

    private func handleAudio(_ sampleBuffer: CMSampleBuffer) {
        guard !isPaused else { return }
        appendAudioSample(sampleBuffer)
    }

    private func appendGIFFrame(_ sampleBuffer: CMSampleBuffer) {
        guard gifFrames.count < gifFrameLimit else { return }

        let time = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        if lastGIFCapture != .negativeInfinity {
            let delta = CMTimeGetSeconds(CMTimeSubtract(time, lastGIFCapture))
            guard delta >= gifFrameInterval * 0.9 else { return }
        }
        lastGIFCapture = time

        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let ciImage = CIImage(cvPixelBuffer: pixelBuffer)
        let context = gifContext

        let scale = min(1, gifMaxWidth / max(ciImage.extent.width, 1))
        let scaled = scale < 1
            ? ciImage.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
            : ciImage

        if let cgImage = context.createCGImage(scaled, from: scaled.extent) {
            gifFrames.append(cgImage)
        }
    }

    // MARK: - Helpers

    private func startElapsedTimer() {
        elapsedTimer?.invalidate()
        elapsedTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self, let startedAt = self.startedAt, !self.isPaused else { return }
            self.elapsed = Date().timeIntervalSince(startedAt)
        }
    }

    private func makeOutputURL(for format: RecordingFormat) -> URL {
        let directory = Settings.saveDirectory
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

        let base = PixCapBridge.shared.resolveFilename(
            pattern: Settings.namingPattern,
            mode: format == .gif ? "gif" : "recording",
            appName: nil,
            windowTitle: nil,
            width: 0,
            height: 0,
            counter: Settings.nextCaptureCounter()
        )

        let safe = base.replacingOccurrences(of: "/", with: "-")
        var url = directory.appendingPathComponent("\(safe).\(format.fileExtension)")
        var counter = 2
        while FileManager.default.fileExists(atPath: url.path) {
            url = directory.appendingPathComponent("\(safe) \(counter).\(format.fileExtension)")
            counter += 1
        }
        return url
    }
}
