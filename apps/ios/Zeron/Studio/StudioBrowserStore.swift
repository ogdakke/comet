import Foundation
import ImageIO
import Observation
import UIKit

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
    @ObservationIgnored private let displayImages = NSCache<NSString, UIImage>()
    @ObservationIgnored private let thumbhashImages = NSCache<NSString, UIImage>()
    @ObservationIgnored private let previewDiskCache = StudioPreviewDiskCache.shared
    @ObservationIgnored private var previewTasks: [String: Task<UIImage?, Never>] = [:]
    @ObservationIgnored private var previewTaskGenerations: [String: UUID] = [:]
    @ObservationIgnored private var previewDemand: [String: Int] = [:]
    @ObservationIgnored private var displayTasks: [String: Task<UIImage?, Never>] = [:]
    @ObservationIgnored private var displayTaskGenerations: [String: UUID] = [:]
    @ObservationIgnored private var displayDemand: [String: Int] = [:]
    @ObservationIgnored private var displayPreheatReleaseTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var preheatedDisplayIds: Set<String> = []
    @ObservationIgnored private var galleryCursor: StudioGalleryCursor?

    private let galleryPageSize = 60

    init() {
        previews.countLimit = 160
        previews.totalCostLimit = 64 * 1024 * 1024
        displayImages.countLimit = 6
        displayImages.totalCostLimit = 144 * 1024 * 1024
        thumbhashImages.countLimit = 640
        thumbhashImages.totalCostLimit = 4 * 1024 * 1024
        Task { await previewDiskCache.prepare() }
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
        displayImages.removeAllObjects()
        thumbhashImages.removeAllObjects()
        previewTasks.values.forEach { $0.cancel() }
        displayTasks.values.forEach { $0.cancel() }
        displayPreheatReleaseTasks.values.forEach { $0.cancel() }
        previewTasks.removeAll()
        previewTaskGenerations.removeAll()
        previewDemand.removeAll()
        displayTasks.removeAll()
        displayTaskGenerations.removeAll()
        displayDemand.removeAll()
        displayPreheatReleaseTasks.removeAll()
        preheatedDisplayIds.removeAll()
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
                let firstPage = Self.galleryItemsOldestFirst(response.artifacts)
                if gallery.count <= galleryPageSize {
                    gallery = firstPage
                } else if let pageOldest = firstPage.first,
                          let anchorIndex = gallery.firstIndex(where: { $0.id == pageOldest.id }) {
                    gallery = Array(gallery[..<anchorIndex]) + firstPage
                } else {
                    let incomingIds = Set(firstPage.map(\.id))
                    let older = gallery.filter {
                        $0.createdDate < (firstPage.first?.createdDate ?? .distantPast)
                            && !incomingIds.contains($0.id)
                    }
                    gallery = older + firstPage
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
            let older = Self.galleryItemsOldestFirst(
                response.artifacts.filter { !existing.contains($0.id) }
            )
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

    nonisolated static func galleryItemsOldestFirst(
        _ items: [StudioGalleryItem]
    ) -> [StudioGalleryItem] {
        items.sorted {
            if $0.createdDate != $1.createdDate { return $0.createdDate < $1.createdDate }
            return $0.id < $1.id
        }
    }

    func removeArtifact(_ artifactId: String) {
        gallery.removeAll { $0.id == artifactId }
        previews.removeObject(forKey: artifactId as NSString)
        displayImages.removeObject(forKey: artifactId as NSString)
        previewTasks[artifactId]?.cancel()
        displayTasks[artifactId]?.cancel()
        displayPreheatReleaseTasks[artifactId]?.cancel()
        previewTasks.removeValue(forKey: artifactId)
        previewTaskGenerations.removeValue(forKey: artifactId)
        previewDemand.removeValue(forKey: artifactId)
        displayTasks.removeValue(forKey: artifactId)
        displayTaskGenerations.removeValue(forKey: artifactId)
        displayDemand.removeValue(forKey: artifactId)
        displayPreheatReleaseTasks.removeValue(forKey: artifactId)
        preheatedDisplayIds.remove(artifactId)
        if let deviceId = selectedDeviceId {
            Task { await previewDiskCache.remove(deviceId: deviceId, artifactId: artifactId) }
        }
    }

    func cachedPreview(artifactId: String) -> UIImage? {
        previews.object(forKey: artifactId as NSString)
    }

    func cachedDisplayImage(artifactId: String) -> UIImage? {
        displayImages.object(forKey: artifactId as NSString)
    }

    func thumbhashImage(
        _ thumbhash: String?,
        aspectRatio: CGFloat?
    ) -> UIImage? {
        guard let thumbhash else { return nil }
        let ratioKey = aspectRatio.map { String(format: "%.4f", Double($0)) } ?? "raw"
        let key = "\(thumbhash):\(ratioKey)" as NSString
        if let image = thumbhashImages.object(forKey: key) { return image }
        guard let image = StudioThumbHash.image(base64: thumbhash, aspectRatio: aspectRatio) else {
            return nil
        }
        let cost = Int(image.size.width * image.scale * image.size.height * image.scale * 4)
        thumbhashImages.setObject(image, forKey: key, cost: cost)
        return image
    }

#if DEBUG
    func seedPreviewForPerformanceFixture(_ image: UIImage, artifactId: String) {
        let cost = Int(image.size.width * image.scale * image.size.height * image.scale * 4)
        previews.setObject(image, forKey: artifactId as NSString, cost: cost)
    }
#endif

    /// Start the display-sized original on touch-down. A short hold lets the
    /// viewer claim the same request after UIKit delivers touch-up without
    /// leaving abandoned downloads alive after a cancelled press.
    func preheatDisplayImage(
        artifact: StudioArtifactDetail,
        deviceId: String,
        workspace: WorkspaceStore
    ) {
        guard artifact.mediaKind == .image,
              cachedDisplayImage(artifactId: artifact.id) == nil else { return }

        let artifactId = artifact.id
        if preheatedDisplayIds.insert(artifactId).inserted {
            _ = beginDisplayImageRequest(
                artifact: artifact,
                deviceId: deviceId,
                workspace: workspace
            )
        }
        displayPreheatReleaseTasks[artifactId]?.cancel()
        displayPreheatReleaseTasks[artifactId] = Task { [weak self] in
            try? await Task.sleep(for: .seconds(1.5))
            guard !Task.isCancelled, let self else { return }
            self.preheatedDisplayIds.remove(artifactId)
            self.displayPreheatReleaseTasks.removeValue(forKey: artifactId)
            self.endDisplayImageRequest(artifactId: artifactId)
        }
    }

    func beginDisplayImageRequest(
        artifact: StudioArtifactDetail,
        deviceId: String,
        workspace: WorkspaceStore
    ) -> Task<UIImage?, Never> {
        let artifactId = artifact.id
        let key = artifactId as NSString
        if let image = displayImages.object(forKey: key) {
            return Task { image }
        }
        displayDemand[artifactId, default: 0] += 1
        if let task = displayTasks[artifactId] { return task }

        let generation = UUID()
        let task = Task<UIImage?, Never>(priority: .userInitiated) {
            do {
                let file = try await workspace.downloadStudioArtifact(
                    deviceId: deviceId,
                    artifactId: artifactId,
                    declaredSize: artifact.sizeBytes
                )
                defer { try? FileManager.default.removeItem(at: file.url) }
                guard !Task.isCancelled else { return nil }

                let longestEdge = max(Int(artifact.width ?? 0), Int(artifact.height ?? 0))
                let maximumPixelSize = min(max(longestEdge, 2_048), 3_072)
                let image = await Task.detached(priority: .userInitiated) {
                    Self.decodeImage(at: file.url, maximumPixelSize: maximumPixelSize)
                }.value
                guard !Task.isCancelled else { return nil }
                if let image {
                    let cost = Int(image.size.width * image.scale * image.size.height * image.scale * 4)
                    displayImages.setObject(image, forKey: key, cost: cost)
                }
                return image
            } catch {
                return nil
            }
        }
        displayTasks[artifactId] = task
        displayTaskGenerations[artifactId] = generation
        Task { [weak self] in
            _ = await task.value
            guard let self,
                  self.displayTaskGenerations[artifactId] == generation else { return }
            self.displayTasks.removeValue(forKey: artifactId)
            self.displayTaskGenerations.removeValue(forKey: artifactId)
        }
        return task
    }

    func endDisplayImageRequest(artifactId: String) {
        guard let count = displayDemand[artifactId] else { return }
        if count > 1 {
            displayDemand[artifactId] = count - 1
        } else {
            displayDemand.removeValue(forKey: artifactId)
            displayTasks[artifactId]?.cancel()
        }
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
            if let diskData = await previewDiskCache.data(deviceId: deviceId, artifactId: artifactId) {
                let diskImage = await Task.detached(priority: .utility) {
                    Self.downsample(data: diskData, maximumPixelSize: 448)
                }.value
                guard !Task.isCancelled else { return nil }
                if let diskImage {
                    let cost = Int(
                        diskImage.size.width * diskImage.scale
                            * diskImage.size.height * diskImage.scale * 4
                    )
                    previews.setObject(diskImage, forKey: key, cost: cost)
                    return diskImage
                }
                await previewDiskCache.remove(deviceId: deviceId, artifactId: artifactId)
            }

            guard let relayData = try? await workspace.readStudioPreview(
                deviceId: deviceId,
                artifactId: artifactId
            ) else { return nil }
            let relayImage = await Task.detached(priority: .utility) {
                Self.downsample(data: relayData, maximumPixelSize: 448)
            }.value
            guard !Task.isCancelled else { return nil }
            if let relayImage {
                let cost = Int(
                    relayImage.size.width * relayImage.scale
                        * relayImage.size.height * relayImage.scale * 4
                )
                previews.setObject(relayImage, forKey: key, cost: cost)
                await previewDiskCache.store(relayData, deviceId: deviceId, artifactId: artifactId)
            }
            return relayImage
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

    private nonisolated static func decodeImage(
        at url: URL,
        maximumPixelSize: Int
    ) -> UIImage? {
        let sourceOptions = [kCGImageSourceShouldCache: false] as CFDictionary
        guard let source = CGImageSourceCreateWithURL(url as CFURL, sourceOptions) else {
            return nil
        }
        let options = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maximumPixelSize,
            kCGImageSourceShouldCacheImmediately: true,
        ] as CFDictionary
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options) else {
            return nil
        }
        return UIImage(cgImage: image)
    }
}
