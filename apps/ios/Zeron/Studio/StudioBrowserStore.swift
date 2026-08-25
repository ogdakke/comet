import Foundation
import ImageIO
import Observation
import UIKit

private actor StudioPreviewGate {
    private let limit: Int
    private var active = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(limit: Int) {
        self.limit = limit
    }

    func load(
        artifactId: String,
        deviceId: String,
        workspace: WorkspaceStore
    ) async throws -> Data {
        await acquire()
        defer { release() }
        try Task.checkCancellation()
        return try await workspace.readStudioPreview(
            deviceId: deviceId,
            artifactId: artifactId
        )
    }

    private func acquire() async {
        if active < limit {
            active += 1
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    private func release() {
        if waiters.isEmpty {
            active -= 1
        } else {
            waiters.removeFirst().resume()
        }
    }
}

@MainActor
@Observable
final class StudioBrowserStore {
    var selectedDeviceId: String?
    var threads: [StudioThreadSummary] = []
    var gallery: [StudioGalleryItem] = []
    var threadsLoading = false
    var galleryLoading = false
    var galleryLoadingMore = false
    var threadsError: String?
    var galleryError: String?
    var reloadGeneration = 0

    @ObservationIgnored private let previews = NSCache<NSString, UIImage>()
    @ObservationIgnored private let previewGate = StudioPreviewGate(limit: 4)
    @ObservationIgnored private var previewTasks: [String: Task<UIImage?, Never>] = [:]
    @ObservationIgnored private var previewTaskGenerations: [String: UUID] = [:]
    @ObservationIgnored private var previewDemand: [String: Int] = [:]
    @ObservationIgnored private var galleryCursor: StudioGalleryCursor?

    private let galleryPageSize = 60

    init() {
        previews.countLimit = 160
        previews.totalCostLimit = 64 * 1024 * 1024
    }

    func resolveDevice(from devices: [DeviceRow], online: (String) -> Bool) {
        if let selectedDeviceId,
           devices.contains(where: { $0.id == selectedDeviceId && online($0.id) }) {
            return
        }
        let remembered = UserDefaults.standard.string(forKey: "studioDeviceId")
        selectedDeviceId = devices.first(where: { $0.id == remembered && online($0.id) })?.id
            ?? devices.first(where: { online($0.id) })?.id
            ?? devices.first?.id
    }

    func selectDevice(_ deviceId: String) {
        guard selectedDeviceId != deviceId else { return }
        selectedDeviceId = deviceId
        UserDefaults.standard.set(deviceId, forKey: "studioDeviceId")
        threads = []
        gallery = []
        galleryCursor = nil
        threadsError = nil
        galleryError = nil
        previews.removeAllObjects()
        previewTasks.values.forEach { $0.cancel() }
        previewTasks.removeAll()
        previewTaskGenerations.removeAll()
        previewDemand.removeAll()
        reloadGeneration += 1
    }

    func reload() {
        threadsError = nil
        galleryError = nil
        reloadGeneration += 1
    }

    func watchThreads(workspace: WorkspaceStore, deviceId: String) async {
        threadsLoading = true
        threadsError = nil
        do {
            let stream = try await workspace.watchStudioThreads(
                deviceId: deviceId,
                includeArchived: true
            )
            for try await response in stream {
                guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
                threads = response.conversations.sorted {
                    if $0.updatedDate != $1.updatedDate { return $0.updatedDate > $1.updatedDate }
                    return $0.id < $1.id
                }
                threadsLoading = false
            }
        } catch {
            guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
            threadsLoading = false
            threadsError = error.localizedDescription
        }
    }

    func watchGallery(workspace: WorkspaceStore, deviceId: String) async {
        galleryLoading = true
        galleryError = nil
        do {
            let stream = try await workspace.watchStudioGallery(
                deviceId: deviceId,
                pageSize: galleryPageSize
            )
            for try await response in stream {
                guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
                let firstPage = response.artifacts.sorted {
                    if $0.createdDate != $1.createdDate { return $0.createdDate > $1.createdDate }
                    return $0.id < $1.id
                }.reversed()
                if gallery.count <= galleryPageSize {
                    gallery = Array(firstPage)
                } else if let firstNewest = firstPage.first,
                          let anchorIndex = gallery.firstIndex(where: { $0.id == firstNewest.id }) {
                    gallery = Array(gallery[..<anchorIndex]) + Array(firstPage)
                } else {
                    let incomingIds = Set(firstPage.map(\.id))
                    let older = gallery.filter {
                        $0.createdDate < (firstPage.first?.createdDate ?? .distantPast)
                            && !incomingIds.contains($0.id)
                    }
                    gallery = older + Array(firstPage)
                }
                galleryCursor = response.nextCursor
                galleryLoading = false
            }
        } catch {
            guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
            galleryLoading = false
            galleryError = error.localizedDescription
        }
    }

    func loadMoreGallery(workspace: WorkspaceStore, deviceId: String) async {
        guard selectedDeviceId == deviceId,
              let cursor = galleryCursor,
              !galleryLoadingMore else { return }
        galleryLoadingMore = true
        defer { galleryLoadingMore = false }
        do {
            let response = try await workspace.listStudioGalleryPage(
                deviceId: deviceId,
                pageSize: galleryPageSize,
                cursor: cursor
            )
            guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
            let existing = Set(gallery.map(\.id))
            let older = response.artifacts
                .filter { !existing.contains($0.id) }
                .sorted {
                    if $0.createdDate != $1.createdDate { return $0.createdDate < $1.createdDate }
                    return $0.id < $1.id
                }
            gallery.insert(contentsOf: older, at: 0)
            galleryCursor = response.nextCursor
        } catch {
            guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
            galleryError = error.localizedDescription
        }
    }

    func shouldLoadMore(after item: StudioGalleryItem) -> Bool {
        galleryCursor != nil && gallery.prefix(12).contains(where: { $0.id == item.id })
    }

    func removeArtifact(_ artifactId: String) {
        gallery.removeAll { $0.id == artifactId }
        previews.removeObject(forKey: artifactId as NSString)
        previewTasks[artifactId]?.cancel()
        previewTasks.removeValue(forKey: artifactId)
        previewTaskGenerations.removeValue(forKey: artifactId)
        previewDemand.removeValue(forKey: artifactId)
    }

    func cachedPreview(artifactId: String) -> UIImage? {
        previews.object(forKey: artifactId as NSString)
    }

    func preview(
        artifactId: String,
        deviceId: String,
        workspace: WorkspaceStore
    ) async -> UIImage? {
        let request = beginPreviewRequest(
            artifactId: artifactId,
            deviceId: deviceId,
            workspace: workspace
        )
        defer { endPreviewRequest(artifactId: artifactId) }
        return await request.value
    }

    func beginPreviewRequest(
        artifactId: String,
        deviceId: String,
        workspace: WorkspaceStore
    ) -> Task<UIImage?, Never> {
        let key = artifactId as NSString
        if let image = previews.object(forKey: key) {
            return Task { image }
        }
        previewDemand[artifactId, default: 0] += 1
        if let task = previewTasks[artifactId] { return task }

        let generation = UUID()
        let task = Task<UIImage?, Never> {
            guard let data = try? await previewGate.load(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            ) else { return nil }
            let image = await Task.detached(priority: .utility) {
                Self.downsample(data: data, maximumPixelSize: 448)
            }.value
            guard !Task.isCancelled else { return nil }
            if let image {
                let cost = Int(image.size.width * image.scale * image.size.height * image.scale * 4)
                previews.setObject(image, forKey: key, cost: cost)
            }
            return image
        }
        previewTasks[artifactId] = task
        previewTaskGenerations[artifactId] = generation
        Task { [weak self] in
            _ = await task.value
            guard let self,
                  self.previewTaskGenerations[artifactId] == generation else { return }
            self.previewTasks.removeValue(forKey: artifactId)
            self.previewTaskGenerations.removeValue(forKey: artifactId)
        }
        return task
    }

    func endPreviewRequest(artifactId: String) {
        guard let count = previewDemand[artifactId] else { return }
        if count > 1 {
            previewDemand[artifactId] = count - 1
        } else {
            previewDemand.removeValue(forKey: artifactId)
            previewTasks[artifactId]?.cancel()
        }
    }

    private nonisolated static func downsample(data: Data, maximumPixelSize: Int) -> UIImage? {
        let options = [kCGImageSourceShouldCache: false] as CFDictionary
        guard let source = CGImageSourceCreateWithData(data as CFData, options) else { return nil }
        let thumbnailOptions = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maximumPixelSize,
            kCGImageSourceShouldCacheImmediately: true,
        ] as CFDictionary
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, thumbnailOptions) else {
            return nil
        }
        return UIImage(cgImage: image)
    }
}
