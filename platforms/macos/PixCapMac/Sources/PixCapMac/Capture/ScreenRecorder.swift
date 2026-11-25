import Foundation
import AppKit
import AVFoundation
import ScreenCaptureKit
import CoreMedia

/// Records the screen to MP4 (AVAssetWriter) or animated GIF (the Rust core).
///
/// Frames arrive from a `SCStream`; MP4 recordings append sample buffers
/// directly, while GIF recordings sample frames down to the configured GIF rate
/// and stream them to a `StreamingGIFWriter`, so memory stays flat (ADR-0003).
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
        /// The stream failed while starting; its stop handler reports the outcome.
        case interruptedWhileStarting

        public var errorDescription: String? {
            switch self {
            case .alreadyRecording: return "A recording is already in progress."
            case .noDisplay: return "No display available to record."
            case .writerSetupFailed(let reason): return "Could not start the recorder: \(reason)"
            case .nothingRecorded: return "No frames were captured."
            case .interruptedWhileStarting: return "The recording stopped as it started."
            }
        }
    }

    @Published public private(set) var isRecording = false
    @Published public private(set) var isPaused = false
    @Published public private(set) var elapsed: TimeInterval = 0

    private let queue = DispatchQueue(label: "app.pixcap.recorder")

    private enum Phase { case idle, active, finishing }

    /// Where the recorder is in its life: `start` claims it from `.idle`,
    /// the first stop moves it to `.finishing`, and it returns to `.idle`
    /// only once the file is complete.
    ///
    /// Read and written only on `queue`, so start/stop are each one atomic
    /// check-and-set. `isRecording` (main actor, for the UI) changes at other
    /// moments, so relying on it let a double hotkey press start two streams,
    /// let a user stop and a stream-error stop both call `finishWriting` (an
    /// AVFoundation exception that kills the app), or let a new recording
    /// start while the previous one was still being written and take over
    /// its writer and file.
    private var phase = Phase.idle
    /// Numbers each claimed recording, so a slow start can tell whether the
    /// recorder still belongs to it. Read and written only on `queue`.
    private var recordingID = 0

    /// Samples are used only while recording, not while finishing. Call on `queue`.
    private var isActive: Bool { phase == .active }

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
    // the output has no gap. Pause and resume are read from the stream's own
    // clock (`captureClock`), which stamps the samples. The last frame's stamp
    // is no good: on a still screen no new frames arrive, so a long pause
    // measured that way came out near zero and froze the GIF for the pause.
    //
    // Samples are delivered on `queue`, which owns all of these. A sample
    // stamped before the last resume (captured during the pause, delivered
    // late) is dropped, and video never goes backwards, because AVAssetWriter
    // fails the whole MP4 on an out-of-order timestamp.
    private var pausedAt: CMTime?
    private var pausedOffset: CMTime = .zero
    private var resumedAt: CMTime = .zero
    private var lastVideoTime: CMTime = .invalid
    private var captureClock: CMClock = CMClockGetHostTimeClock()

    private var clockNow: CMTime { CMClockGetTime(captureClock) }

    // GIF streaming. `gifWriter` is set before the stream starts and cleared
    // after it stops, so the sample queue never races its assignment.
    private var gifWriter: StreamingGIFWriter?
    private var gifFramesSent = 0
    private var gifFrameInterval: Double = 1.0 / 15.0
    private var lastGIFCapture: CMTime = .negativeInfinity
    /// GIF frames are scaled to fit within this size, in pixels. The height
    /// limit matches `MAX_GIF_SIDE` in the core's gif_writer.rs, which
    /// refuses larger frames; change both together.
    private let gifMaxWidth = 900
    private let gifMaxHeight = 4096
    /// Caps the frames kept: 60 s of changing screen at 15 fps (static
    /// stretches add no frames). The core's own cap is higher (3,600) and only
    /// guards against misuse.
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
        let claimed = queue.sync { () -> Int? in
            guard phase == .idle else { return nil }
            phase = .active
            recordingID += 1
            return recordingID
        }
        guard let id = claimed else { throw RecorderError.alreadyRecording }

        do {
            try await startClaimed(id: id, format: format, screen: screen, region: region)
        } catch RecorderError.interruptedWhileStarting {
            // The error stop already claimed and is finishing this recording.
            throw RecorderError.interruptedWhileStarting
        } catch {
            // Clean up only if this start still owns the recording; a stream
            // error may have claimed it meanwhile, and then its stop finishes
            // the file and reports to the user.
            guard claimStop(id: id) else { throw RecorderError.interruptedWhileStarting }
            discardFailedStart()
            throw error
        }
    }

    private func startClaimed(id: Int, format: RecordingFormat, screen: NSScreen?, region: CGRect?) async throws {
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
        gifFramesSent = 0
        lastGIFCapture = .negativeInfinity
        pausedOffset = .zero
        pausedAt = nil
        resumedAt = .zero
        lastVideoTime = .invalid
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
            let fit = min(1, Double(gifMaxWidth) / Double(configuration.width), Double(gifMaxHeight) / Double(configuration.height))
            let width = max(1, Int((Double(configuration.width) * fit).rounded()))
            let height = max(1, Int((Double(configuration.height) * fit).rounded()))
            guard let gifWriter = StreamingGIFWriter(url: url, width: width, height: height, frameDelay: gifFrameInterval) else {
                throw RecorderError.writerSetupFailed("could not create the GIF file")
            }
            self.gifWriter = gifWriter
        }

        // PixCap's own windows stay out of the recording.
        let ownApps = content.applications.filter { $0.bundleIdentifier == Bundle.main.bundleIdentifier }
        let filter = SCContentFilter(display: display, excludingApplications: ownApps, exceptingWindows: [])

        let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
        // Set before capture starts, so no sample handler is reading it yet.
        captureClock = stream.synchronizationClock ?? CMClockGetHostTimeClock()
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        if wantsAudio {
            try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue)
        }
        try await stream.startCapture()

        // A stream error during startCapture may already have stopped this
        // recording; that stop owns the cleanup and the report to the user.
        guard queue.sync(execute: { phase == .active && recordingID == id }) else {
            try? await stream.stopCapture()
            throw RecorderError.interruptedWhileStarting
        }
        self.stream = stream

        await MainActor.run {
            // A stream error after the check above may already have finished
            // this recording (and cleared `isRecording`); showing it as running
            // then would leave the UI stuck in "recording" until relaunch.
            guard queue.sync(execute: { phase == .active && recordingID == id }) else { return }
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
        _ = gifWriter?.finish(keep: false)
        gifWriter = nil
        videoInput = nil
        audioInput = nil
        if let outputURL {
            try? FileManager.default.removeItem(at: outputURL)
            self.outputURL = nil
        }
        queue.sync { phase = .idle }
    }

    /// Atomically moves the recorder to finishing. True only for the one caller
    /// that should finish it; with `id`, only if that recording is still the current one.
    private func claimStop(id: Int? = nil) -> Bool {
        queue.sync {
            guard phase == .active, id == nil || id == recordingID else { return false }
            phase = .finishing
            return true
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
        let url = await finishOutput()
        // Only now may a new recording start: until here this one still owns
        // the writer, the stream and `outputURL`.
        queue.sync { phase = .idle }
        return url
    }

    private func finishOutput() async -> URL? {
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
            if pausedAt != nil {
                let now = clockNow
                if let pausedAt {
                    pausedOffset = CMTimeAdd(pausedOffset, CMTimeSubtract(now, pausedAt))
                }
                pausedAt = nil
                resumedAt = now
            } else {
                pausedAt = clockNow
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
        guard let gifWriter else { return nil }
        self.gifWriter = nil
        // The recording ends now (or when it was paused), on the frames' clock,
        // so a still stretch before Stop keeps its length.
        let end = queue.sync { CMTimeGetSeconds(CMTimeSubtract(pausedAt ?? clockNow, pausedOffset)) }
        // Waits for at most `maxBacklog` queued frames, then writes the trailer.
        let written = gifWriter.finish(keep: true, endingAt: end)
        let dropped = gifWriter.dropped
        if dropped > 0 { NSLog("PixCap: GIF encoder fell behind; %d frames dropped", dropped) }
        return written ? outputURL : nil
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

        guard isLive(sampleBuffer) else { return }

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
        if lastVideoTime.isValid, CMTimeCompare(presentationTime, lastVideoTime) <= 0 { return }

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

        // Appending the original stamp would put this frame out of order.
        guard status == noErr, let adjusted else { return }
        if videoInput.append(adjusted) { lastVideoTime = presentationTime }
    }

    /// False while paused, and for samples captured before the last resume
    /// but delivered after it. Runs on `queue`.
    private func isLive(_ sampleBuffer: CMSampleBuffer) -> Bool {
        guard pausedAt == nil else { return false }
        return CMTimeCompare(CMSampleBufferGetPresentationTimeStamp(sampleBuffer), resumedAt) >= 0
    }

    private func appendAudioSample(_ sampleBuffer: CMSampleBuffer) {
        guard sessionStarted, let audioInput, audioInput.isReadyForMoreMediaData else { return }
        guard pausedOffset != .zero else {
            audioInput.append(sampleBuffer)
            return
        }
        // Re-stamp like video, so audio stays in sync after a pause.
        var count: CMItemCount = 0
        guard CMSampleBufferGetSampleTimingInfoArray(sampleBuffer, entryCount: 0, arrayToFill: nil,
                                                     entriesNeededOut: &count) == noErr, count > 0 else { return }
        var timing = [CMSampleTimingInfo](repeating: CMSampleTimingInfo(), count: count)
        guard CMSampleBufferGetSampleTimingInfoArray(sampleBuffer, entryCount: count, arrayToFill: &timing,
                                                     entriesNeededOut: &count) == noErr else { return }
        for index in timing.indices {
            timing[index].presentationTimeStamp = CMTimeSubtract(timing[index].presentationTimeStamp, pausedOffset)
            if timing[index].decodeTimeStamp.isValid {
                timing[index].decodeTimeStamp = CMTimeSubtract(timing[index].decodeTimeStamp, pausedOffset)
            }
        }
        var adjusted: CMSampleBuffer?
        guard CMSampleBufferCreateCopyWithNewTiming(allocator: kCFAllocatorDefault, sampleBuffer: sampleBuffer,
                                                    sampleTimingEntryCount: count, sampleTimingArray: &timing,
                                                    sampleBufferOut: &adjusted) == noErr,
              let adjusted else { return }
        audioInput.append(adjusted)
    }

    private func handleAudio(_ sampleBuffer: CMSampleBuffer) {
        guard isLive(sampleBuffer) else { return }
        appendAudioSample(sampleBuffer)
    }

    private func appendGIFFrame(_ sampleBuffer: CMSampleBuffer) {
        guard let gifWriter, gifFramesSent < gifFrameLimit else { return }

        let time = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        if lastGIFCapture != .negativeInfinity {
            let delta = CMTimeGetSeconds(CMTimeSubtract(time, lastGIFCapture))
            guard delta >= gifFrameInterval * 0.9 else { return }
        }
        lastGIFCapture = time

        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let rgba = StreamingGIFWriter.rgba(
            from: CIImage(cvPixelBuffer: pixelBuffer),
            width: gifWriter.width,
            height: gifWriter.height,
            context: gifContext
        )
        // Paused time is removed so a pause leaves no gap in playback.
        let captured = CMTimeGetSeconds(CMTimeSubtract(time, pausedOffset))
        gifWriter.enqueue(rgba, at: captured)
        gifFramesSent += 1
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
