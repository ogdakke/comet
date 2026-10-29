import Foundation
import Photos
import UniformTypeIdentifiers

enum StudioArtifactActionError: LocalizedError {
    case photoAccessDenied

    var errorDescription: String? {
        switch self {
        case .photoAccessDenied:
            "Allow Zeron to add to your photo library in Settings to save Studio media."
        }
    }
}

@MainActor
enum StudioArtifactActions {
    /// Save the original to Photos. Access is asked for before the original
    /// crosses the relay, so a refusal doesn't cost a long download.
    static func download(
        _ artifact: StudioArtifactDetail,
        workspace: StudioService,
        deviceId: String,
        existingFile: URL? = nil
    ) async throws {
        try await requestPhotoAddAccess()

        let downloaded: StudioDownloadedFile?
        let fileURL: URL
        if let existingFile {
            downloaded = nil
            fileURL = existingFile
        } else {
            let file = try await workspace.downloadStudioArtifact(
                deviceId: deviceId,
                artifactId: artifact.id,
                declaredSize: artifact.sizeBytes
            )
            downloaded = file
            fileURL = file.url
        }
        defer {
            if let downloaded { try? FileManager.default.removeItem(at: downloaded.url) }
        }

        let resourceType: PHAssetResourceType = artifact.mediaKind == .video ? .video : .photo
        // Photos infers the format from the extension unless told; the relay's
        // file name may not carry one.
        let typeIdentifier = UTType(mimeType: artifact.mimeType)?.identifier
        let originalName = downloaded?.fileName
        try await PHPhotoLibrary.shared().performChanges {
            let options = PHAssetResourceCreationOptions()
            options.uniformTypeIdentifier = typeIdentifier
            if let originalName, !originalName.isEmpty { options.originalFilename = originalName }
            PHAssetCreationRequest.forAsset().addResource(with: resourceType, fileURL: fileURL, options: options)
        }
    }

    /// The original under its own file name, for the share sheet. Remove it
    /// with `removeShareableFile` once the sheet is done.
    static func shareableFile(
        _ artifact: StudioArtifactDetail,
        workspace: StudioService,
        deviceId: String
    ) async throws -> URL {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "ZeronStudioShare", directoryHint: .isDirectory)
        // One share at a time; anything left here is from a sheet that never
        // reported back (the viewer closed under it).
        try? FileManager.default.removeItem(at: root)
        let file = try await workspace.downloadStudioArtifact(
            deviceId: deviceId,
            artifactId: artifact.id,
            declaredSize: artifact.sizeBytes
        )
        do {
            let directory = root.appending(path: UUID().uuidString, directoryHint: .isDirectory)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let destination = directory.appending(path: shareFileName(file, artifact: artifact))
            try FileManager.default.moveItem(at: file.url, to: destination)
            return destination
        } catch {
            try? FileManager.default.removeItem(at: file.url)
            throw error
        }
    }

    static func removeShareableFile(_ url: URL) {
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }

    private static func requestPhotoAddAccess() async throws {
        // Add-only access needs its own usage string; without it the prompt
        // would crash the app, so fall back to full access (whose string the
        // app already declares for attachments).
        let level: PHAccessLevel = Bundle.main.object(
            forInfoDictionaryKey: "NSPhotoLibraryAddUsageDescription"
        ) == nil ? .readWrite : .addOnly
        let status = await PHPhotoLibrary.requestAuthorization(for: level)
        guard status == .authorized || status == .limited else {
            throw StudioArtifactActionError.photoAccessDenied
        }
    }

    private static func shareFileName(_ file: StudioDownloadedFile, artifact: StudioArtifactDetail) -> String {
        let named = URL(filePath: file.fileName).lastPathComponent
        let base = named.isEmpty || named == "/" ? "Studio \(artifact.id.prefix(8))" : named
        guard URL(filePath: base).pathExtension.isEmpty,
              let ext = UTType(mimeType: artifact.mimeType)?.preferredFilenameExtension else { return base }
        return "\(base).\(ext)"
    }
}
