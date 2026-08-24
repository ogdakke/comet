import AVKit
import ImageIO
import Observation
import SwiftUI
import UIKit

@MainActor
@Observable
private final class StudioViewerMediaStore {
    var previews: [String: UIImage] = [:]
    var images: [String: UIImage] = [:]
    var players: [String: AVPlayer] = [:]
    var loadingOriginals: Set<String> = []
    var errors: [String: String] = [:]

    @ObservationIgnored private var temporaryURLs: [String: URL] = [:]
    @ObservationIgnored private var previewTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var originalTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var retainedIds: Set<String> = []
    @ObservationIgnored private var selectedId: String?

    func preview(for artifactId: String) -> UIImage? { previews[artifactId] }
    func image(for artifactId: String) -> UIImage? { images[artifactId] }
    func player(for artifactId: String) -> AVPlayer? { players[artifactId] }
    func error(for artifactId: String) -> String? { errors[artifactId] }
    func originalURL(for artifactId: String) -> URL? { temporaryURLs[artifactId] }

    func prepare(
        selectedId: String,
        artifacts: [StudioArtifactDetail],
        browser: StudioBrowserStore,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard let selectedIndex = artifacts.firstIndex(where: { $0.id == selectedId }) else {
            return
        }
        self.selectedId = selectedId

        // Start the selected item first, then walk outward. The relay can
        // multiplex these reads, but request order still determines which
        // original gets its first chunk first.
        let neighborhood = [0, -1, 1, -2, 2].compactMap { offset in
            let index = selectedIndex + offset
            return artifacts.indices.contains(index) ? artifacts[index] : nil
        }
        retainedIds = Set(neighborhood.map(\.id))
        trimOutsideNeighborhood()
        trimTemporaryFiles(keeping: selectedId)
        trimVideoOriginals(
            keeping: selectedId,
            videoIds: Set(artifacts.filter { $0.mediaKind == .video }.map(\.id))
        )

        for player in players.values { player.pause() }
        players[selectedId]?.play()

        for artifact in neighborhood {
            loadPreview(
                artifact,
                browser: browser,
                workspace: workspace,
                deviceId: deviceId
            )
            // Images are small enough to keep a five-item display-quality
            // window. Videos can be much larger, so only fetch the selected
            // original while their neighboring thumbnails stay warm.
            if artifact.mediaKind == .image || artifact.id == selectedId {
                loadOriginal(
                    artifact,
                    browser: browser,
                    workspace: workspace,
                    deviceId: deviceId
                )
            }
        }
    }

    func reset() {
        previewTasks.values.forEach { $0.cancel() }
        originalTasks.values.forEach { $0.cancel() }
        previewTasks.removeAll()
        originalTasks.removeAll()
        for player in players.values { player.pause() }
        for url in temporaryURLs.values { try? FileManager.default.removeItem(at: url) }
        temporaryURLs.removeAll()
        previews.removeAll()
        images.removeAll()
        players.removeAll()
        loadingOriginals.removeAll()
        errors.removeAll()
    }

    private func loadPreview(
        _ artifact: StudioArtifactDetail,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        if previews[artifact.id] == nil,
           let cached = browser.cachedPreview(artifactId: artifact.id) {
            previews[artifact.id] = cached
        }
        guard previews[artifact.id] == nil, previewTasks[artifact.id] == nil else { return }
        let artifactId = artifact.id
        previewTasks[artifactId] = Task { [weak self] in
            defer { self?.previewTasks.removeValue(forKey: artifactId) }
            let image = await browser.preview(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            )
            guard let self else { return }
            guard !Task.isCancelled, self.retainedIds.contains(artifactId), let image else { return }
            self.previews[artifactId] = image
        }
    }

