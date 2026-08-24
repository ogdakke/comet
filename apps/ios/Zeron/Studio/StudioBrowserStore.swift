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
    var threadsError: String?
    var galleryError: String?
    var reloadGeneration = 0

    @ObservationIgnored private let previews = NSCache<NSString, UIImage>()
    @ObservationIgnored private var previewTasks: [String: Task<UIImage?, Never>] = [:]

    init() {
        previews.countLimit = 48
        previews.totalCostLimit = 24 * 1024 * 1024
    }

    func resolveDevice(from devices: [DeviceRow], online: (String) -> Bool) {
        if let selectedDeviceId, devices.contains(where: { $0.id == selectedDeviceId }) {
            return
        }
        let remembered = UserDefaults.standard.string(forKey: "studioDeviceId")
        selectedDeviceId = devices.first(where: { $0.id == remembered && online($0.id) })?.id
            ?? devices.first(where: { online($0.id) })?.id
            ?? devices.first?.id
    }

    func selectDevice(_ id: String) {
        guard selectedDeviceId != id else { return }
        selectedDeviceId = id
        UserDefaults.standard.set(id, forKey: "studioDeviceId")
        threads = []
        gallery = []
        threadsError = nil
        galleryError = nil
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
            let stream = try await workspace.watchStudioGallery(deviceId: deviceId)
            for try await response in stream {
                guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
                gallery = response.artifacts.sorted {
                    if $0.createdDate != $1.createdDate { return $0.createdDate > $1.createdDate }
                    return $0.id < $1.id
                }
                galleryLoading = false
            }
        } catch {
            guard !Task.isCancelled, selectedDeviceId == deviceId else { return }
            galleryLoading = false
            galleryError = error.localizedDescription
        }
    }

    func preview(
        artifactId: String,
        deviceId: String,
        workspace: WorkspaceStore
    ) async -> UIImage? {
        let key = artifactId as NSString
        if let image = previews.object(forKey: key) { return image }
        if let task = previewTasks[artifactId] { return await task.value }

        let task = Task<UIImage?, Never> {
            guard let data = try? await workspace.readStudioPreview(
                deviceId: deviceId,
                artifactId: artifactId
            ) else { return nil }
            return Self.downsample(data: data, maximumPixelSize: 768)
        }
        previewTasks[artifactId] = task
        let image = await task.value
        previewTasks.removeValue(forKey: artifactId)
        if let image {
            let cost = Int(image.size.width * image.scale * image.size.height * image.scale * 4)
            previews.setObject(image, forKey: key, cost: cost)
        }
        return image
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
