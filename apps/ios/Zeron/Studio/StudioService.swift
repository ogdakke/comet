import CryptoKit
import Foundation

/// Studio on a desktop's engine, over the core's Studio passthrough
/// (`CoreClient.studioCall` / `studioWatch`): the core carries JSON, this
/// side owns the wire models.
@MainActor
final class StudioService {
    private let client: CoreClient
    private let mediaGate = StudioMediaRequestGate()

    init(client: CoreClient) {
        self.client = client
    }

    /// Desktops that can host Studio, by their registry rows.
    var hosts: [DeviceView] {
        client.devices().filter { !$0.isSelf && ["macos", "linux", "windows"].contains($0.platform) }
    }

    func isOnline(_ deviceId: String) -> Bool {
        hosts.first { $0.id == deviceId }?.online ?? false
    }

    // MARK: Calls

    func listStudioProviders(deviceId: String) async throws -> [StudioProviderConnection] {
        let response: StudioProviderListResponse = try await call(deviceId, "ListStudioProviders", [:])
        return response.providers
    }

    /// Store `secret` for `providerId` on the desktop (its credential store,
    /// never this phone's), then check it against the provider.
    func setStudioProviderCredential(
        deviceId: String,
        providerId: String,
        displayLabel: String,
        secret: String
    ) async throws -> StudioProviderConnection {
        let _: StudioProviderConnection = try await call(deviceId, "SetStudioProviderCredential", [
            "providerId": providerId, "displayLabel": displayLabel, "secret": secret,
        ])
        return try await call(deviceId, "ValidateStudioProvider", ["providerId": providerId])
    }

    func removeStudioProviderCredential(deviceId: String, providerId: String) async throws {
        let _: JSONValue = try await call(deviceId, "RemoveStudioProviderCredential", ["providerId": providerId])
    }

    func listStudioModels(
        deviceId: String,
        providerId: String,
        refresh: Bool = false
    ) async throws -> StudioModelListResponse {
        try await call(deviceId, "ListStudioModels", ["providerId": providerId, "refresh": refresh])
    }

    func evaluateStudioComposer(
        deviceId: String,
        snapshot: StudioComposerSnapshot,
        providerId: String
    ) async throws -> StudioComposerEvaluation {
        try await call(deviceId, "EvaluateStudioComposer", [
            "composer": try Self.jsonObject(snapshot),
            "providerId": providerId,
        ])
    }

    func importStudioAsset(
        deviceId: String,
        assetId: String,
        data: Data,
        mimeType: String
    ) async throws -> StudioComposerAttachment {
        guard !data.isEmpty else { throw StudioError.message("The selected Studio attachment is empty") }
        guard data.count <= 64 * 1024 * 1024 else {
            throw StudioError.message("Studio attachments must be 64 MB or smaller")
        }
        let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        let chunkSize = 256 * 1024
        var offset = 0
        while offset < data.count {
            try Task.checkCancellation()
            let end = min(offset + chunkSize, data.count)
            let last = end == data.count
            var params: [String: Any] = [
                "assetId": assetId,
                "offset": offset,
                "data": data[offset..<end].base64EncodedString(),
                "last": last,
                "mimeHint": mimeType,
            ]
            if last { params["expectedHash"] = digest }
            let response: JSONValue = try await call(deviceId, "ImportStudioAsset", params)
            if last {
                let encoded = try JSONEncoder().encode(response)
                return try JSONDecoder().decode(StudioComposerAttachment.self, from: encoded)
            }
            guard let nextOffset = response.objectValue?["nextOffset"]?.int64Value,
                  nextOffset > Int64(offset), nextOffset <= Int64(data.count) else {
                throw StudioError.message("Studio attachment upload stopped advancing")
            }
            offset = Int(nextOffset)
        }
        throw StudioError.message("Studio did not commit the attachment")
    }

    /// Starts the turn's runs; the host answers with the whole thread as it
    /// stands (`conversation_view`), the new turn's runs still pending.
    func createStudioTurn(
        deviceId: String,
        snapshot: StudioComposerSnapshot
    ) async throws -> StudioThread {
        let request = StudioCreateTurnRequest(
            conversationId: snapshot.conversationId ?? "",
            prompt: snapshot.prompt,
            sourceTurnId: snapshot.sourceTurnId,
            composer: snapshot
        )
        return try await call(deviceId, "CreateStudioTurn", try Self.jsonObject(request))
    }

