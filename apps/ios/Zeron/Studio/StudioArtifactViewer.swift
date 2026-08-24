import AVKit
import ImageIO
import Observation
import SwiftUI
import UIKit

@MainActor
@Observable
private final class StudioViewerMediaStore {
    var preview: UIImage?
    var image: UIImage?
    var player: AVPlayer?
    var loadingOriginal = false
    var error: String?

    @ObservationIgnored private var temporaryURL: URL?

    var originalURL: URL? { temporaryURL }

    func load(
        artifact: StudioArtifactDetail,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore,
        deviceId: String
    ) async {
        reset()
        error = nil
        preview = await browser.preview(
            artifactId: artifact.id,
            deviceId: deviceId,
            workspace: workspace
        )
        guard !Task.isCancelled else { return }

        loadingOriginal = true
        do {
            let file = try await workspace.downloadStudioArtifact(
                deviceId: deviceId,
                artifactId: artifact.id,
                declaredSize: artifact.sizeBytes
            )
            guard !Task.isCancelled else {
                try? FileManager.default.removeItem(at: file.url)
                loadingOriginal = false
                return
            }
            temporaryURL = file.url
            if artifact.mediaKind == .video {
                let player = AVPlayer(url: file.url)
                self.player = player
                player.play()
            } else {
                let longestEdge = max(Int(artifact.width ?? 0), Int(artifact.height ?? 0))
                image = await Task.detached(priority: .userInitiated) {
                    Self.decodeImage(
                        at: file.url,
                        maximumPixelSize: min(max(longestEdge, 2_048), 6_144)
                    )
                }.value
                if image == nil {
                    throw RelayError.rpc("The downloaded image couldn't be decoded")
                }
            }
        } catch is CancellationError {
            // Selection changes are ordinary navigation, not viewer errors.
        } catch {
            guard !Task.isCancelled else { return }
            self.error = error.localizedDescription
        }
        loadingOriginal = false
    }

    func reset() {
        player?.pause()
        player = nil
        image = nil
        preview = nil
        loadingOriginal = false
        if let temporaryURL {
            try? FileManager.default.removeItem(at: temporaryURL)
            self.temporaryURL = nil
        }
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
        guard let cgImage = CGImageSourceCreateThumbnailAtIndex(source, 0, options) else {
            return nil
        }
        return UIImage(cgImage: cgImage)
    }
}

