import Observation
import SwiftUI

@MainActor
@Observable
private final class StudioThreadStore {
    var thread: StudioThread?
    var loading = true
    var error: String?

    func watch(workspace: WorkspaceStore, deviceId: String, threadId: String) async {
        loading = true
        error = nil
        do {
            let stream = try await workspace.watchStudioThread(
                deviceId: deviceId,
                threadId: threadId
            )
            for try await thread in stream {
                guard !Task.isCancelled else { return }
                self.thread = thread
                loading = false
            }
        } catch {
            guard !Task.isCancelled else { return }
            loading = false
            self.error = error.localizedDescription
        }
    }
}

struct StudioThreadView: View {
    @Environment(AppModel.self) private var model
    let threadId: String
    let browser: StudioBrowserStore
    @Binding var path: [StudioRoute]
    @State private var store = StudioThreadStore()

    private let columns = [
        GridItem(.flexible(), spacing: 3),
        GridItem(.flexible(), spacing: 3),
    ]

    var body: some View {
        Group {
            if store.loading, store.thread == nil {
                VStack(spacing: 14) {
                    ZeronPulse()
                    Text("Loading thread…")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let error = store.error, store.thread == nil {
                ContentUnavailableView(
                    "Thread unavailable",
                    systemImage: "exclamationmark.triangle",
                    description: Text(error)
                )
            } else if let thread = store.thread {
                threadFeed(thread)
            }
        }
        .background(Theme.bg.ignoresSafeArea())
        .navigationTitle(store.thread?.conversation.title ?? "Thread")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(.hidden, for: .tabBar)
        .task(id: "\(browser.selectedDeviceId ?? "none")-\(threadId)") {
            guard let workspace = model.workspace,
                  let deviceId = browser.selectedDeviceId else { return }
            await workspace.markStudioThreadSeen(deviceId: deviceId, threadId: threadId)
            await store.watch(workspace: workspace, deviceId: deviceId, threadId: threadId)
        }
    }

    private func threadFeed(_ thread: StudioThread) -> some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 24) {
                ForEach(thread.turns.sorted(by: { $0.position < $1.position })) { turn in
                    VStack(alignment: .leading, spacing: 14) {
                        Text(turn.prompt)
                            .font(Theme.sans(16, weight: .medium))
                            .foregroundStyle(Theme.text)
                            .fixedSize(horizontal: false, vertical: true)

                        ForEach(turn.runs.sorted(by: { $0.position < $1.position })) { run in
                            runView(run, turn: turn)
                        }
                    }
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 18)
        }
        .scrollEdgeEffectStyle(.soft, for: .top)
    }

    private func runView(_ run: StudioRun, turn: StudioTurn) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text(run.model.displayName)
                    .font(Theme.sans(12, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                Spacer()
                runStatus(run)
            }

            if !run.artifacts.isEmpty {
                LazyVGrid(columns: columns, spacing: 3) {
                    ForEach(run.artifacts.sorted(by: { $0.outputPosition < $1.outputPosition })) {
                        artifact in
                        Button {
                            path.append(.artifact(StudioArtifactDetail(
                                artifact: artifact,
                                turn: turn,
                                run: run
                            )))
                        } label: {
                            Rectangle()
                                .fill(Theme.elementHover)
                                .aspectRatio(1, contentMode: .fit)
                                .overlay {
                                    StudioMediaPreviewView(
                                        artifactId: artifact.id,
                                        mediaKind: artifact.mediaKind,
                                        browser: browser,
                                        contentMode: .fill
                                    )
                                }
                                .clipped()
                                .overlay(alignment: .bottomTrailing) {
                                    if artifact.mediaKind == .video {
                                        Image(systemName: "play.fill")
                                            .font(.system(size: 11, weight: .semibold))
                                            .foregroundStyle(.white)
                                            .padding(7)
                                            .shadow(radius: 2)
                                    }
                                }
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel("Open \(artifact.mediaKind.rawValue) from \(run.model.displayName)")
                    }
                }
            } else if let error = run.error, !error.isEmpty {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.dangerSoft)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    @ViewBuilder private func runStatus(_ run: StudioRun) -> some View {
        if run.state.isCreating {
            HStack(spacing: 7) {
                if let progress = run.progress {
                    ProgressView(value: Double(progress))
                        .frame(width: 46)
                } else {
                    MiniSpinner()
                }
                Text(run.state.label)
            }
            .font(Theme.sans(11))
            .foregroundStyle(Theme.textFaint)
        } else {
            Text(run.state.label)
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(run.state == .succeeded ? Theme.statusCompleted : Theme.textFaint)
        }
    }
}

struct StudioArtifactView: View {
    let artifact: StudioArtifactDetail
    let browser: StudioBrowserStore

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Rectangle()
                    .fill(Theme.elementHover)
                    .aspectRatio(artifact.aspectRatio, contentMode: .fit)
                    .overlay {
                        StudioMediaPreviewView(
                            artifactId: artifact.id,
                            mediaKind: artifact.mediaKind,
                            browser: browser,
                            contentMode: .fit
                        )
                    }
                    .clipShape(RoundedRectangle(cornerRadius: 14))
                    .overlay(alignment: .center) {
                        if artifact.mediaKind == .video {
                            Image(systemName: "play.slash")
                                .font(.system(size: 28, weight: .medium))
                                .foregroundStyle(.white)
                                .padding(16)
                                .glassEffect(.regular, in: Circle())
                        }
                    }

                VStack(alignment: .leading, spacing: 12) {
                    Text(artifact.prompt)
                        .font(Theme.sans(16, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .fixedSize(horizontal: false, vertical: true)

                    LabeledContent("Model", value: artifact.modelDisplayName)
                    if let dimensions = dimensions {
                        LabeledContent("Dimensions", value: dimensions)
                    }
                    if let duration = duration {
                        LabeledContent("Duration", value: duration)
                    }
                    LabeledContent("Size", value: ByteCountFormatter.string(
                        fromByteCount: Int64(artifact.sizeBytes),
                        countStyle: .file
                    ))
                    LabeledContent(
                        "Created",
                        value: artifact.createdDate.formatted(.relative(presentation: .named))
                    )
                }
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)

                if artifact.mediaKind == .video {
                    Text("Video playback is not available on iPhone yet. This is the optimized preview.")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textFaint)
                        .fixedSize(horizontal: false, vertical: true)
                } else {
                    Text("Optimized preview")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textFaint)
                }
            }
            .padding(16)
        }
        .background(Theme.bg.ignoresSafeArea())
        .navigationTitle(artifact.mediaKind == .video ? "Video" : "Image")
        .navigationBarTitleDisplayMode(.inline)
        .scrollEdgeEffectStyle(.soft, for: .top)
    }

    private var dimensions: String? {
        guard let width = artifact.width, let height = artifact.height else { return nil }
        return "\(width) × \(height)"
    }

    private var duration: String? {
        guard let seconds = artifact.durationSeconds else { return nil }
        let total = max(0, Int(seconds.rounded()))
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}

private extension StudioRunState {
    var label: String {
        switch self {
        case .draft: "Draft"
        case .quoting: "Quoting"
        case .awaitingConfirmation: "Awaiting confirmation"
        case .queued: "Queued"
        case .running: "Creating"
        case .downloading: "Downloading"
        case .succeeded: "Done"
        case .failed: "Failed"
        case .cancelling: "Cancelling"
        case .cancelled: "Cancelled"
        }
    }
}
