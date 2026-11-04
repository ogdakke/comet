import AVFoundation
import PhotosUI
import UIKit
import UniformTypeIdentifiers

/// Presents the attach menu (Photos / Camera / Files / Paste) and turns picks
/// into `StagedImage`s. Images: HEIC→JPEG, long side ≤ 2560px, ≤ 24 MB.
/// Videos and other files stay as they are, staged by path in a temp copy.
final class AttachmentPicker: NSObject, PHPickerViewControllerDelegate, UIImagePickerControllerDelegate, UINavigationControllerDelegate, UIDocumentPickerDelegate {
    private weak var host: UIViewController?
    private let completion: ([StagedImage]) -> Void
    private var retainSelf: AttachmentPicker?

    init(host: UIViewController, completion: @escaping ([StagedImage]) -> Void) {
        self.host = host
        self.completion = completion
    }

    static func menu(host: UIViewController, limit: Int, completion: @escaping ([StagedImage]) -> Void) -> UIMenu {
        var items: [UIMenuElement] = [
            UIAction(title: "Photo Library", image: UIImage(systemName: "photo.on.rectangle")) { _ in
                AttachmentPicker(host: host, completion: completion).presentPhotos(limit: limit)
            },
        ]
        if UIImagePickerController.isSourceTypeAvailable(.camera) {
            items.append(UIAction(title: "Take Photo", image: UIImage(systemName: "camera")) { _ in
                AttachmentPicker(host: host, completion: completion).presentCamera()
            })
        }
        items.append(UIAction(title: "Choose File", image: UIImage(systemName: "folder")) { _ in
            AttachmentPicker(host: host, completion: completion).presentFiles()
        })
        if UIPasteboard.general.hasImages {
            items.append(UIAction(title: "Paste Image", image: UIImage(systemName: "doc.on.clipboard")) { _ in
                let staged = UIPasteboard.general.images?.compactMap { AttachmentPicker.stage(image: $0) } ?? []
                completion(staged)
            })
        }
        return UIMenu(children: items)
    }

    func presentPhotos(limit: Int) {
        var config = PHPickerConfiguration(photoLibrary: .shared())
        config.filter = .any(of: [.images, .videos])
        config.selectionLimit = max(1, limit)
        config.preferredAssetRepresentationMode = .current
        let picker = PHPickerViewController(configuration: config)
        picker.delegate = self
        retainSelf = self
        host?.present(picker, animated: true)
    }

    func presentCamera() {
        let picker = UIImagePickerController()
        picker.sourceType = .camera
        picker.delegate = self
        retainSelf = self
        host?.present(picker, animated: true)
    }

