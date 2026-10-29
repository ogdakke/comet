import AVFoundation
import Foundation
import UniformTypeIdentifiers

/// An original video played straight off the relay: AVFoundation asks for
/// byte ranges and the loader answers them from Studio's artifact chunks, so
/// nothing is written to disk and only a bounded window stays in memory.
final class StudioVideoStream {
    let player: AVPlayer

    private let loader: StudioVideoResourceLoader
    private let asset: AVURLAsset

    init(
        artifactId: String,
        mimeType: String,
        declaredSize: UInt64,
        readChunk: @escaping @Sendable (UInt64) async throws -> StudioArtifactBytes
    ) {
        loader = StudioVideoResourceLoader(
            mimeType: mimeType,
            declaredSize: declaredSize,
            readChunk: readChunk
        )
        let suffix = UTType(mimeType: mimeType)?.preferredFilenameExtension ?? "mp4"
        let url = URL(string: "zeron-studio-video://media/\(artifactId).\(suffix)")!
        asset = AVURLAsset(url: url)
        asset.resourceLoader.setDelegate(loader, queue: loader.queue)
        player = AVPlayer(playerItem: AVPlayerItem(asset: asset))
        player.automaticallyWaitsToMinimizeStalling = true
    }

    func cancel() {
        player.pause()
        // Dropping the item cancels AVFoundation's outstanding range requests
        // instead of letting them drain against a closed viewer.
        player.replaceCurrentItem(with: nil)
        loader.cancelAll()
    }
}

/// Studio videos play under `.playback`, so they're heard with the ring
/// switch on silent like Photos — unless a voice call owns the session.
@MainActor
enum StudioViewerAudioSession {
    private static var active = false

    static func activate() {
        let session = AVAudioSession.sharedInstance()
        guard !active, session.category != .playAndRecord else { return }
        do {
            try session.setCategory(.playback, mode: .moviePlayback)
            try session.setActive(true)
            active = true
        } catch {
            // Playback still works under the default category.
        }
    }

    /// Hand audio back to whatever the viewer interrupted (music resumes).
    static func deactivate() {
        guard active else { return }
        active = false
        let session = AVAudioSession.sharedInstance()
        // A call that started meanwhile has replaced the category; leave it.
        guard session.category == .playback else { return }
        try? session.setActive(false, options: .notifyOthersOnDeactivation)
        try? session.setCategory(.soloAmbient)
    }
}

/// The relay's recent chunks. AVFoundation re-requests overlapping ranges
/// while it probes and seeks, so a lookup finds the chunk *covering* an
/// offset, and concurrent requests for the same chunk share one relay read.
private actor StudioVideoChunkCache {
    /// The engine serves artifacts in chunks of this size from the requested
    /// offset (`ARTIFACT_READ_CHUNK_BYTES`); reading on that grid lets
    /// unaligned range requests share chunks. A different grid stays correct,
    /// just with fewer hits.
    static let grid: UInt64 = 192_000

    private struct Entry {
        let start: UInt64
        let bytes: StudioArtifactBytes
        var end: UInt64 { start + UInt64(bytes.data.count) }
    }

    private let countLimit: Int
    private var entries: [Entry] = []
    private var inFlight: [UInt64: Task<StudioArtifactBytes, Error>] = [:]

    init(countLimit: Int) {
        self.countLimit = countLimit
    }

    /// The chunk holding `offset` and the offset its data starts at.
    func chunk(
        containing offset: UInt64,
        load: @escaping @Sendable (UInt64) async throws -> StudioArtifactBytes
    ) async throws -> (start: UInt64, bytes: StudioArtifactBytes) {
        if let index = entries.firstIndex(where: { $0.start <= offset && offset < $0.end }) {
            let entry = entries.remove(at: index)
            entries.append(entry)
            return (entry.start, entry.bytes)
        }
        let aligned = offset - offset % Self.grid
        let bytes = try await read(at: aligned, load: load)
        if offset < aligned + UInt64(bytes.data.count) || bytes.done {
            return (aligned, bytes)
        }
        return (offset, try await read(at: offset, load: load))
    }

    func removeAll() {
        inFlight.values.forEach { $0.cancel() }
        inFlight.removeAll()
        entries.removeAll()
    }

    private func read(
        at start: UInt64,
        load: @escaping @Sendable (UInt64) async throws -> StudioArtifactBytes
    ) async throws -> StudioArtifactBytes {
        if let entry = entries.first(where: { $0.start == start }) { return entry.bytes }
        let task: Task<StudioArtifactBytes, Error>
        if let existing = inFlight[start] {
            task = existing
        } else {
            task = Task { try await load(start) }
            inFlight[start] = task
        }
        defer { if inFlight[start] == task { inFlight[start] = nil } }
        let bytes = try await task.value
        if !entries.contains(where: { $0.start == start }) {
            entries.append(Entry(start: start, bytes: bytes))
            if entries.count > countLimit { entries.removeFirst(entries.count - countLimit) }
        }
        return bytes
    }
}

