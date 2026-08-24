import Foundation
import Photos

enum StudioArtifactActionError: LocalizedError {
    case photoAccessDenied

    var errorDescription: String? {
        switch self {
        case .photoAccessDenied:
            "Allow photo additions in Settings to download Studio media."
        }
    }
}

@MainActor
enum StudioArtifactActions {
    static func download(
        _ artifact: StudioArtifactDetail,
        workspace: WorkspaceStore,
        deviceId: String,
        existingFile: URL? = nil
    ) async throws {
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

        let status = await PHPhotoLibrary.requestAuthorization(for: .addOnly)
        guard status == .authorized || status == .limited else {
            throw StudioArtifactActionError.photoAccessDenied
        }
        try await PHPhotoLibrary.shared().performChanges {
            let request = PHAssetCreationRequest.forAsset()
            request.addResource(
                with: artifact.mediaKind == .video ? .video : .photo,
                fileURL: fileURL,
                options: nil
            )
        }
    }
}
