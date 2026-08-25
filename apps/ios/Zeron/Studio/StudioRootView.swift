import SwiftUI

enum StudioRoute: Hashable {
    case thread(String)
}

private enum StudioLibraryMode: String, CaseIterable, Identifiable {
    case gallery = "Gallery"
    case threads = "Threads"

    var id: Self { self }
}

private struct StudioLibrarySwitcher: View {
    @Binding var selection: StudioLibraryMode
    @Namespace private var selectionMotion

    var body: some View {
        GlassEffectContainer(spacing: 0) {
            HStack(spacing: 0) {
                ForEach(StudioLibraryMode.allCases) { mode in
                    Button {
                        withAnimation(.snappy(duration: 0.24)) {
                            selection = mode
                        }
                    } label: {
                        Text(mode.rawValue)
                            .font(Theme.sans(14, weight: .medium))
                            .foregroundStyle(selection == mode ? Theme.text : Theme.textMuted)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                            .background {
                                if selection == mode {
                                    Capsule()
                                        .fill(.clear)
                                        .glassEffect(.regular.interactive(), in: Capsule())
                                        .glassEffectID("studio-library-selection", in: selectionMotion)
                                }
                            }
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(selection == mode ? .isSelected : [])
                }
            }
            .padding(4)
            .frame(width: 188, height: 44)
            .glassEffect(.regular.interactive(), in: Capsule())
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Studio view")
    }
}

struct StudioRootView: View {
    @Environment(AppModel.self) private var model
    @State private var browser = StudioBrowserStore()
    @State private var mode = StudioLibraryMode.gallery
    @State private var path: [StudioRoute] = []
    @State private var viewer: StudioViewerSession?
    @State private var galleryTransitionSource = StudioGalleryTransitionSource()
    @Namespace private var artifactTransition

    private var hostKey: String {
        "\(browser.selectedDeviceId ?? "none")-\(browser.reloadGeneration)"
    }

    var body: some View {
        NavigationStack(path: $path) {
            content
                .background(Theme.surface.ignoresSafeArea())
                .navigationTitle("")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .principal) {
                        StudioLibrarySwitcher(selection: $mode)
                    }
                    ToolbarItem(placement: .topBarTrailing) {
                        machineMenu
                    }
                }
                .scrollEdgeEffectStyle(.soft, for: .top)
            .navigationDestination(for: StudioRoute.self) { route in
                switch route {
                case .thread(let threadId):
                    StudioThreadView(
                        threadId: threadId,
                        browser: browser,
                        artifactTransition: artifactTransition,
                        openViewer: { viewer = $0 }
                    )
                }
            }
            .task(id: model.studioHosts.map(\.id).joined()) {
                browser.resolveDevice(from: model.studioHosts, online: model.deviceOnline)
            }
            .task(id: "threads-\(hostKey)") {
                guard model.demo == nil,
                      let deviceId = browser.selectedDeviceId,
                      let workspace = model.workspace else { return }
                await browser.watchThreads(workspace: workspace, deviceId: deviceId)
            }
            .task(id: "gallery-\(hostKey)") {
                guard model.demo == nil,
                      let deviceId = browser.selectedDeviceId,
                      let workspace = model.workspace else { return }
                await browser.watchGallery(workspace: workspace, deviceId: deviceId)
            }
            .background {
                StudioViewerPresentationBridge(
                    session: $viewer,
                    browser: browser,
                    model: model,
                    transitionSource: galleryTransitionSource,
                    showThread: { threadId in
                        mode = .threads
                        path = [.thread(threadId)]
                    }
                )
                .frame(width: 0, height: 0)
            }
        }
    }

    @ViewBuilder private var content: some View {
        if model.demo != nil {
            unavailable(
                title: "Studio is not in the demo",
                message: "Connect to a desktop device to browse your Studio library."
            )
        } else if model.studioHosts.isEmpty {
            unavailable(
                title: "Studio needs a desktop",
                message: "Open Zeron on a desktop device, then try again."
            )
        } else if let deviceId = browser.selectedDeviceId, !model.deviceOnline(deviceId) {
            unavailable(
                title: "Desktop is offline",
                message: "Reconnect \(model.deviceName(deviceId)) to browse its Studio library."
            )
        } else {
            switch mode {
            case .gallery:
                StudioGalleryView(
                    browser: browser,
                    transitionSource: galleryTransitionSource,
                    openViewer: { viewer = $0 },
                    showThread: { threadId in
                        mode = .threads
                        path = [.thread(threadId)]
                    }
                )
            case .threads:
                StudioThreadsView(browser: browser, path: $path)
            }
        }
    }