private final class StudioVideoResourceLoader: NSObject, AVAssetResourceLoaderDelegate, @unchecked Sendable {
    let queue = DispatchQueue(label: "sh.zeron.studio-video-loader", qos: .userInitiated)

    private let mimeType: String
    private let declaredSize: UInt64
    private let readChunk: @Sendable (UInt64) async throws -> StudioArtifactBytes
    // ~9 MB: enough to scrub back over what just played without a re-read.
    private let cache = StudioVideoChunkCache(countLimit: 48)
    private var requests: [ObjectIdentifier: Task<Void, Never>] = [:]

    init(
        mimeType: String,
        declaredSize: UInt64,
        readChunk: @escaping @Sendable (UInt64) async throws -> StudioArtifactBytes
    ) {
        self.mimeType = mimeType
        self.declaredSize = declaredSize
        self.readChunk = readChunk
    }

    func resourceLoader(
        _ resourceLoader: AVAssetResourceLoader,
        shouldWaitForLoadingOfRequestedResource loadingRequest: AVAssetResourceLoadingRequest
    ) -> Bool {
        if let information = loadingRequest.contentInformationRequest {
            information.contentType = UTType(mimeType: mimeType)?.identifier ?? UTType.movie.identifier
            information.contentLength = Int64(clamping: declaredSize)
            information.isByteRangeAccessSupported = true
        }

        guard let dataRequest = loadingRequest.dataRequest else {
            loadingRequest.finishLoading()
            return true
        }

        let key = ObjectIdentifier(loadingRequest)
        let readChunk = readChunk
        requests[key] = Task { [weak self, weak loadingRequest] in
            guard let self, let loadingRequest else { return }
            do {
                var offset = UInt64(max(dataRequest.currentOffset, dataRequest.requestedOffset))
                let requestedEnd = dataRequest.requestsAllDataToEndOfResource
                    ? declaredSize
                    : min(declaredSize, offset.saturatingAdd(UInt64(max(0, dataRequest.requestedLength))))

                while offset < requestedEnd {
                    try Task.checkCancellation()
                    let (start, chunk) = try await cache.chunk(containing: offset, load: readChunk)
                    let skip = Int(clamping: offset - start)
                    guard skip < chunk.data.count else { break }
                    let count = min(chunk.data.count - skip, Int(clamping: requestedEnd - offset))
                    guard count > 0 else { break }
                    let lower = chunk.data.startIndex + skip
                    try Task.checkCancellation()
                    dataRequest.respond(with: chunk.data.subdata(in: lower..<(lower + count)))
                    offset += UInt64(count)
                    if chunk.done, skip + count >= chunk.data.count { break }
                }
                if !loadingRequest.isCancelled { loadingRequest.finishLoading() }
            } catch {
                // A cancelled request is AVFoundation's to drop; answering it is an error.
                if !loadingRequest.isCancelled, !(error is CancellationError) {
                    loadingRequest.finishLoading(with: error)
                }
            }
            queue.async { [weak self] in self?.requests.removeValue(forKey: key) }
        }
        return true
    }

    func resourceLoader(
        _ resourceLoader: AVAssetResourceLoader,
        didCancel loadingRequest: AVAssetResourceLoadingRequest
    ) {
        requests.removeValue(forKey: ObjectIdentifier(loadingRequest))?.cancel()
    }

    func cancelAll() {
        queue.async { [weak self] in
            guard let self else { return }
            requests.values.forEach { $0.cancel() }
            requests.removeAll()
            Task { await self.cache.removeAll() }
        }
    }
}

private extension UInt64 {
    func saturatingAdd(_ other: UInt64) -> UInt64 {
        let (sum, overflow) = addingReportingOverflow(other)
        return overflow ? .max : sum
    }
}
