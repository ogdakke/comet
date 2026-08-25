import AVKit
import ImageIO
import Observation
import SwiftUI
import UIKit

private struct StudioViewerScrollState: Equatable {
    let detailsVisible: Bool
    let shouldDismiss: Bool
}

@MainActor
@Observable
private final class StudioViewerMediaStore {
    var previews: [String: UIImage] = [:]
    var images: [String: UIImage] = [:]
    var players: [String: AVPlayer] = [:]
    var loadingOriginals: Set<String> = []
    var errors: [String: String] = [:]

    @ObservationIgnored private var temporaryURLs: [String: URL] = [:]
    @ObservationIgnored private var videoStreams: [String: StudioVideoStream] = [:]
    @ObservationIgnored private var previewTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var originalTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var neighborWarmTask: Task<Void, Never>?
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
        neighborWarmTask?.cancel()
        trimOutsideNeighborhood()
        trimTemporaryFiles(keeping: selectedId)
        trimVideoStreams(keeping: selectedId)

        for player in players.values { player.pause() }
        players[selectedId]?.play()

        for artifact in neighborhood {
            loadPreview(
                artifact,
                browser: browser,
                workspace: workspace,
                deviceId: deviceId
            )
        }

        if let selected = neighborhood.first {
            loadOriginal(
                selected,
                browser: browser,
                workspace: workspace,
                deviceId: deviceId
            )
        }

        // Do not compete with an active swipe or filmstrip fling. Once the
        // selection rests, warm the neighboring images for the next swipe.
        let neighbors = neighborhood.dropFirst().filter { $0.mediaKind == .image }
        neighborWarmTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(220))
            guard let self, !Task.isCancelled, self.selectedId == selectedId else { return }
            for artifact in neighbors {
                self.loadOriginal(
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
        neighborWarmTask?.cancel()
        neighborWarmTask = nil
        previewTasks.removeAll()
        originalTasks.removeAll()
        for player in players.values { player.pause() }
        videoStreams.values.forEach { $0.cancel() }
        videoStreams.removeAll()
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
        if artifact.mediaKind == .video {
            loadVideo(
                artifact,
                workspace: workspace,
                deviceId: deviceId
            )
            return
        }
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
            } catch is CancellationError {
                // Moving through the filmstrip cancels work outside the window.
            } catch {
                guard !Task.isCancelled else { return }
                errors[artifactId] = error.localizedDescription
            }
        }
    }

    private func loadVideo(
        _ artifact: StudioArtifactDetail,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard videoStreams[artifact.id] == nil else { return }
        let artifactId = artifact.id
        let stream = StudioVideoStream(
            artifactId: artifactId,
            mimeType: artifact.mimeType,
            declaredSize: artifact.sizeBytes,
            readChunk: { offset in
                try await workspace.readStudioArtifactChunk(
                    deviceId: deviceId,
                    artifactId: artifactId,
                    offset: offset
                )
            }
        )
        videoStreams[artifactId] = stream
        players[artifactId] = stream.player
        if selectedId == artifactId { stream.player.play() }
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
            videoStreams.removeValue(forKey: id)?.cancel()
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

    private func trimVideoStreams(keeping selectedId: String) {
        for id in players.keys.filter({ $0 != selectedId }) {
            players[id]?.pause()
            players.removeValue(forKey: id)
            videoStreams.removeValue(forKey: id)?.cancel()
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
    @Bindable var session: StudioViewerSession
    let browser: StudioBrowserStore
    let onDismiss: () -> Void
    let showThread: (String) -> Void

    @State private var media = StudioViewerMediaStore()
    @State private var detailsVisible = false
    @State private var pullDismissed = false
    @State private var confirmDelete = false
    @State private var actionError: String?
    @State private var saving = false
    @State private var filmstripPosition: String?
    @State private var filmstripUserDriven = false
    @State private var pagerPosition: String?
    @State private var pagerUserDriven = false
    @State private var chromeReady = false
    @State private var backdropReady = false

    private var selected: StudioArtifactDetail? { session.selected }
    private var showingChrome: Bool { chromeReady && !detailsVisible }

    init(
        session: StudioViewerSession,
        browser: StudioBrowserStore,
        onDismiss: @escaping () -> Void,
        showThread: @escaping (String) -> Void
    ) {
        self.session = session
        self.browser = browser
        self.onDismiss = onDismiss
        self.showThread = showThread
        _filmstripPosition = State(initialValue: session.selectedId)
        _pagerPosition = State(initialValue: session.selectedId)
    }

    var body: some View {
        GeometryReader { geometry in
            ScrollView(.vertical) {
                VStack(spacing: 0) {
                    mediaPager
                        .frame(height: geometry.size.height)
                    if let selected {
                        artifactDetails(selected)
                    }
                }
            }
            .scrollIndicators(.hidden)
            .onScrollGeometryChange(for: StudioViewerScrollState.self) { geometry in
                let offset = geometry.contentOffset.y + geometry.contentInsets.top
                return StudioViewerScrollState(
                    detailsVisible: offset >= 72,
                    shouldDismiss: offset < -110
                )
            } action: { _, value in
                detailsVisible = value.detailsVisible
                if value.shouldDismiss, !pullDismissed {
                    pullDismissed = true
                    closeViewer()
                }
            }
            .background(Color.black.opacity(backdropReady ? 1 : 0).ignoresSafeArea())
            .overlay(alignment: .top) { topControls }
            .overlay(alignment: .bottom) { bottomChrome(width: geometry.size.width) }
        }
        .ignoresSafeArea()
        .task(id: "media-\(session.selectedId)-\(session.artifacts.count)") {
            try? await Task.sleep(for: .milliseconds(90))
            guard !Task.isCancelled else { return }
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
            pagerPosition = session.selectedId
            chromeReady = false
            backdropReady = false
        }
        .task {
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            withAnimation(.easeOut(duration: 0.16)) {
                chromeReady = true
                backdropReady = true
            }
        }
        .onChange(of: session.selectedId) { _, selectedId in
            if !filmstripUserDriven, filmstripPosition != selectedId {
                withAnimation(.snappy(duration: 0.2)) {
                    filmstripPosition = selectedId
                }
            }
            if !pagerUserDriven, pagerPosition != selectedId {
                pagerPosition = selectedId
            }
        }
        .onChange(of: filmstripPosition) { _, centeredId in
            guard filmstripUserDriven,
                  let centeredId,
                  centeredId != session.selectedId else { return }
            session.selectedId = centeredId
        }
        .onChange(of: pagerPosition) { _, centeredId in
            guard pagerUserDriven,
                  let centeredId,
                  centeredId != session.selectedId else { return }
            session.selectedId = centeredId
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
            backdropReady = false
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
        ScrollView(.horizontal) {
            LazyHStack(spacing: 0) {
                ForEach(session.artifacts) { artifact in
                    Group {
                        if artifact.id == session.selectedId {
                            selectedMedia(artifact)
                        } else if let image = media.image(for: artifact.id)
                                    ?? media.preview(for: artifact.id)
                                    ?? session.openingPreview(for: artifact.id)
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
                    .containerRelativeFrame(.horizontal)
                    .frame(maxHeight: .infinity)
                    .id(artifact.id)
                }
            }
            .scrollTargetLayout()
        }
        .scrollIndicators(.hidden)
        .scrollTargetBehavior(.paging)
        .scrollPosition(id: $pagerPosition, anchor: .center)
        .onScrollPhaseChange { _, phase in
            pagerUserDriven = phase.isUserDriven
        }
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
                .aspectRatio(artifact.aspectRatio, contentMode: .fit)
        } else if artifact.mediaKind == .image {
            ZStack {
                if let preview = media.preview(for: artifact.id)
                    ?? session.openingPreview(for: artifact.id)
                    ?? browser.cachedPreview(artifactId: artifact.id) {
                    Image(uiImage: preview)
                        .resizable()
                        .scaledToFit()
                } else if media.image(for: artifact.id) == nil {
                    StudioMediaPreviewView(
                        artifactId: artifact.id,
                        mediaKind: artifact.mediaKind,
                        browser: browser,
                        contentMode: .fit
                    )
                }

                if let image = media.image(for: artifact.id) {
                    StudioZoomableImage(image: image)
                        .id(artifact.id)
                }
            }
        } else if let preview = media.preview(for: artifact.id)
                    ?? session.openingPreview(for: artifact.id)
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
                        .studioViewerGlassControl()
                }
                .buttonStyle(.plain)
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
                        .studioViewerGlassControl()
                }
                .buttonStyle(.plain)
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
                            session.selectedId = artifact.id
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
                filmstripUserDriven = phase.isUserDriven
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
                        .studioViewerGlassControl()
                    }
                    .buttonStyle(.plain)
                    .disabled(selected == nil || saving)
                    .accessibilityLabel("Download")

                    Spacer()

                    Button(role: .destructive) { confirmDelete = true } label: {
                        Image(systemName: "trash")
                            .studioViewerGlassControl()
                    }
                    .buttonStyle(.plain)
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
            onDismiss()
            return
        }
        withAnimation(.easeIn(duration: 0.1)) {
            chromeReady = false
            backdropReady = false
        }
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(100))
            onDismiss()
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
                onDismiss()
            } catch {
                actionError = error.localizedDescription
            }
        }
    }
}