struct StudioArtifactViewer: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Bindable var session: StudioViewerSession
    let browser: StudioBrowserStore
    let showThread: (String) -> Void

    @State private var media = StudioViewerMediaStore()
    @State private var detailOffset: CGFloat = 0
    @State private var pullDismissed = false
    @State private var confirmDelete = false
    @State private var actionError: String?
    @State private var saving = false

    private var selected: StudioArtifactDetail? { session.selected }
    private var showingChrome: Bool { detailOffset < 72 }

    var body: some View {
        GeometryReader { geometry in
            ScrollView(.vertical) {
                LazyVStack(spacing: 0) {
                    mediaPager
                        .frame(height: geometry.size.height)
                        .containerRelativeFrame(.vertical)
                    if let selected {
                        artifactDetails(selected)
                    }
                }
            }
            .scrollIndicators(.hidden)
            .scrollTargetBehavior(.viewAligned)
            .onScrollGeometryChange(for: CGFloat.self) { geometry in
                geometry.contentOffset.y + geometry.contentInsets.top
            } action: { _, value in
                detailOffset = value
                if value < -110, !pullDismissed {
                    pullDismissed = true
                    dismiss()
                }
            }
            .background(Color.black.ignoresSafeArea())
            .overlay(alignment: .top) { topControls }
            .overlay(alignment: .bottom) { bottomChrome }
        }
        .ignoresSafeArea()
        .task(id: session.selectedId) {
            guard let selected,
                  let workspace = model.workspace,
                  let deviceId = browser.selectedDeviceId else { return }
            await media.load(
                artifact: selected,
                browser: browser,
                workspace: workspace,
                deviceId: deviceId
            )
        }
        .task(id: "gallery-page-\(session.selectedId)") {
            guard session.openedFromGallery,
                  let item = browser.gallery.first(where: { $0.id == session.selectedId }),
                  browser.shouldLoadMore(after: item),
                  let workspace = model.workspace,
                  let deviceId = browser.selectedDeviceId else { return }
            await browser.loadMoreGallery(workspace: workspace, deviceId: deviceId)
            session.append(browser.gallery.map(StudioArtifactDetail.init(item:)))
        }
        .onDisappear { media.reset() }
        .confirmationDialog(
            "Delete this creation?",
            isPresented: $confirmDelete,
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) { deleteSelected() }
            Button("Cancel", role: .cancel) { }
        } message: {
            Text("This removes it from Studio on the selected machine.")
        }
        .alert("Studio action failed", isPresented: Binding(
            get: { actionError != nil },
            set: { if !$0 { actionError = nil } }
        )) {
            Button("OK") { actionError = nil }
        } message: {
            Text(actionError ?? "")
        }
    }

    private var mediaPager: some View {
        TabView(selection: $session.selectedId) {
            ForEach(session.artifacts) { artifact in
                Group {
                    if artifact.id == session.selectedId {
                        selectedMedia(artifact)
                    } else {
                        StudioMediaPreviewView(
                            artifactId: artifact.id,
                            mediaKind: artifact.mediaKind,
                            browser: browser,
                            contentMode: .fit
                        )
                    }
                }
                .tag(artifact.id)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black)
            }
        }
        .tabViewStyle(.page(indexDisplayMode: .never))
        .contextMenu {
            Button { downloadSelected() } label: {
                Label("Download", systemImage: "square.and.arrow.down")
            }
            if let selected {
                Button { showThread(selected.conversationId) } label: {
                    Label("Show in Thread", systemImage: "rectangle.stack")
                }
            }
            Button(role: .destructive) { confirmDelete = true } label: {
                Label("Delete", systemImage: "trash")
            }
        }
    }

    @ViewBuilder private func selectedMedia(_ artifact: StudioArtifactDetail) -> some View {
        if artifact.mediaKind == .video, let player = media.player {
            VideoPlayer(player: player)
        } else if artifact.mediaKind == .image, let image = media.image ?? media.preview {
            StudioZoomableImage(image: image)
                .id(artifact.id)
        } else if let preview = media.preview {
            Image(uiImage: preview)
                .resizable()
                .scaledToFit()
        } else {
            ProgressView()
                .tint(.white)
        }
    }

    private var topControls: some View {
        HStack {
            Button { dismiss() } label: {
                Image(systemName: "chevron.backward")
                    .font(.system(size: 16, weight: .semibold))
                    .frame(width: 42, height: 42)
            }
            .accessibilityLabel("Close")

            Spacer()

            Menu {
                if let selected {
                    Button { downloadSelected() } label: {
                        Label("Download", systemImage: "square.and.arrow.down")
                    }
                    Button {
                        showThread(selected.conversationId)
                    } label: {
                        Label("Show in Thread", systemImage: "rectangle.stack")
                    }
                    Button(role: .destructive) { confirmDelete = true } label: {
                        Label("Delete", systemImage: "trash")
                    }
                }
            } label: {
                Image(systemName: "ellipsis")
                    .font(.system(size: 17, weight: .semibold))
                    .frame(width: 42, height: 42)
            }
            .accessibilityLabel("More actions")
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 14)
        .padding(.top, 54)
        .padding(.bottom, 12)
        .background(.ultraThinMaterial.opacity(0.45))
        .opacity(showingChrome ? 1 : 0)
        .allowsHitTesting(showingChrome)
        .animation(.easeOut(duration: 0.18), value: showingChrome)
    }

    private var bottomChrome: some View {
        VStack(spacing: 10) {
            if media.loadingOriginal {
                ProgressView()
                    .controlSize(.small)
                    .tint(.white)
                    .accessibilityLabel("Loading full quality")
            }

            ScrollView(.horizontal) {
                LazyHStack(spacing: 5) {
                    ForEach(session.artifacts) { artifact in
                        Button {
                            withAnimation(.snappy(duration: 0.22)) {
                                session.selectedId = artifact.id
                            }
                        } label: {
                            StudioMediaPreviewView(
                                artifactId: artifact.id,
                                mediaKind: artifact.mediaKind,
                                browser: browser,
                                contentMode: .fill
                            )
                            .frame(width: 48, height: 48)
                            .clipShape(RoundedRectangle(cornerRadius: 5))
                            .overlay {
                                RoundedRectangle(cornerRadius: 5)
                                    .stroke(.white, lineWidth: session.selectedId == artifact.id ? 2 : 0)
                            }
                            .scaleEffect(session.selectedId == artifact.id ? 1.08 : 1)
                        }
                        .buttonStyle(.plain)
                        .id(artifact.id)
                        .accessibilityLabel("View item")
                        .accessibilityAddTraits(session.selectedId == artifact.id ? .isSelected : [])
                    }
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 4)
            }
            .frame(height: 58)
            .scrollIndicators(.hidden)
            .scrollPosition(id: Binding(
                get: { session.selectedId as String? },
                set: { _ in }
            ), anchor: .center)

            HStack(spacing: 44) {
                Button {
                    if let selected { showThread(selected.conversationId) }
                } label: {
                    Image(systemName: "rectangle.stack")
                }
                .disabled(session.selected == nil)
                .accessibilityLabel("Show in Thread")

                Button { downloadSelected() } label: {
                    if saving {
                        ProgressView().controlSize(.small).tint(.white)
                    } else {
                        Image(systemName: "square.and.arrow.down")
                    }
                }
                .disabled(selected == nil || saving)
                .accessibilityLabel("Download")

                Button(role: .destructive) { confirmDelete = true } label: {
                    Image(systemName: "trash")
                }
                .disabled(selected == nil)
                .accessibilityLabel("Delete")
            }
            .font(.system(size: 20, weight: .regular))
            .foregroundStyle(.white)
            .frame(height: 42)
        }
        .padding(.top, 10)
        .padding(.bottom, 26)
        .background(.ultraThinMaterial.opacity(0.5))
        .opacity(showingChrome ? 1 : 0)
        .allowsHitTesting(showingChrome)
        .animation(.easeOut(duration: 0.18), value: showingChrome)
    }

    private func artifactDetails(_ artifact: StudioArtifactDetail) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(artifact.prompt)
                .font(Theme.sans(17, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            VStack(spacing: 12) {
                LabeledContent("Model", value: artifact.modelDisplayName)
                if let width = artifact.width, let height = artifact.height {
                    LabeledContent("Dimensions", value: "\(width) × \(height)")
                }
                if let seconds = artifact.durationSeconds {
                    let total = max(0, Int(seconds.rounded()))
                    LabeledContent("Duration", value: String(format: "%d:%02d", total / 60, total % 60))
                }
                LabeledContent(
                    "Size",
                    value: ByteCountFormatter.string(
                        fromByteCount: Int64(artifact.sizeBytes),
                        countStyle: .file
                    )
                )
                LabeledContent(
                    "Created",
                    value: artifact.createdDate.formatted(.relative(presentation: .named))
                )
            }
            .font(Theme.sans(13))
            .foregroundStyle(Theme.textMuted)

            if let error = media.error {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.dangerSoft)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 20)
        .padding(.top, 28)
        .padding(.bottom, 120)
        .background(Theme.bg)
    }

    private func downloadSelected() {
        guard let artifact = selected,
              let workspace = model.workspace,
              let deviceId = browser.selectedDeviceId,
              !saving else { return }
        saving = true
        Task {
            defer { saving = false }
            do {
                try await StudioArtifactActions.download(
                    artifact,
                    workspace: workspace,
                    deviceId: deviceId,
                    existingFile: media.originalURL
                )
            } catch {
                actionError = error.localizedDescription
            }
        }
    }

    private func deleteSelected() {
        guard let artifact = selected,
              let workspace = model.workspace,
              let deviceId = browser.selectedDeviceId else { return }
        Task {
            do {
                try await workspace.deleteStudioArtifact(
                    deviceId: deviceId,
                    artifactId: artifact.id
                )
                browser.removeArtifact(artifact.id)
                dismiss()
            } catch {
                actionError = error.localizedDescription
            }
        }
    }
}

