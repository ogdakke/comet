import CryptoKit
import Foundation

actor StudioPreviewDiskCache {
    static let shared = StudioPreviewDiskCache()
    static let maximumBytes: Int64 = 512 * 1024 * 1024
    static let maximumFileCount = 12_000

    private let fileManager = FileManager.default
    private let root: URL
    private var prepared = false
    private var writesSinceTrim = 0

    init() {
        root = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appending(path: "StudioPreviews", directoryHint: .isDirectory)
            .appending(path: "v1", directoryHint: .isDirectory)
    }

    func data(deviceId: String, artifactId: String) -> Data? {
        prepareIfNeeded()
        return try? Data(contentsOf: url(deviceId: deviceId, artifactId: artifactId), options: .mappedIfSafe)
    }

    func store(_ data: Data, deviceId: String, artifactId: String) {
        guard !data.isEmpty, data.count <= 8 * 1024 * 1024 else { return }
        prepareIfNeeded()
        do {
            try data.write(to: url(deviceId: deviceId, artifactId: artifactId), options: .atomic)
            writesSinceTrim += 1
            if writesSinceTrim >= 64 {
                writesSinceTrim = 0
                trim()
            }
        } catch {
            // The cache is disposable. A failed write must not make the relay
            // preview unavailable to the current request.
        }
    }

    func remove(deviceId: String, artifactId: String) {
        prepareIfNeeded()
        try? fileManager.removeItem(at: url(deviceId: deviceId, artifactId: artifactId))
    }

    func prepare() {
        prepareIfNeeded()
        trim()
    }

    private func prepareIfNeeded() {
        guard !prepared else { return }
        prepared = true
        try? fileManager.createDirectory(at: root, withIntermediateDirectories: true)
    }

    private func url(deviceId: String, artifactId: String) -> URL {
        let digest = SHA256.hash(data: Data("\(deviceId):\(artifactId)".utf8))
        let name = digest.map { String(format: "%02x", $0) }.joined()
        return root.appending(path: name, directoryHint: .notDirectory)
    }

    private func trim() {
        guard let urls = try? fileManager.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: [.fileSizeKey, .contentModificationDateKey, .isRegularFileKey],
            options: [.skipsHiddenFiles]
        ) else { return }

        var entries: [(url: URL, size: Int64, date: Date)] = []
        entries.reserveCapacity(urls.count)
        var totalBytes: Int64 = 0
        for url in urls {
            guard let values = try? url.resourceValues(forKeys: [
                .fileSizeKey,
                .contentModificationDateKey,
                .isRegularFileKey,
            ]), values.isRegularFile == true else { continue }
            let size = Int64(values.fileSize ?? 0)
            entries.append((url, size, values.contentModificationDate ?? .distantPast))
            totalBytes += size
        }

        guard entries.count > Self.maximumFileCount || totalBytes > Self.maximumBytes else { return }
        entries.sort { $0.date < $1.date }
        var remainingCount = entries.count
        for entry in entries {
            guard remainingCount > Self.maximumFileCount || totalBytes > Self.maximumBytes else { break }
            if (try? fileManager.removeItem(at: entry.url)) != nil {
                remainingCount -= 1
                totalBytes -= entry.size
            }
        }
    }
}