private struct StudioZoomableImage: UIViewRepresentable {
    let image: UIImage

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> StudioImageScrollView {
        let scroll = StudioImageScrollView()
        scroll.delegate = context.coordinator
        scroll.minimumZoomScale = 1
        scroll.maximumZoomScale = 6
        scroll.bouncesZoom = true
        scroll.showsHorizontalScrollIndicator = false
        scroll.showsVerticalScrollIndicator = false
        scroll.backgroundColor = .clear
        scroll.panGestureRecognizer.isEnabled = false

        let imageView = context.coordinator.imageView
        imageView.contentMode = .scaleAspectFit
        imageView.clipsToBounds = true
        scroll.addSubview(imageView)
        scroll.zoomImageView = imageView

        let doubleTap = UITapGestureRecognizer(
            target: context.coordinator,
            action: #selector(Coordinator.doubleTap(_:))
        )
        doubleTap.numberOfTapsRequired = 2
        scroll.addGestureRecognizer(doubleTap)
        context.coordinator.scrollView = scroll
        return scroll
    }

    func updateUIView(_ scroll: StudioImageScrollView, context: Context) {
        if context.coordinator.imageView.image !== image {
            context.coordinator.imageView.image = image
        }
        scroll.setNeedsLayout()
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

private final class StudioImageScrollView: UIScrollView {
    weak var zoomImageView: UIView?

    override func layoutSubviews() {
        super.layoutSubviews()
        if zoomScale <= minimumZoomScale + 0.001 {
            zoomImageView?.frame = bounds
        }
    }
}

private extension View {
    func studioViewerGlassControl() -> some View {
        frame(width: 40, height: 40)
            .glassEffect(.regular.interactive(), in: Circle())
            .contentShape(Circle())
            .padding(2)
    }

}

private extension ScrollPhase {
    var isUserDriven: Bool {
        switch self {
        case .tracking, .interacting, .decelerating:
            true
        case .idle, .animating:
            false
        @unknown default:
            false
        }
    }
}