    private func unavailable(title: String, message: String) -> some View {
        ContentUnavailableView {
            Label(title, systemImage: "desktopcomputer.trianglebadge.exclamationmark")
        } description: {
            Text(message)
        } actions: {
            Button("Try again") { browser.reload() }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var machineMenu: some View {
        Menu {
            ForEach(model.studioHosts) { device in
                Button {
                    browser.selectDevice(device.id)
                } label: {
                    Label {
                        Text(model.deviceName(device.id))
                    } icon: {
                        if browser.selectedDeviceId == device.id {
                            Image(systemName: "checkmark")
                        } else {
                            Image(systemName: model.deviceOnline(device.id)
                                  ? "desktopcomputer" : "desktopcomputer.trianglebadge.exclamationmark")
                        }
                    }
                }
            }
        } label: {
            Image(systemName: "desktopcomputer")
        }
        .accessibilityLabel("Studio machine")
        .accessibilityValue(browser.selectedDeviceId.map(model.deviceName) ?? "None")
    }

}

private struct StudioGalleryView: View {
    @Environment(AppModel.self) private var model
    let browser: StudioBrowserStore
    let transitionSource: StudioGalleryTransitionSource
    let openViewer: (StudioViewerSession) -> Void
    let showThread: (String) -> Void
    @State private var artifactToDelete: StudioArtifactDetail?
    @State private var actionError: String?

    var body: some View {
        if browser.galleryLoading, browser.gallery.isEmpty {
            loading
        } else if let error = browser.galleryError, browser.gallery.isEmpty {
            errorView(error)
        } else if browser.gallery.isEmpty {
            ContentUnavailableView(
                "No creations yet",
                systemImage: "photo.stack",
                description: Text("Images and videos from your Studio threads will appear here.")
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            StudioGalleryCollectionView(
                items: browser.gallery,
                browser: browser,
                workspace: model.workspace,
                deviceId: browser.selectedDeviceId,
                transitionSource: transitionSource,
                openItem: { item, preview in
                    openViewer(StudioViewerSession(
                        artifacts: browser.gallery.map(StudioArtifactDetail.init(item:)),
                        selectedId: item.id,
                        openedFromGallery: true,
                        openingPreview: preview ?? browser.cachedPreview(artifactId: item.id)
                    ))
                },
                downloadItem: { download(StudioArtifactDetail(item: $0)) },
                showThread: pathToThread,
                deleteItem: { artifactToDelete = StudioArtifactDetail(item: $0) },
                loadOlder: loadOlder
            )
            .confirmationDialog(
                "Delete this creation?",
                isPresented: Binding(
                    get: { artifactToDelete != nil },
                    set: { if !$0 { artifactToDelete = nil } }
                ),
                titleVisibility: .visible
            ) {
                Button("Delete", role: .destructive) {
                    guard let artifact = artifactToDelete else { return }
                    artifactToDelete = nil
                    delete(artifact)
                }
                Button("Cancel", role: .cancel) { artifactToDelete = nil }
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
    }

    private var loading: some View {
        VStack(spacing: 14) {
            ZeronPulse()
            Text("Loading gallery…")
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textFaint)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func errorView(_ error: String) -> some View {
        ContentUnavailableView(
            "Gallery unavailable",
            systemImage: "exclamationmark.triangle",
            description: Text(error)
        )
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func pathToThread(_ threadId: String) {
        showThread(threadId)
    }

    private func loadOlder() {
        guard let workspace = model.workspace,
              let deviceId = browser.selectedDeviceId else { return }
        Task {
            await browser.loadMoreGallery(workspace: workspace, deviceId: deviceId)
        }
    }

    private func download(_ artifact: StudioArtifactDetail) {
        guard let workspace = model.workspace,
              let deviceId = browser.selectedDeviceId else { return }
        Task {
            do {
                try await StudioArtifactActions.download(
                    artifact,
                    workspace: workspace,
                    deviceId: deviceId
                )
            } catch {
                actionError = error.localizedDescription
            }
        }
    }

    private func delete(_ artifact: StudioArtifactDetail) {
        guard let workspace = model.workspace,
              let deviceId = browser.selectedDeviceId else { return }
        Task {
            do {
                try await workspace.deleteStudioArtifact(
                    deviceId: deviceId,
                    artifactId: artifact.id
                )
                browser.removeArtifact(artifact.id)
            } catch {
                actionError = error.localizedDescription
            }
        }
    }
}

private struct StudioThreadsView: View {
    let browser: StudioBrowserStore
    @Binding var path: [StudioRoute]
    @State private var archivedOpen = false

    private var active: [StudioThreadSummary] { browser.threads.filter { !$0.archived } }
    private var archived: [StudioThreadSummary] { browser.threads.filter(\.archived) }

    var body: some View {
        if browser.threadsLoading, browser.threads.isEmpty {
            VStack(spacing: 14) {
                ZeronPulse()
                Text("Loading threads…")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let error = browser.threadsError, browser.threads.isEmpty {
            ContentUnavailableView(
                "Threads unavailable",
                systemImage: "exclamationmark.triangle",
                description: Text(error)
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if active.isEmpty, archived.isEmpty {
            ContentUnavailableView(
                "No threads yet",
                systemImage: "rectangle.stack",
                description: Text("New Studio threads will appear here.")
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            List {
                ForEach(active) { thread in threadRow(thread) }
                if !archived.isEmpty {
                    DisclosureGroup("Archived (\(archived.count))", isExpanded: $archivedOpen) {
                        ForEach(archived) { thread in threadRow(thread) }
                    }
                    .font(Theme.sans(12, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
        }
    }

    private func threadRow(_ thread: StudioThreadSummary) -> some View {
        Button {
            path.append(.thread(thread.id))
        } label: {
            HStack(spacing: 12) {
                if let item = browser.gallery.last(where: { $0.conversationId == thread.id }) {
                    StudioPreviewView(item: item, browser: browser)
                        .frame(width: 52, height: 52)
                        .clipShape(RoundedRectangle(cornerRadius: 9))
                } else {
                    RoundedRectangle(cornerRadius: 9)
                        .fill(Theme.elementHover)
                        .frame(width: 52, height: 52)
                        .overlay { Image(systemName: "photo").foregroundStyle(Theme.textFaint) }
                }

                VStack(alignment: .leading, spacing: 4) {
                    Text(thread.title)
                        .font(Theme.sans(14, weight: .medium))
                        .foregroundStyle(thread.archived ? Theme.textMuted : Theme.text)
                        .lineLimit(1)
                    HStack(spacing: 6) {
                        Text("\(thread.turnCount) \(thread.turnCount == 1 ? "turn" : "turns")")
                        Text("·")
                        Text(thread.updatedDate.formatted(.relative(presentation: .named)))
                    }
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textFaint)
                }
                Spacer(minLength: 8)
                if thread.creating {
                    HStack(spacing: 6) {
                        MiniSpinner()
                        Text("Creating")
                    }
                    .font(Theme.sans(11, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                } else if thread.done {
                    Text("Done")
                        .font(Theme.sans(11, weight: .semibold))
                        .foregroundStyle(Theme.statusCompleted)
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .listRowBackground(Color.clear)
        .listRowSeparatorTint(Theme.border)
    }
}

struct StudioPreviewView: View {
    let item: StudioGalleryItem
    let browser: StudioBrowserStore

    var body: some View {
        StudioMediaPreviewView(
            artifactId: item.id,
            mediaKind: item.mediaKind,
            browser: browser,
            contentMode: .fill
        )
    }
}

struct StudioMediaPreviewView: View {
    @Environment(AppModel.self) private var model
    let artifactId: String
    let mediaKind: StudioMediaKind
    let browser: StudioBrowserStore
    var contentMode: ContentMode = .fill
    @State private var image: UIImage?

    init(
        artifactId: String,
        mediaKind: StudioMediaKind,
        browser: StudioBrowserStore,
        contentMode: ContentMode = .fill
    ) {
        self.artifactId = artifactId
        self.mediaKind = mediaKind
        self.browser = browser
        self.contentMode = contentMode
        _image = State(initialValue: browser.cachedPreview(artifactId: artifactId))
    }

    var body: some View {
        ZStack {
            Theme.elementHover
            if let image {
                Image(uiImage: image)
                    .resizable()
                    .aspectRatio(contentMode: contentMode)
            } else {
                Image(systemName: mediaKind == .video ? "film" : "photo")
                    .font(.system(size: 20, weight: .light))
                    .foregroundStyle(Theme.textFaint.opacity(0.5))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .task(id: "\(browser.selectedDeviceId ?? "none")-\(artifactId)") {
            guard let deviceId = browser.selectedDeviceId,
                  let workspace = model.workspace else { return }
            image = await browser.preview(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            )
        }
    }
}
