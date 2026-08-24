import SwiftUI

enum StudioRoute: Hashable {
    case thread(String)
    case artifact(String)
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
                            .font(Theme.sans(16, weight: .medium))
                            .foregroundStyle(Theme.text)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                            .background {
                                if selection == mode {
                                    Capsule()
                                        .fill(Theme.bg.opacity(0.66))
                                        .matchedGeometryEffect(
                                            id: "studio-library-selection",
                                            in: selectionMotion
                                        )
                                }
                            }
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(selection == mode ? .isSelected : [])
                }
            }
            .padding(5)
            .frame(width: 220, height: 52)
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
                }
                .scrollEdgeEffectStyle(.soft, for: .top)
            .navigationDestination(for: StudioRoute.self) { route in
                switch route {
                case .thread:
                    StudioDestinationPlaceholder(title: "Thread", symbol: "rectangle.stack")
                case .artifact:
                    StudioDestinationPlaceholder(title: "Artifact", symbol: "photo")
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
                StudioGalleryView(browser: browser, path: $path)
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

}

private struct StudioGalleryView: View {
    @Environment(AppModel.self) private var model
    let browser: StudioBrowserStore
    @Binding var path: [StudioRoute]

    private let columns = Array(repeating: GridItem(.flexible(), spacing: 2), count: 3)

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
            ScrollView {
                LazyVGrid(columns: columns, spacing: 2) {
                    ForEach(browser.gallery) { item in
                        Button {
                            path.append(.artifact(item.id))
                        } label: {
                            Rectangle()
                                .fill(Theme.elementHover)
                                .aspectRatio(1, contentMode: .fit)
                                .overlay {
                                    StudioPreviewView(item: item, browser: browser)
                                }
                                .clipped()
                                .overlay(alignment: .bottomTrailing) {
                                    if item.mediaKind == .video {
                                        Label(duration(item.durationSeconds), systemImage: "play.fill")
                                            .font(Theme.sans(10, weight: .semibold))
                                            .foregroundStyle(.white)
                                            .padding(5)
                                            .shadow(radius: 2)
                                    }
                                }
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel("\(item.modelDisplayName), \(item.prompt)")
                        .task {
                            guard browser.shouldLoadMore(after: item),
                                  let workspace = model.workspace,
                                  let deviceId = browser.selectedDeviceId else { return }
                            await browser.loadMoreGallery(
                                workspace: workspace,
                                deviceId: deviceId
                            )
                        }
                    }
                }
                if browser.galleryLoadingMore {
                    ProgressView()
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 16)
                }
            }
            .scrollEdgeEffectStyle(.soft, for: .top)
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

    private func duration(_ seconds: Double?) -> String {
        guard let seconds else { return "Video" }
        let total = max(0, Int(seconds.rounded()))
        return String(format: "%d:%02d", total / 60, total % 60)
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
                if let item = browser.gallery.first(where: { $0.conversationId == thread.id }) {
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
    @Environment(AppModel.self) private var model
    let item: StudioGalleryItem
    let browser: StudioBrowserStore
    @State private var image: UIImage?

    var body: some View {
        ZStack {
            Theme.elementHover
            if let image {
                Image(uiImage: image)
                    .resizable()
                    .scaledToFill()
            } else {
                Image(systemName: item.mediaKind == .video ? "film" : "photo")
                    .font(.system(size: 20, weight: .light))
                    .foregroundStyle(Theme.textFaint.opacity(0.5))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .task(id: "\(browser.selectedDeviceId ?? "none")-\(item.id)") {
            guard let deviceId = browser.selectedDeviceId,
                  let workspace = model.workspace else { return }
            image = await browser.preview(
                artifactId: item.id,
                deviceId: deviceId,
                workspace: workspace
            )
        }
    }
}

private struct StudioDestinationPlaceholder: View {
    let title: String
    let symbol: String

    var body: some View {
        ContentUnavailableView(title, systemImage: symbol)
            .background(Theme.bg.ignoresSafeArea())
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
    }
}