    /// Run a failed run again; the host answers with the thread, the run
    /// queued. `retryAnyway` for a run whose submission the host can't
    /// confirm never reached the provider (it may bill twice).
    func retryStudioRun(deviceId: String, runId: String, retryAnyway: Bool = false) async throws -> StudioThread {
        try await call(deviceId, "RetryStudioRun", ["runId": runId, "retryAnyway": retryAnyway])
    }

    func listStudioGalleryPage(
        deviceId: String,
        pageSize: Int,
        cursor: StudioGalleryCursor
    ) async throws -> StudioGalleryResponse {
        // Decoded off the main thread: a page is a hundred-odd items.
        let reply = try await Self.rawCall(client, deviceId, "ListStudioArtifacts", [
            "limit": pageSize,
            "cursor": ["createdAt": cursor.createdAt, "artifactId": cursor.artifactId],
        ])
        return try await Task.detached(priority: .userInitiated) {
            try JSONDecoder().decode(StudioGalleryResponse.self, from: Data(reply.utf8))
        }.value
    }

    func markStudioThreadSeen(deviceId: String, threadId: String) async {
        let _: JSONValue? = try? await call(deviceId, "MarkStudioConversationSeen", ["conversationId": threadId])
    }

    func renameStudioThread(deviceId: String, threadId: String, title: String) async throws {
        let _: StudioThreadSummary = try await call(deviceId, "RenameStudioConversation", ["conversationId": threadId, "title": title])
    }

    func archiveStudioThread(deviceId: String, threadId: String, archived: Bool) async throws {
        let _: StudioThreadSummary = try await call(deviceId, "ArchiveStudioConversation", ["conversationId": threadId, "archived": archived])
    }

    func deleteStudioThread(deviceId: String, threadId: String) async throws {
        let _: JSONValue = try await call(deviceId, "DeleteStudioConversation", ["conversationId": threadId])
    }

    func deleteStudioArtifact(deviceId: String, artifactId: String) async throws {
        struct Response: Decodable { let ok: Bool }
        let response: Response = try await call(deviceId, "DeleteStudioArtifact", ["artifactId": artifactId])
        guard response.ok else { throw StudioError.message("Studio didn't delete the artifact") }
    }

    // MARK: Streams

    func watchStudioThreads(
        deviceId: String,
        includeArchived: Bool = false
    ) throws -> AsyncThrowingStream<StudioThreadListResponse, Error> {
        try watch(deviceId, "WatchStudioConversations", ["includeArchived": includeArchived])
    }

    func watchStudioGallery(
        deviceId: String,
        pageSize: Int
    ) throws -> AsyncThrowingStream<StudioGalleryResponse, Error> {
        try watch(deviceId, "WatchStudioGallery", ["limit": pageSize])
    }

    func watchStudioThread(
        deviceId: String,
        threadId: String
    ) throws -> AsyncThrowingStream<StudioThread, Error> {
        try watch(deviceId, "WatchStudioConversation", ["conversationId": threadId])
    }

    // MARK: Media

    /// Preview reads stay bounded. Originals come only through
    /// `downloadStudioArtifact`, one at a time, into a temporary file.
    /// Off the main thread: each chunk is a base64 string in JSON, and a
    /// gallery fling reads dozens of them.
    nonisolated func readStudioPreview(
        deviceId: String,
        artifactId: String,
        priority: StudioMediaRequestPriority = .visible
    ) async throws -> Data {
        let gate = mediaGate
        let held: StudioMediaRequestPriority = priority == .foreground ? .foreground : .visible
        try await gate.acquire(priority: priority, key: artifactId)
        defer { Task { await gate.release(priority: held) } }
        var result = Data()
        var offset: UInt64 = 0
        repeat {
            try Task.checkCancellation()
            let reply = try await Self.rawCall(
                client, deviceId, "ReadStudioPreviewChunk", ["artifactId": artifactId, "offset": offset]
            )
            let chunk = try JSONDecoder().decode(StudioArtifactChunk.self, from: Data(reply.utf8))
            guard chunk.artifactId == artifactId,
                  chunk.nextOffset >= offset,
                  let bytes = Data(base64Encoded: chunk.data) else {
                throw StudioError.message("Studio returned an invalid media chunk")
            }
            guard result.count + bytes.count <= 8 * 1024 * 1024 else {
                throw StudioError.message("Studio media exceeds the mobile size limit")
            }
            result.append(bytes)
            if chunk.done { return result }
            guard chunk.nextOffset > offset else {
                throw StudioError.message("Studio media read did not advance")
            }
            offset = chunk.nextOffset
        } while true
    }