private struct StudioZoomableImage: UIViewRepresentable {
    let image: UIImage

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> UIScrollView {
        let scroll = UIScrollView()
        scroll.delegate = context.coordinator
        scroll.minimumZoomScale = 1
        scroll.maximumZoomScale = 6
        scroll.bouncesZoom = true
        scroll.showsHorizontalScrollIndicator = false
        scroll.showsVerticalScrollIndicator = false
        scroll.backgroundColor = .black
        scroll.panGestureRecognizer.isEnabled = false

        let imageView = context.coordinator.imageView
        imageView.contentMode = .scaleAspectFit
        imageView.clipsToBounds = true
        scroll.addSubview(imageView)

        let doubleTap = UITapGestureRecognizer(
            target: context.coordinator,
            action: #selector(Coordinator.doubleTap(_:))
        )
        doubleTap.numberOfTapsRequired = 2
        scroll.addGestureRecognizer(doubleTap)
        context.coordinator.scrollView = scroll
        return scroll
    }

    func updateUIView(_ scroll: UIScrollView, context: Context) {
        context.coordinator.imageView.frame = scroll.bounds
        if context.coordinator.imageView.image !== image {
            context.coordinator.imageView.image = image
        }
    }

    final class Coordinator: NSObject, UIScrollViewDelegate {
        let imageView = UIImageView()
        weak var scrollView: UIScrollView?

        func viewForZooming(in scrollView: UIScrollView) -> UIView? { imageView }

        func scrollViewDidZoom(_ scrollView: UIScrollView) {
            scrollView.panGestureRecognizer.isEnabled = scrollView.zoomScale > 1.001
        }

        @objc func doubleTap(_ gesture: UITapGestureRecognizer) {
            guard let scroll = scrollView else { return }
            if scroll.zoomScale > 1.001 {
                scroll.setZoomScale(1, animated: true)
            } else {
                scroll.panGestureRecognizer.isEnabled = true
                let point = gesture.location(in: imageView)
                let size = CGSize(width: scroll.bounds.width / 3, height: scroll.bounds.height / 3)
                scroll.zoom(to: CGRect(
                    x: point.x - size.width / 2,
                    y: point.y - size.height / 2,
                    width: size.width,
                    height: size.height
                ), animated: true)
            }
        }
    }
}
