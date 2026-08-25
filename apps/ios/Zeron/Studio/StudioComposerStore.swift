import Observation
import UIKit

@MainActor
@Observable
final class StudioComposerStore {
    let threadId: String
    var snapshot: StudioComposerSnapshot
    var evaluation: StudioComposerEvaluation?
    var models: [StudioComposerModel] = []
    var provider: StudioProviderConnection?
    var previews: [String: UIImage] = [:]
    var loading = false
    var sending = false
    var error: String?
    var modelPickerRequested = false

    @ObservationIgnored private var evaluationTask: Task<Void, Never>?
    @ObservationIgnored private var importTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var rememberedSelections: [StudioComposerMode: [StudioSelectedModel]] = [:]

    init(threadId: String) {
        self.threadId = threadId
        snapshot = StudioComposerSnapshot(conversationId: threadId)
    }

    var prompt: String {
        get { snapshot.prompt }
        set {
            guard snapshot.prompt != newValue else { return }
            snapshot.prompt = newValue
            scheduleEvaluation()
        }
    }

    var canSend: Bool {
        !snapshot.prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && evaluation?.send.enabled == true
            && !sending
    }

    var selectedModels: [StudioComposerModel] {
        let ids = Set(snapshot.selected.map(\.modelId))
        return models.filter { ids.contains($0.id) }
    }

    func load(workspace: WorkspaceStore, deviceId: String) async {
        guard !loading, provider == nil else { return }
        loading = true
        error = nil
        defer { loading = false }
        do {
            let providers = try await workspace.listStudioProviders(deviceId: deviceId)
            guard let provider = providers.first(where: \.configured) else {
                throw StudioComposerError.noConfiguredProvider
            }
            self.provider = provider
            let response = try await workspace.listStudioModels(
                deviceId: deviceId,
                providerId: provider.providerId
            )
            models = response.models.filter {
                $0.operation != .imageEdit && $0.operation != .upscale
            }
            snapshot.catalogFetchedAt = response.fetchedAt
            guard let initial = models.first(where: { $0.outputKind == .image }) else {
                throw StudioComposerError.noImageModels
            }
            snapshot.selected = [selection(for: initial)]
            rememberedSelections[.image] = snapshot.selected
            try await evaluate(workspace: workspace, deviceId: deviceId)
        } catch {
            self.error = error.localizedDescription
        }
    }