    private func loadOriginal(
        _ artifact: StudioArtifactDetail,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard images[artifact.id] == nil,
              players[artifact.id] == nil,
              originalTasks[artifact.id] == nil else { return }
        let artifactId = artifact.id
        loadingOriginals.insert(artifactId)
        errors.removeValue(forKey: artifactId)
        originalTasks[artifactId] = Task(priority: artifactId == selectedId ? .userInitiated : .utility) {
            defer {
                originalTasks.removeValue(forKey: artifactId)
                loadingOriginals.remove(artifactId)
            }
            do {
                let file = try await workspace.downloadStudioArtifact(
                    deviceId: deviceId,
                    artifactId: artifactId,
                    declaredSize: artifact.sizeBytes
                )
                guard !Task.isCancelled, retainedIds.contains(artifactId) else {
                    try? FileManager.default.removeItem(at: file.url)
                    return
                }
                temporaryURLs[artifactId] = file.url
                if artifact.mediaKind == .video {
                    let player = AVPlayer(url: file.url)
                    players[artifactId] = player
                    if selectedId == artifactId { player.play() }
                } else {
                    let longestEdge = max(Int(artifact.width ?? 0), Int(artifact.height ?? 0))
                    let image = await Task.detached(priority: .userInitiated) {
                        Self.decodeImage(
                            at: file.url,
                            maximumPixelSize: min(max(longestEdge, 2_048), 6_144)
                        )
                    }.value
                    guard !Task.isCancelled, retainedIds.contains(artifactId) else { return }
                    if let image {
                        images[artifactId] = image
                    } else {
                        errors[artifactId] = "The downloaded image couldn't be decoded"
                    }
                    if selectedId != artifactId {
                        try? FileManager.default.removeItem(at: file.url)
                        temporaryURLs.removeValue(forKey: artifactId)
                    }
                }
            } catch is CancellationError {
                // Moving through the filmstrip cancels work outside the window.
            } catch {
                guard !Task.isCancelled else { return }
                errors[artifactId] = error.localizedDescription
            }
        }
    }

    private func trimOutsideNeighborhood() {
        for id in previewTasks.keys.filter({ !retainedIds.contains($0) }) {
            previewTasks[id]?.cancel()
        }
        for id in originalTasks.keys.filter({ !retainedIds.contains($0) }) {
            originalTasks[id]?.cancel()
        }
        for id in previews.keys.filter({ !retainedIds.contains($0) }) { previews.removeValue(forKey: id) }
        for id in images.keys.filter({ !retainedIds.contains($0) }) { images.removeValue(forKey: id) }
        for id in players.keys.filter({ !retainedIds.contains($0) }) {
            players[id]?.pause()
            players.removeValue(forKey: id)
        }
        for id in temporaryURLs.keys.filter({ !retainedIds.contains($0) }) {
            if let url = temporaryURLs[id] { try? FileManager.default.removeItem(at: url) }
            temporaryURLs.removeValue(forKey: id)
        }
    }

    private func trimTemporaryFiles(keeping selectedId: String) {
        for id in temporaryURLs.keys.filter({ $0 != selectedId }) {
            if let url = temporaryURLs[id] { try? FileManager.default.removeItem(at: url) }
            temporaryURLs.removeValue(forKey: id)
        }
    }

