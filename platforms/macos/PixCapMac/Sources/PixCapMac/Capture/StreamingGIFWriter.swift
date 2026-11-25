import Foundation
import CoreImage

/// Writes a GIF recording frame by frame through the Rust core (ADR-0003).
///
/// Frames are encoded on a private serial queue, so the capture queue never
/// waits for the encoder. At most `maxBacklog` frames wait at once; beyond
/// that new frames are dropped, which lowers the effective frame rate on a
/// slow Mac instead of stalling capture or growing memory.
///
/// Each frame is shown until the next kept frame's capture time, so the GIF
/// plays in real time whether frames were dropped here or skipped by
/// ScreenCaptureKit because the screen did not change. That needs the next
/// frame's time, so the newest frame is held back until another arrives (or
/// the recording finishes).
final class StreamingGIFWriter {
    /// Frames allowed to wait for the encoder. An estimate (ADR-0003 §4.3):
    /// 4 x 900 x 506 x 4 bytes is about 7.3 MB held at worst (plus the one
    /// frame held back), and encoding takes about 29 ms per frame against a
    /// 66 ms frame interval at 15 fps.
    static let maxBacklog = 4

    let width: Int
    let height: Int

    private let encodeQueue = DispatchQueue(label: "app.pixcap.gif-encode")
    /// Touched only on `encodeQueue`; nil once finished.
    private var handle: OpaquePointer?

    /// How long the last frame is shown, in seconds: one frame interval.
    private let frameDelay: Double

    // All below are guarded by `lock`.
    private let lock = NSLock()
    private var pending = 0
    private var droppedFrames = 0
    /// The newest frame, waiting for the next one to know how long it shows.
    private var held: (rgba: Data, time: Double)?
    /// Capture time of the first frame; frame times are measured from it.
    private var origin: Double?
    /// Centiseconds already given to frames. Rounding against the running
    /// total keeps 15 fps (6.67 cs) from drifting to 7 cs a frame.
    private var emittedCentiseconds = 0

    /// Opens the writer, or returns nil when the core refuses (bad size, unwritable folder).
    init?(url: URL, width: Int, height: Int, frameDelay: Double) {
        guard width > 0, height > 0, frameDelay > 0 else { return nil }
        guard let handle = url.path.withCString({
            pixcap_gif_begin($0, UInt32(width), UInt32(height))
        }) else { return nil }
        self.handle = handle
        self.frameDelay = frameDelay
        self.width = width
        self.height = height
    }

    deinit {
        // Unreachable while frames are queued (each block holds `self`), so no
        // lock is needed. Discards an unfinished file.
        if let handle { _ = pixcap_gif_finish(handle, 0) }
    }

    /// Frames dropped so far because the encoder fell behind.
    var dropped: Int {
        lock.lock(); defer { lock.unlock() }
        return droppedFrames
    }

    /// Queues one RGBA frame of `width` x `height`, captured at `time` seconds
    /// (any clock that only moves forward, with paused time removed).
    /// Returns false if it was dropped.
    @discardableResult
    func enqueue(_ rgba: Data, at time: Double) -> Bool {
        lock.lock()
        guard pending < Self.maxBacklog else {
            droppedFrames += 1
            lock.unlock()
            return false
        }
        let origin = self.origin ?? time
        self.origin = origin
        var ready: (rgba: Data, delay: UInt32)?
        if let previous = held {
            ready = (previous.rgba, delayUntil(time - origin))
            pending += 1
        }
        held = (rgba, time)
        lock.unlock()

        if let ready { encode(ready.rgba, delay: ready.delay) }
        return true
    }

    /// Writes the held frame, waits for queued frames, then completes the
    /// file (`keep`) or deletes it. True only when a complete GIF is at the
    /// URL. Later calls return false.
    ///
    /// `end` is when the recording stopped, on the frames' clock; the last
    /// frame shows until then (at least one frame interval).
    func finish(keep: Bool, endingAt end: Double? = nil) -> Bool {
        lock.lock()
        var last: (rgba: Data, delay: UInt32)?
        if keep, let frame = held, let origin {
            // A NaN `end` (invalid clock reading) falls back to one frame interval.
            let stop = end.flatMap { $0.isFinite ? $0 : nil } ?? 0
            let shownUntil = max(stop, frame.time + frameDelay)
            last = (frame.rgba, delayUntil(shownUntil - origin))
            pending += 1
        }
        held = nil
        lock.unlock()

        if let last { encode(last.rgba, delay: last.delay) }
        return encodeQueue.sync {
            guard let handle else { return false }
            self.handle = nil
            return pixcap_gif_finish(handle, keep ? 1 : 0) == 0 && keep
        }
    }

    /// Centiseconds the frame now being released is shown, so that frames so
    /// far add up to `elapsed` seconds. Call with `lock` held.
    private func delayUntil(_ elapsed: Double) -> UInt32 {
        // GIF delays are whole centiseconds, 1...65535.
        let target = Int((max(0, elapsed) * 100).rounded())
        let delay = UInt32(min(65_535, max(1, target - emittedCentiseconds)))
        // When the cap cuts a very long still stretch (over 655 s), drop the
        // excess rather than carry it onto the next frame.
        emittedCentiseconds = max(emittedCentiseconds + Int(delay), target)
        return delay
    }

    private func encode(_ rgba: Data, delay: UInt32) {
        encodeQueue.async { [self] in
            if let handle {
                let status = rgba.withUnsafeBytes { bytes in
                    pixcap_gif_add_frame(
                        handle,
                        bytes.bindMemory(to: UInt8.self).baseAddress,
                        bytes.count,
                        UInt32(width),
                        UInt32(height),
                        delay
                    )
                }
                if status != 0 { NSLog("PixCap: a GIF frame could not be encoded") }
            }
            lock.lock()
            pending -= 1
            lock.unlock()
        }
    }

    /// Renders `image` scaled to exactly `width` x `height` as top-down RGBA bytes.
    static func rgba(from image: CIImage, width: Int, height: Int, context: CIContext) -> Data {
        let extent = image.extent
        let scaled = image
            .transformed(by: CGAffineTransform(translationX: -extent.minX, y: -extent.minY))
            .transformed(by: CGAffineTransform(
                scaleX: CGFloat(width) / max(extent.width, 1),
                y: CGFloat(height) / max(extent.height, 1)
            ))
        var data = Data(count: width * height * 4)
        data.withUnsafeMutableBytes { bytes in
            guard let base = bytes.baseAddress else { return }
            context.render(
                scaled,
                toBitmap: base,
                rowBytes: width * 4,
                bounds: CGRect(x: 0, y: 0, width: width, height: height),
                format: .RGBA8,
                colorSpace: CGColorSpace(name: CGColorSpace.sRGB)
            )
        }
        return data
    }
}