    /// A square came on screen: its preview read goes ahead of prefetch.
    nonisolated func promotePreview(artifactId: String) {
        let gate = mediaGate
        Task { await gate.promote(key: artifactId) }
    }

    func readStudioArtifactChunk(
        deviceId: String,
        artifactId: String,
        offset: UInt64
    ) async throws -> StudioArtifactBytes {
        try await mediaGate.acquire(priority: .foreground)
        defer { Task { await mediaGate.release(priority: .foreground) } }
        let chunk: StudioArtifactChunk = try await call(
            deviceId, "ReadStudioArtifactChunk", ["artifactId": artifactId, "offset": offset]
        )
        guard chunk.artifactId == artifactId,
              chunk.nextOffset >= offset,
              let bytes = Data(base64Encoded: chunk.data),
              bytes.count <= 512 * 1024 else {
            throw StudioError.message("Studio returned an invalid artifact chunk")
        }
        if !chunk.done, chunk.nextOffset == offset {
            throw StudioError.message("Studio artifact read did not advance")
        }
        return StudioArtifactBytes(
            fileName: chunk.fileName,
            mimeType: chunk.mimeType,
            data: bytes,
            nextOffset: chunk.nextOffset,
            done: chunk.done
        )
    }

    /// Stream an original artifact to one short-lived temporary file, so
    /// videos never accumulate in RAM. Callers remove the file when done.
    func downloadStudioArtifact(
        deviceId: String,
        artifactId: String,
        declaredSize: UInt64
    ) async throws -> StudioDownloadedFile {
        // Allow a small metadata discrepancy, but don't let an unbounded
        // response consume the phone's temporary storage.
        let allowance = max(declaredSize / 20, 1_048_576)
        let (maximumBytes, overflow) = declaredSize.addingReportingOverflow(allowance)
        guard !overflow else { throw StudioError.message("Studio reported an invalid artifact size") }
        try await mediaGate.acquire(priority: .foreground)
        defer { Task { await mediaGate.release(priority: .foreground) } }

        let directory = FileManager.default.temporaryDirectory
            .appending(path: "ZeronStudioViewer", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var outputURL: URL?
        var handle: FileHandle?
        var written: UInt64 = 0
        var offset: UInt64 = 0
        do {
            repeat {
                try Task.checkCancellation()
                let chunk: StudioArtifactChunk = try await call(
                    deviceId, "ReadStudioArtifactChunk", ["artifactId": artifactId, "offset": offset]
                )
                guard chunk.artifactId == artifactId,
                      chunk.nextOffset >= offset,
                      let bytes = Data(base64Encoded: chunk.data) else {
                    throw StudioError.message("Studio returned an invalid artifact chunk")
                }
                guard written + UInt64(bytes.count) <= maximumBytes else {
                    throw StudioError.message("Studio artifact exceeds its declared size")
                }
                if outputURL == nil {
                    let ext = URL(filePath: chunk.fileName).pathExtension
                    let url = directory.appending(path: UUID().uuidString + (ext.isEmpty ? "" : ".\(ext)"))
                    guard FileManager.default.createFile(atPath: url.path, contents: nil) else {
                        throw StudioError.message("Couldn't create a temporary Studio file")
                    }
                    outputURL = url
                    handle = try FileHandle(forWritingTo: url)
                }
                try handle?.write(contentsOf: bytes)
                written += UInt64(bytes.count)
                if chunk.done {
                    try handle?.close()
                    guard let outputURL else { throw StudioError.message("Studio returned an empty artifact") }
                    return StudioDownloadedFile(url: outputURL, fileName: chunk.fileName, mimeType: chunk.mimeType)
                }
                guard chunk.nextOffset > offset else {
                    throw StudioError.message("Studio artifact read did not advance")
                }
                offset = chunk.nextOffset
            } while true
        } catch {
            try? handle?.close()
            if let outputURL { try? FileManager.default.removeItem(at: outputURL) }
            throw error
        }
    }

    // MARK: Transport

    private func call<Response: Decodable>(
        _ deviceId: String,
        _ method: String,
        _ params: [String: Any]
    ) async throws -> Response {
        let body = try Self.encode(params)
        let reply: String
        do {
            reply = try await client.studioCall(deviceId: deviceId, method: method, paramsJson: body)
        } catch let error as CoreError {
            throw StudioError(error)
        }
        return try JSONDecoder().decode(Response.self, from: Data(reply.utf8))
    }

    /// `call` without the main actor, for media reads.
    private nonisolated static func rawCall(
        _ client: CoreClient,
        _ deviceId: String,
        _ method: String,
        _ params: [String: Any]
    ) async throws -> String {
        let body = try encode(params)
        do {
            return try await client.studioCall(deviceId: deviceId, method: method, paramsJson: body)
        } catch let error as CoreError {
            throw StudioError(error)
        }
    }

    private func watch<Item: Decodable & Sendable>(
        _ deviceId: String,
        _ method: String,
        _ params: [String: Any]
    ) throws -> AsyncThrowingStream<Item, Error> {
        let body = try Self.encode(params)
        let (stream, continuation) = AsyncThrowingStream<Item, Error>.makeStream()
        let sink = StreamSink { json in
            do {
                continuation.yield(try JSONDecoder().decode(Item.self, from: Data(json.utf8)))
            } catch {
                continuation.finish(throwing: error)
            }
        } end: { error in
            continuation.finish(throwing: error.map(StudioError.message))
        }
        let handle: StudioStream
        do {
            handle = try client.studioWatch(deviceId: deviceId, method: method, paramsJson: body, sink: sink)
        } catch let error as CoreError {
            throw StudioError(error)
        }
        continuation.onTermination = { _ in handle.cancel() }
        return stream
    }

    private nonisolated static func encode(_ params: [String: Any]) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: params)
        return String(decoding: data, as: UTF8.self)
    }

    private nonisolated static func jsonObject<T: Encodable>(_ value: T) throws -> [String: Any] {
        let data = try JSONEncoder().encode(value)
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw StudioError.message("Studio request could not be encoded")
        }
        return object
    }
}