    private func trimVideoOriginals(keeping selectedId: String, videoIds: Set<String>) {
        for id in originalTasks.keys.filter({ videoIds.contains($0) && $0 != selectedId }) {
            originalTasks[id]?.cancel()
        }
        for id in players.keys.filter({ $0 != selectedId }) {
            players[id]?.pause()
            players.removeValue(forKey: id)
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
    @State private var filmstripPosition: String?
    @State private var filmstripUserDriven = false
    @State private var chromeReady = false

    private var selected: StudioArtifactDetail? { session.selected }
    private var showingChrome: Bool { chromeReady && detailOffset < 72 }

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
                    closeViewer()
                }
            }
            .background(Color.black.ignoresSafeArea())
            .overlay(alignment: .top) { topControls }
            .overlay(alignment: .bottom) { bottomChrome(width: geometry.size.width) }
        }
        .ignoresSafeArea()
        .task(id: session.selectedId) {
            guard let selected,
                  let workspace = model.workspace,
                  let deviceId = browser.selectedDeviceId else { return }
            media.prepare(
                selectedId: selected.id,
                artifacts: session.artifacts,
                browser: browser,
                workspace: workspace,
                deviceId: deviceId
            )
        }
        .onAppear {
            filmstripPosition = session.selectedId
            chromeReady = false
        }
        .task {
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            withAnimation(.easeOut(duration: 0.16)) {
                chromeReady = true
            }
        }
        .onChange(of: session.selectedId) { _, selectedId in
            if !filmstripUserDriven, filmstripPosition != selectedId {
                withAnimation(.snappy(duration: 0.2)) {
                    filmstripPosition = selectedId
                }
            }
        }
        .onChange(of: filmstripPosition) { _, centeredId in
            guard filmstripUserDriven,
                  let centeredId,
                  centeredId != session.selectedId else { return }
            session.selectedId = centeredId
        }
        .onChange(of: session.artifacts.count) {
            guard let selected,
                  let workspace = model.workspace,
                  let deviceId = browser.selectedDeviceId else { return }
            media.prepare(
                selectedId: selected.id,
                artifacts: session.artifacts,
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
            session.replaceArtifacts(with: browser.gallery.reversed().map(StudioArtifactDetail.init(item:)))
        }
        .onDisappear {
            chromeReady = false
            media.reset()
        }
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
                    } else if let image = media.image(for: artifact.id)
                                ?? media.preview(for: artifact.id)
                                ?? browser.cachedPreview(artifactId: artifact.id) {
                        Image(uiImage: image)
                            .resizable()
                            .scaledToFit()
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
        if artifact.mediaKind == .video, let player = media.player(for: artifact.id) {
            VideoPlayer(player: player)
        } else if artifact.mediaKind == .image,
                  let image = media.image(for: artifact.id)
                    ?? media.preview(for: artifact.id)
                    ?? browser.cachedPreview(artifactId: artifact.id) {
            StudioZoomableImage(image: image)
                .id(artifact.id)
        } else if let preview = media.preview(for: artifact.id)
                    ?? browser.cachedPreview(artifactId: artifact.id) {
            Image(uiImage: preview)
                .resizable()
                .scaledToFit()
        } else {
            StudioMediaPreviewView(
                artifactId: artifact.id,
                mediaKind: artifact.mediaKind,
                browser: browser,
                contentMode: .fit
            )
        }
    }

    private var topControls: some View {
        GlassEffectContainer(spacing: 16) {
            HStack {
                Button { closeViewer() } label: {
                    Image(systemName: "chevron.backward")
                        .font(.system(size: 14, weight: .semibold))
                }
                .buttonStyle(.glass)
                .buttonBorderShape(.circle)
                .controlSize(.small)
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
                        .font(.system(size: 15, weight: .semibold))
                }
                .buttonStyle(.glass)
                .buttonBorderShape(.circle)
                .controlSize(.small)
                .accessibilityLabel("More actions")
            }
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 14)
        .padding(.top, 54)
        .padding(.bottom, 12)
        .opacity(showingChrome ? 1 : 0)
        .allowsHitTesting(showingChrome)
        .animation(.easeOut(duration: 0.18), value: showingChrome)
    }

    private func bottomChrome(width: CGFloat) -> some View {
        VStack(spacing: 10) {
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
                .padding(.vertical, 4)
                .scrollTargetLayout()
            }
            .frame(height: 58)
            .contentMargins(.horizontal, max(0, (width - 48) / 2), for: .scrollContent)
            .scrollIndicators(.hidden)
            .scrollTargetBehavior(.viewAligned)
            .scrollPosition(id: $filmstripPosition, anchor: .center)
            .onScrollPhaseChange { _, phase in
                switch phase {
                case .tracking, .interacting, .decelerating:
                    filmstripUserDriven = true
                case .idle, .animating:
                    filmstripUserDriven = false
                @unknown default:
                    filmstripUserDriven = false
                }
            }

            GlassEffectContainer(spacing: 0) {
                HStack {
                    Button { downloadSelected() } label: {
                        Group {
                            if saving {
                                ProgressView().controlSize(.small)
                            } else {
                                Image(systemName: "square.and.arrow.down")
                            }
                        }
                    }
                    .buttonStyle(.glass)
                    .buttonBorderShape(.circle)
                    .controlSize(.small)
                    .disabled(selected == nil || saving)
                    .accessibilityLabel("Download")

                    Spacer()

                    Button(role: .destructive) { confirmDelete = true } label: {
                        Image(systemName: "trash")
                    }
                    .buttonStyle(.glass)
                    .buttonBorderShape(.circle)
                    .controlSize(.small)
                    .disabled(selected == nil)
                    .accessibilityLabel("Delete")
                }
                .padding(.horizontal, 14)
            }
            .font(.system(size: 15, weight: .regular))
            .foregroundStyle(.white)
        }
        .padding(.top, 8)
        .padding(.bottom, 26)
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

            if let error = media.error(for: artifact.id) {
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
                    existingFile: media.originalURL(for: artifact.id)
                )
            } catch {
                actionError = error.localizedDescription
            }
        }
    }

    private func closeViewer() {
        guard chromeReady else {
            dismiss()
            return
        }
        withAnimation(.easeIn(duration: 0.1)) {
            chromeReady = false
        }
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(100))
            dismiss()
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