    func presentFiles() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: true)
        picker.allowsMultipleSelection = true
        picker.delegate = self
        retainSelf = self
        host?.present(picker, animated: true)
    }

    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        let group = DispatchGroup()
        var staged: [(Int, StagedImage)] = []
        var failures: [String] = []
        let lock = NSLock()
        func finish(_ i: Int, _ result: Result<StagedImage, Error>) {
            lock.lock()
            switch result {
            case let .success(s): staged.append((i, s))
            case let .failure(error): failures.append(error.localizedDescription)
            }
            lock.unlock()
            group.leave()
        }
        for (i, result) in results.enumerated() {
            group.enter()
            let provider = result.itemProvider
            if provider.hasItemConformingToTypeIdentifier(UTType.movie.identifier) {
                provider.loadFileRepresentation(forTypeIdentifier: UTType.movie.identifier) { url, error in
                    // The provided file is deleted when this returns: copy it.
                    guard let url else { return finish(i, .failure(error ?? StagingError.unreadable(provider.suggestedName ?? "video"))) }
                    do {
                        let copy = try AttachmentPicker.copyToStaging(url, name: provider.suggestedName.map { "\($0).\(url.pathExtension)" })
                        Task {
                            let s = await AttachmentPicker.stage(file: copy, mimeType: UTType(filenameExtension: copy.pathExtension)?.preferredMIMEType ?? "video/quicktime")
                            finish(i, .success(s))
                        }
                    } catch {
                        finish(i, .failure(error))
                    }
                }
            } else {
                provider.loadDataRepresentation(forTypeIdentifier: UTType.image.identifier) { data, error in
                    guard let data, let image = UIImage(data: data), let s = AttachmentPicker.stage(image: image) else {
                        return finish(i, .failure(error ?? StagingError.unreadable(provider.suggestedName ?? "image")))
                    }
                    finish(i, .success(s))
                }
            }
        }
        group.notify(queue: .main) { [self] in
            completion(staged.sorted { $0.0 < $1.0 }.map(\.1))
            report(failures)
            retainSelf = nil
        }
    }

    func imagePickerController(_ picker: UIImagePickerController, didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]) {
        picker.dismiss(animated: true)
        if let image = info[.originalImage] as? UIImage, let s = AttachmentPicker.stage(image: image) { completion([s]) }
        retainSelf = nil
    }

    func imagePickerControllerDidCancel(_ picker: UIImagePickerController) {
        picker.dismiss(animated: true)
        retainSelf = nil
    }

    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        Task { @MainActor [self] in
            var staged: [StagedImage] = []
            var failures: [String] = []
            for url in urls {
                let type = UTType(filenameExtension: url.pathExtension)
                if type?.conforms(to: .image) == true, let data = try? Data(contentsOf: url), let image = UIImage(data: data) {
                    if let s = AttachmentPicker.stage(image: image, name: url.deletingPathExtension().lastPathComponent) {
                        staged.append(s)
                        try? FileManager.default.removeItem(at: url)
                    } else {
                        failures.append("\(url.lastPathComponent) is too large as an image.")
                    }
                    continue
                }
                do {
                    // `asCopy` made this ours already: move it out of the inbox.
                    let copy = try AttachmentPicker.copyToStaging(url, move: true)
                    staged.append(await AttachmentPicker.stage(file: copy, mimeType: type?.preferredMIMEType ?? "application/octet-stream"))
                } catch {
                    failures.append("\(url.lastPathComponent): \(error.localizedDescription)")
                }
            }
            completion(staged)
            report(failures)
            retainSelf = nil
        }
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) { retainSelf = nil }

    private enum StagingError: LocalizedError {
        case unreadable(String)

        var errorDescription: String? {
            switch self {
            case let .unreadable(name): "Couldn't read \(name)."
            }
        }
    }

    /// Staging failures are shown, never dropped.
    private func report(_ failures: [String]) {
        guard !failures.isEmpty, let host else { return }
        let alert = UIAlertController(title: "Couldn't attach", message: failures.joined(separator: "\n"), preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        // The picker may still be dismissing: present from whatever is on top.
        var top = host
        while let next = top.presentedViewController, !next.isBeingDismissed { top = next }
        top.present(alert, animated: true)
    }

    /// A temp copy of `url` in its own folder (so discarding it removes the
    /// folder), keeping the file's name.
    static func copyToStaging(_ url: URL, name: String? = nil, move: Bool = false) throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("staged-\(UUID().uuidString.lowercased())", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let dest = dir.appendingPathComponent(name ?? url.lastPathComponent)
        if move {
            do { try FileManager.default.moveItem(at: url, to: dest) } catch { try FileManager.default.copyItem(at: url, to: dest) }
        } else {
            try FileManager.default.copyItem(at: url, to: dest)
        }
        return dest
    }

    /// A video or any other file, staged by path; videos get a poster frame.
    static func stage(file url: URL, mimeType: String) async -> StagedImage {
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize).flatMap { $0 }.map(Int64.init) ?? 0
        var thumb: UIImage?
        if mimeType.hasPrefix("video/") {
            let generator = AVAssetImageGenerator(asset: AVURLAsset(url: url))
            generator.appliesPreferredTrackTransform = true
            generator.maximumSize = CGSize(width: 360, height: 360)
            if let cg = try? await generator.image(at: .zero).image { thumb = UIImage(cgImage: cg) }
        }
        return StagedImage(id: UUID().uuidString.lowercased(), name: url.lastPathComponent, thumbnail: thumb, file: StagedFile(url: url, size: size, mimeType: mimeType))
    }

    static func stage(image: UIImage, name: String? = nil) -> StagedImage? {
        let maxSide: CGFloat = 2560
        let size = image.size
        let scale = min(1, maxSide / max(size.width, size.height))
        let target = CGSize(width: (size.width * scale).rounded(), height: (size.height * scale).rounded())
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let resized = scale < 1 ? UIGraphicsImageRenderer(size: target, format: format).image { _ in image.draw(in: CGRect(origin: .zero, size: target)) } : image
        guard let data = resized.jpegData(compressionQuality: 0.86), data.count <= 24 * 1024 * 1024 else { return nil }
        let thumb = resized.preparingThumbnail(of: CGSize(width: 180, height: 180)) ?? resized
        let id = UUID().uuidString.lowercased()
        return StagedImage(id: id, name: "\(name ?? "image-\(id.prefix(8))").jpg", data: data, thumbnail: thumb)
    }
}