/// A Studio stream's items, on a core thread.
private final class StreamSink: StudioStreamSink, @unchecked Sendable {
    private let item: (String) -> Void
    private let end: (String?) -> Void

    init(item: @escaping (String) -> Void, end: @escaping (String?) -> Void) {
        self.item = item
        self.end = end
    }

    func onItem(json: String) { item(json) }
    func onEnd(error: String?) { end(error) }
}

enum StudioError: LocalizedError {
    case message(String)
    /// The desktop's engine has no Studio (or too old a one for the call).
    case unsupported(String)

    init(_ error: CoreError) {
        switch error {
        case .Unsupported(let message):
            self = .unsupported(message)
        case .NotFound(let message), .InvalidArgument(let message),
             .Network(let message), .HostError(let message), .Auth(let message),
             .Storage(let message), .NotImplemented(let message), .Internal(let message):
            self = .message(message)
        case .HostUnavailable:
            self = .message("The desktop is offline")
        case .Closed:
            self = .message("Not connected")
        }
    }

    var errorDescription: String? {
        switch self {
        case .message(let message), .unsupported(let message): message
        }
    }
}

struct StudioDownloadedFile: Sendable {
    let url: URL
    let fileName: String
    let mimeType: String
}

enum StudioMediaRequestPriority: Sendable {
    case foreground
    /// A preview on screen now.
    case visible
    /// A preview ahead of the scroll.
    case prefetch
}