    func setMode(
        _ mode: StudioComposerMode,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard snapshot.mode != mode else { return }
        rememberedSelections[snapshot.mode] = snapshot.selected
        snapshot.mode = mode
        snapshot.duration = nil
        let outputKind: StudioMediaKind = mode == .image ? .image : .video
        let restored = rememberedSelections[mode]?.filter { selected in
            models.contains { $0.id == selected.modelId && $0.outputKind == outputKind }
        } ?? []
        if restored.isEmpty, let first = models.first(where: { $0.outputKind == outputKind }) {
            snapshot.selected = [selection(for: first)]
        } else {
            snapshot.selected = restored
        }
        seedDurationIfNeeded()
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    func toggleModel(
        _ model: StudioComposerModel,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        if snapshot.selected.contains(where: { $0.modelId == model.id }) {
            snapshot.selected.removeAll { $0.modelId == model.id }
        } else {
            snapshot.selected.append(selection(for: model))
        }
        seedDurationIfNeeded()
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    func setDuration(
        _ value: StudioControlValue,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        snapshot.duration = value
        for index in snapshot.selected.indices {
            snapshot.selected[index].controls["duration"] = value
        }
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    func setControl(
        modelId: String,
        controlId: String,
        value: StudioControlValue,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard let index = snapshot.selected.firstIndex(where: { $0.modelId == modelId }) else {
            return
        }
        snapshot.selected[index].controls[controlId] = value
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    func adjustOutputCount(
        modelId: String,
        delta: Int,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard snapshot.mode == .image,
              let index = snapshot.selected.firstIndex(where: { $0.modelId == modelId }),
              let model = models.first(where: { $0.id == modelId }) else { return }
        let current = Int(snapshot.selected[index].outputCount)
        snapshot.selected[index].outputCount = UInt32(
            min(max(current + delta, 1), Int(max(model.maximumOutputCount, 1)))
        )
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    func importAttachment(
        data: Data,
        mimeType: String,
        kind: StudioComposerMediaKind,
        preview: UIImage?,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        let assetId = UUID().uuidString.lowercased()
        let pending = StudioComposerAttachment(
            id: assetId,
            kind: kind,
            pending: true,
            origin: .asset,
            mimeType: mimeType,
            byteSize: UInt64(data.count),
            width: preview.map { UInt32($0.size.width * $0.scale) },
            height: preview.map { UInt32($0.size.height * $0.scale) },
            durationSeconds: nil,
            contentHash: "",
            roleHint: nil
        )
        snapshot.attachments.append(pending)
        if let preview { previews[assetId] = preview }
        scheduleEvaluation()
        importTasks[assetId] = Task { [weak self] in
            guard let self else { return }
            do {
                let committed = try await workspace.importStudioAsset(
                    deviceId: deviceId,
                    assetId: assetId,
                    data: data,
                    mimeType: mimeType
                )
                guard !Task.isCancelled else { return }
                guard let index = self.snapshot.attachments.firstIndex(where: { $0.id == assetId })
                else {
                    self.importTasks.removeValue(forKey: assetId)
                    return
                }
                self.snapshot.attachments[index] = committed
                self.importTasks.removeValue(forKey: assetId)
                try await self.evaluate(workspace: workspace, deviceId: deviceId)
            } catch is CancellationError {
                self.importTasks.removeValue(forKey: assetId)
            } catch {
                self.snapshot.attachments.removeAll { $0.id == assetId }
                self.previews.removeValue(forKey: assetId)
                self.importTasks.removeValue(forKey: assetId)
                self.error = error.localizedDescription
                try? await self.evaluate(workspace: workspace, deviceId: deviceId)
            }
        }
    }

    func removeAttachment(
        _ assetId: String,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        importTasks.removeValue(forKey: assetId)?.cancel()
        snapshot.attachments.removeAll { $0.id == assetId }
        previews.removeValue(forKey: assetId)
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    func send(workspace: WorkspaceStore, deviceId: String) {
        let trimmed = snapshot.prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, canSend else { return }
        snapshot.prompt = trimmed
        sending = true
        error = nil
        Task {
            defer { sending = false }
            do {
                _ = try await workspace.createStudioTurn(deviceId: deviceId, snapshot: snapshot)
                snapshot.prompt = ""
                snapshot.attachments = []
                previews = [:]
                snapshot.sourceTurnId = nil
                try await evaluate(workspace: workspace, deviceId: deviceId)
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    func applyConflictAction(
        _ action: StudioConflictAction,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard case .object(let object) = action.action,
              let type = object["type"]?.stringValue else {
            error = "Studio returned an invalid conflict action"
            return
        }
        switch type {
        case "remove_unsupported_references":
            let ids = stringArray(object["asset_ids"])
            for id in ids { importTasks.removeValue(forKey: id)?.cancel() }
            snapshot.attachments.removeAll { ids.contains($0.id) }
            for id in ids { previews.removeValue(forKey: id) }
        case "remove_all_attachments":
            importTasks.values.forEach { $0.cancel() }
            importTasks.removeAll()
            snapshot.attachments = []
            previews = [:]
        case "deselect_incompatible_models", "drop_vanished_models":
            let ids = stringArray(object["model_ids"])
            snapshot.selected.removeAll { ids.contains($0.modelId) }
        case "keep_models_drop_others":
            let ids = stringArray(object["model_ids"])
            snapshot.selected.removeAll { !ids.contains($0.modelId) }
        case "clamp_duration":
            snapshot.duration = decode(StudioControlValue.self, from: object["value"])
        case "clear_duration":
            snapshot.duration = nil
            for index in snapshot.selected.indices {
                snapshot.selected[index].controls.removeValue(forKey: "duration")
            }
        case "open_model_picker":
            modelPickerRequested = true
        case "refresh_catalog":
            Task { await refreshCatalog(workspace: workspace, deviceId: deviceId) }
            return
        case "shorten_prompt":
            if let maximum = object["maximum_chars"]?.int64Value {
                snapshot.prompt = String(snapshot.prompt.prefix(Int(maximum)))
            }
        case "clear_prompt": snapshot.prompt = ""
        case "switch_mode":
            guard let raw = object["mode"]?.stringValue,
                  let mode = StudioComposerMode(rawValue: raw) else { return }
            setMode(mode, workspace: workspace, deviceId: deviceId)
            return
        case "reset_control":
            guard let modelId = object["model_id"]?.stringValue,
                  let controlId = object["control_id"]?.stringValue,
                  let value = decode(StudioControlValue.self, from: object["value"]) else { return }
            if let index = snapshot.selected.firstIndex(where: { $0.modelId == modelId }) {
                snapshot.selected[index].controls[controlId] = value
            }
        case "pin_attachment_role":
            guard let assetId = object["asset_id"]?.stringValue,
                  let role = object["role"]?.stringValue,
                  let index = snapshot.attachments.firstIndex(where: { $0.id == assetId })
            else { return }
            snapshot.attachments[index].roleHint = role
        case "revert_model_selection":
            if let restored = decode([StudioSelectedModel].self, from: object["selected"]) {
                snapshot.selected = restored
            }
        case "revert_mode":
            guard let raw = object["mode"]?.stringValue,
                  let mode = StudioComposerMode(rawValue: raw) else { return }
            snapshot.mode = mode
            if let restored = decode([StudioSelectedModel].self, from: object["selected"]) {
                snapshot.selected = restored
            }
            snapshot.duration = decode(StudioControlValue.self, from: object["duration"])
        case "dismiss_warn": break
        default:
            error = "This Studio resolution is not available on iPhone yet"
            return
        }
        Task { try? await evaluate(workspace: workspace, deviceId: deviceId) }
    }

    private func refreshCatalog(workspace: WorkspaceStore, deviceId: String) async {
        guard let provider else { return }
        do {
            let response = try await workspace.listStudioModels(
                deviceId: deviceId,
                providerId: provider.providerId,
                refresh: true
            )
            models = response.models.filter { $0.operation != .imageEdit && $0.operation != .upscale }
            snapshot.catalogFetchedAt = response.fetchedAt
            try await evaluate(workspace: workspace, deviceId: deviceId)
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func scheduleEvaluation() {
        evaluationTask?.cancel()
        evaluationTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(140))
            guard let self, !Task.isCancelled,
                  let workspace = self.currentWorkspace,
                  let deviceId = self.currentDeviceId else { return }
            try? await self.evaluate(workspace: workspace, deviceId: deviceId)
        }
    }

    @ObservationIgnored private weak var currentWorkspace: WorkspaceStore?
    @ObservationIgnored private var currentDeviceId: String?

    private func evaluate(workspace: WorkspaceStore, deviceId: String) async throws {
        currentWorkspace = workspace
        currentDeviceId = deviceId
        guard let provider else { throw StudioComposerError.noConfiguredProvider }
        let requested = snapshot
        let view = try await workspace.evaluateStudioComposer(
            deviceId: deviceId,
            snapshot: requested,
            providerId: provider.providerId
        )
        guard requested == snapshot else { return }
        evaluation = view
        error = nil
        if view.openPicker { modelPickerRequested = true }
    }

    private func selection(for model: StudioComposerModel) -> StudioSelectedModel {
        var controls: [String: StudioControlValue] = [:]
        for control in model.controls where control.id != "duration" {
            if let value = control.default ?? control.choices.first?.value {
                controls[control.id] = value
            }
        }
        return StudioSelectedModel(
            providerId: model.providerId,
            modelId: model.id,
            outputCount: 1,
            controls: controls
        )
    }

    private func seedDurationIfNeeded() {
        guard snapshot.mode == .video else {
            snapshot.duration = nil
            return
        }
        guard snapshot.duration == nil else { return }
        let selectedIds = Set(snapshot.selected.map(\.modelId))
        let durations = models
            .filter { selectedIds.contains($0.id) }
            .flatMap(\.controls)
            .first(where: { $0.id == "duration" })
        snapshot.duration = durations?.default ?? durations?.choices.first?.value
        if let duration = snapshot.duration {
            for index in snapshot.selected.indices {
                snapshot.selected[index].controls["duration"] = duration
            }
        }
    }

    private func stringArray(_ value: JSONValue?) -> Set<String> {
        guard case .array(let values) = value else { return [] }
        return Set(values.compactMap(\.stringValue))
    }

    private func decode<T: Decodable>(_ type: T.Type, from value: JSONValue?) -> T? {
        guard let value, value != .null,
              let data = try? JSONEncoder().encode(value) else { return nil }
        return try? JSONDecoder().decode(type, from: data)
    }
}

private enum StudioComposerError: LocalizedError {
    case noConfiguredProvider
    case noImageModels

    var errorDescription: String? {
        switch self {
        case .noConfiguredProvider: "Configure a Studio provider on the desktop first."
        case .noImageModels: "The configured Studio provider has no image models."
        }
    }
}
