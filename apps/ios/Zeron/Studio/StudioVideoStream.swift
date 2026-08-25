import AVFoundation
import Foundation
import UniformTypeIdentifiers

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
        loader.cancelAll()
    }
}

private actor StudioVideoChunkCache {
    private let countLimit: Int
    private var entries: [UInt64: StudioArtifactBytes] = [:]
    private var order: [UInt64] = []

    init(countLimit: Int) {
        self.countLimit = countLimit
    }

    func chunk(
        at offset: UInt64,
        load: @Sendable (UInt64) async throws -> StudioArtifactBytes
    ) async throws -> StudioArtifactBytes {
        if let cached = entries[offset] {
            order.removeAll { $0 == offset }
            order.append(offset)
            return cached
        }
        let loaded = try await load(offset)
        entries[offset] = loaded
        order.append(offset)
        while order.count > countLimit {
            entries.removeValue(forKey: order.removeFirst())
        }
        return loaded
    }

    func removeAll() {
        entries.removeAll()
        order.removeAll()
    }
}

private final class StudioVideoResourceLoader: NSObject, AVAssetResourceLoaderDelegate, @unchecked Sendable {
    let queue = DispatchQueue(label: "sh.zeron.studio-video-loader", qos: .userInitiated)

    private let mimeType: String
    private let declaredSize: UInt64
    private let readChunk: @Sendable (UInt64) async throws -> StudioArtifactBytes
    private let cache = StudioVideoChunkCache(countLimit: 24)
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
        requests[key] = Task { [weak self, weak loadingRequest] in
            guard let self, let loadingRequest else { return }
            do {
                var offset = UInt64(max(dataRequest.currentOffset, dataRequest.requestedOffset))
                let requestedLength = UInt64(max(0, dataRequest.requestedLength))
                let requestedEnd = min(declaredSize, offset.saturatingAdd(requestedLength))

                while offset < requestedEnd {
                    try Task.checkCancellation()
                    let chunk = try await cache.chunk(at: offset, load: readChunk)
                    let remaining = requestedEnd - offset
                    let count = min(chunk.data.count, Int(clamping: remaining))
                    guard count > 0 else { break }
                    dataRequest.respond(with: chunk.data.prefix(count))
                    offset += UInt64(count)
                    if chunk.done { break }
                }
                loadingRequest.finishLoading()
            } catch is CancellationError {
                loadingRequest.finishLoading(with: CancellationError())
            } catch {
                loadingRequest.finishLoading(with: error)
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
