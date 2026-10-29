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
    /// What last went wrong: a load, send, import or pick. The next edit
    /// clears it (an evaluation doesn't: it would wipe an import's failure
    /// the moment it was shown); a failed load holds until it's retried.
    var error: String?
    /// The provider or its models didn't load: the notice offers a retry.
    var loadFailed = false
    /// The desktop has no Studio provider key: the composer offers to add one.
    var missingProvider = false
    var modelPickerRequested = false
    /// A send's reply: the whole thread with the new turn, ahead of the watch.
    @ObservationIgnored var onSent: ((StudioThread) -> Void)?

    @ObservationIgnored private var evaluationTask: Task<Void, Never>?
    @ObservationIgnored private var importTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored private var rememberedSelections: [StudioComposerMode: [StudioSelectedModel]] = [:]
    /// The desktop the provider, models and staged assets belong to.
    @ObservationIgnored private var loadedDeviceId: String?
    /// Bumped by a device switch: replies for the old desktop are dropped.
    @ObservationIgnored private var generation = 0

    init(threadId: String) {
        self.threadId = threadId
        snapshot = StudioComposerSnapshot(conversationId: threadId)
    }

    var prompt: String {
        get { snapshot.prompt }
        set {
            guard snapshot.prompt != newValue else { return }
            snapshot.prompt = newValue
            clearError()
            scheduleEvaluation()
        }
    }

    var canSend: Bool {
        !snapshot.prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && evaluation?.send.enabled == true
            && !sending
    }

    func load(workspace: StudioService, deviceId: String) async {
        if deviceId != loadedDeviceId { reset(for: deviceId) }
        guard !loading, provider == nil else { return }
        let generation = generation
        loading = true
        error = nil
        loadFailed = false
        missingProvider = false
        defer { if generation == self.generation { loading = false } }
        do {
            let providers = try await workspace.listStudioProviders(deviceId: deviceId)
            guard generation == self.generation else { return }
            guard let provider = providers.first(where: \.configured) else {
                missingProvider = true
                return
            }
            let response = try await workspace.listStudioModels(
                deviceId: deviceId,
                providerId: provider.providerId
            )
            guard generation == self.generation else { return }
            self.provider = provider
            models = response.models.filter {
                $0.operation != .imageEdit && $0.operation != .upscale
            }
            snapshot.catalogFetchedAt = response.fetchedAt
            // Keep a draft's models across a reload (a key just added);
            // otherwise start on the mode's first model.
            let outputKind: StudioMediaKind = snapshot.mode == .image ? .image : .video
            snapshot.selected.removeAll { selected in
                !models.contains { $0.id == selected.modelId && $0.outputKind == outputKind }
            }
            if snapshot.selected.isEmpty {
                guard let initial = models.first(where: { $0.outputKind == outputKind }) else {
                    throw outputKind == .image ? StudioComposerError.noImageModels : StudioComposerError.noVideoModels
                }
                snapshot.selected = [selection(for: initial)]
            }
            rememberedSelections[snapshot.mode] = snapshot.selected
            seedDurationIfNeeded()
            try await evaluate(workspace: workspace, deviceId: deviceId)
        } catch {
            guard generation == self.generation, !Task.isCancelled else { return }
            // Retrying starts over from the providers.
            provider = nil
            loadFailed = true
            self.error = error.localizedDescription
        }
    }

    /// A different desktop: its own providers, models and assets. The
    /// prompt and mode carry over; everything tied to the old host goes.
    private func reset(for deviceId: String) {
        loadedDeviceId = deviceId
        generation += 1
        evaluationTask?.cancel()
        importTasks.values.forEach { $0.cancel() }
        importTasks.removeAll()
        rememberedSelections.removeAll()
        provider = nil
        models = []
        evaluation = nil
        previews = [:]
        loading = false
        sending = false
        error = nil
        loadFailed = false
        missingProvider = false
        snapshot.selected = []
        snapshot.attachments = []
        snapshot.duration = nil
        snapshot.sourceTurnId = nil
        snapshot.catalogFetchedAt = nil
    }

    func setMode(
        _ mode: StudioComposerMode,
        workspace: StudioService,
        deviceId: String
    ) {
        guard snapshot.mode != mode else { return }
        clearError()
        rememberedSelections[snapshot.mode] = snapshot.selected
        snapshot.mode = mode
        snapshot.duration = nil
        for index in snapshot.selected.indices {
            snapshot.selected[index].controls.removeValue(forKey: "duration")
        }
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
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    func toggleModel(
        _ model: StudioComposerModel,
        workspace: StudioService,
        deviceId: String
    ) {
        clearError()
        if snapshot.selected.contains(where: { $0.modelId == model.id }) {
            snapshot.selected.removeAll { $0.modelId == model.id }
        } else {
            snapshot.selected.append(selection(for: model))
        }
        seedDurationIfNeeded()
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    func setDuration(
        _ value: StudioControlValue,
        workspace: StudioService,
        deviceId: String
    ) {
        clearError()
        applyDuration(value)
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    func setControl(
        modelId: String,
        controlId: String,
        value: StudioControlValue,
        workspace: StudioService,
        deviceId: String
    ) {
        guard let index = snapshot.selected.firstIndex(where: { $0.modelId == modelId }) else {
            return
        }
        clearError()
        snapshot.selected[index].controls[controlId] = value
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    func adjustOutputCount(
        modelId: String,
        delta: Int,
        workspace: StudioService,
        deviceId: String
    ) {
        guard snapshot.mode == .image,
              let index = snapshot.selected.firstIndex(where: { $0.modelId == modelId }),
              let model = models.first(where: { $0.id == modelId }) else { return }
        clearError()
        let current = Int(snapshot.selected[index].outputCount)
        snapshot.selected[index].outputCount = UInt32(
            min(max(current + delta, 1), Int(max(model.maximumOutputCount, 1)))
        )
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    /// Stages a reference and uploads it. A multi-pick reports one item's
    /// failure without the next item's start clearing it, so this leaves
    /// `error` alone.
    func importAttachment(
        data: Data,
        mimeType: String,
        kind: StudioComposerMediaKind,
        preview: UIImage?,
        pixelSize: CGSize?,
        workspace: StudioService,
        deviceId: String
    ) {
        guard deviceId == loadedDeviceId else { return }
        let assetId = UUID().uuidString.lowercased()
        let pending = StudioComposerAttachment(
            id: assetId,
            kind: kind,
            pending: true,
            origin: .asset,
            mimeType: mimeType,
            byteSize: UInt64(data.count),
            width: pixelSize.map { UInt32($0.width) },
            height: pixelSize.map { UInt32($0.height) },
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
                self.importTasks.removeValue(forKey: assetId)
                guard let index = self.snapshot.attachments.firstIndex(where: { $0.id == assetId })
                else { return }
                self.snapshot.attachments[index] = committed
                try await self.evaluate(workspace: workspace, deviceId: deviceId)
            } catch {
                // Removed (or the desktop switched) while uploading: not a failure.
                guard !Task.isCancelled, !(error is CancellationError) else { return }
                self.importTasks.removeValue(forKey: assetId)
                guard self.snapshot.attachments.contains(where: { $0.id == assetId }) else { return }
                self.snapshot.attachments.removeAll { $0.id == assetId }
                self.previews.removeValue(forKey: assetId)
                self.error = error.localizedDescription
                try? await self.evaluate(workspace: workspace, deviceId: deviceId)
            }
        }
    }

    func removeAttachment(
        _ assetId: String,
        workspace: StudioService,
        deviceId: String
    ) {
        clearError()
        importTasks.removeValue(forKey: assetId)?.cancel()
        snapshot.attachments.removeAll { $0.id == assetId }
        previews.removeValue(forKey: assetId)
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    func send(workspace: StudioService, deviceId: String) {
        guard canSend, deviceId == loadedDeviceId else { return }
        let trimmed = snapshot.prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        var request = snapshot
        request.prompt = trimmed
        let generation = generation
        sending = true
        clearError()
        Task {
            defer { if generation == self.generation { sending = false } }
            do {
                let thread = try await workspace.createStudioTurn(deviceId: deviceId, snapshot: request)
                guard generation == self.generation else { return }
                onSent?(thread)
                // Clear what was sent; anything typed or attached while it
                // went out stays in the draft.
                if snapshot.prompt.trimmingCharacters(in: .whitespacesAndNewlines) == trimmed {
                    snapshot.prompt = ""
                }
                let sent = Set(request.attachments.map(\.id))
                snapshot.attachments.removeAll { sent.contains($0.id) }
                previews = previews.filter { !sent.contains($0.key) }
                snapshot.sourceTurnId = nil
                try? await evaluate(workspace: workspace, deviceId: deviceId)
            } catch {
                guard generation == self.generation else { return }
                self.error = error.localizedDescription
            }
        }
    }

    func applyConflictAction(
        _ action: StudioConflictAction,
        workspace: StudioService,
        deviceId: String
    ) {
        guard case .object(let object) = action.action,
              let type = object["type"]?.stringValue else {
            error = "Studio returned an invalid conflict action"
            return
        }
        clearError()
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
            applyDuration(decode(StudioControlValue.self, from: object["value"]))
        case "clear_duration":
            applyDuration(nil)
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
            applyDuration(decode(StudioControlValue.self, from: object["duration"]))
        case "dismiss_warn": break
        default:
            error = "This Studio resolution is not available on iPhone yet"
            return
        }
        reevaluate(workspace: workspace, deviceId: deviceId)
    }

    private func refreshCatalog(workspace: StudioService, deviceId: String) async {
        guard let provider else { return }
        let generation = generation
        do {
            let response = try await workspace.listStudioModels(
                deviceId: deviceId,
                providerId: provider.providerId,
                refresh: true
            )
            guard generation == self.generation else { return }
            models = response.models.filter { $0.operation != .imageEdit && $0.operation != .upscale }
            snapshot.catalogFetchedAt = response.fetchedAt
            try await evaluate(workspace: workspace, deviceId: deviceId)
        } catch {
            guard generation == self.generation else { return }
            self.error = error.localizedDescription
        }
    }

    /// Supersedes any debounced evaluation: this edit's reply covers it.
    private func reevaluate(workspace: StudioService, deviceId: String) {
        evaluationTask?.cancel()
        evaluationTask = Task { [weak self] in
            try? await self?.evaluate(workspace: workspace, deviceId: deviceId)
        }
    }

    private func scheduleEvaluation() {
        evaluationTask?.cancel()
        evaluationTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(140))
            guard let self, !Task.isCancelled,
                  let workspace = self.currentWorkspace,
                  let deviceId = self.loadedDeviceId else { return }
            try? await self.evaluate(workspace: workspace, deviceId: deviceId)
        }
    }

    @ObservationIgnored private weak var currentWorkspace: StudioService?

    private func evaluate(workspace: StudioService, deviceId: String) async throws {
        currentWorkspace = workspace
        guard deviceId == loadedDeviceId else { return }
        guard let provider else { throw StudioComposerError.noConfiguredProvider }
        let requested = snapshot
        let generation = generation
        let view = try await workspace.evaluateStudioComposer(
            deviceId: deviceId,
            snapshot: requested,
            providerId: provider.providerId
        )
        // Out of order, or for a draft that has since changed: a newer
        // evaluation is on its way.
        guard generation == self.generation, requested == snapshot else { return }
        let previous = evaluation
        evaluation = view
        // The host asks for the picker on every evaluation while nothing is
        // selected; open it once, not again on each keystroke after it's
        // dismissed.
        if view.openPicker, previous?.openPicker != true { modelPickerRequested = true }
    }

    private func clearError() {
        guard !loadFailed else { return }
        error = nil
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

    /// The global duration, mirrored onto every selected model's controls.
    private func applyDuration(_ value: StudioControlValue?) {
        snapshot.duration = value
        for index in snapshot.selected.indices {
            if let value {
                snapshot.selected[index].controls["duration"] = value
            } else {
                snapshot.selected[index].controls.removeValue(forKey: "duration")
            }
        }
    }

    /// Video drafts always carry a duration, on every selected model (one
    /// added later included).
    private func seedDurationIfNeeded() {
        guard snapshot.mode == .video else {
            snapshot.duration = nil
            return
        }
        if snapshot.duration == nil {
            let selectedIds = Set(snapshot.selected.map(\.modelId))
            let durations = models
                .filter { selectedIds.contains($0.id) }
                .flatMap(\.controls)
                .first(where: { $0.id == "duration" })
            snapshot.duration = durations?.default ?? durations?.choices.first?.value
        }
        applyDuration(snapshot.duration)
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
    case noVideoModels

    var errorDescription: String? {
        switch self {
        case .noConfiguredProvider: "Configure a Studio provider on the desktop first."
        case .noImageModels: "The configured Studio provider has no image models."
        case .noVideoModels: "The configured Studio provider has no video models."
        }
    }
}
