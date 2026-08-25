import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

struct StudioComposerView: View {
    @Environment(AppModel.self) private var appModel
    let store: StudioComposerStore
    let browser: StudioBrowserStore

    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPhotoPicker = false
    @State private var showFilePicker = false
    @State private var showModelPicker = false
    @State private var configuredModelId: String?
    @State private var shownConflict: StudioComposerConflict?

    private var workspace: WorkspaceStore? { appModel.workspace }
    private var deviceId: String? { browser.selectedDeviceId }

    var body: some View {
        VStack(spacing: 6) {
            if let error = store.error {
                Text(error)
                    .font(Theme.sans(11.5))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 24)
            } else if let conflict = store.evaluation?.conflicts.first {
                Button { shownConflict = conflict } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "exclamationmark.circle")
                        Text(conflict.title).lineLimit(1)
                        Spacer(minLength: 4)
                        Image(systemName: "chevron.up")
                            .font(.system(size: 9, weight: .semibold))
                    }
                    .font(Theme.sans(11.5, weight: .medium))
                    .foregroundStyle(Theme.warning)
                    .padding(.horizontal, 24)
                }
                .buttonStyle(.plain)
            }

            ComposerShell(
                draft: Binding(get: { store.prompt }, set: { store.prompt = $0 }),
                placeholder: store.snapshot.mode == .image
                    ? "Describe the image you want to create"
                    : "Describe the video you want to create",
                sendEnabled: store.canSend,
                showStop: false,
                busy: store.sending,
                keepExpanded: showPhotoPicker || showFilePicker || showModelPicker
                    || configuredModelId != nil || shownConflict != nil,
                onSend: send,
                externalTray: tray,
                externalTrayForcesExpanded: !store.snapshot.attachments.isEmpty
            ) {
                ComposerChip(label: store.snapshot.mode == .image ? "Image" : "Video") {
                    switchMode()
                }
                if store.snapshot.mode == .video,
                   let duration = store.evaluation?.globals.duration {
                    StudioDurationChip(
                        value: duration,
                        choices: store.evaluation?.globals.durationChoices ?? [],
                        select: setDuration
                    )
                }
                if let chips = store.evaluation?.models, !chips.isEmpty {
                    ForEach(chips) { chip in
                        ComposerChip(label: modelChipLabel(chip)) {
                            configuredModelId = chip.modelId
                        }
                    }
                }
                ComposerChip(label: "Models") { showModelPicker = true }
            }
        }
        .photosPicker(
            isPresented: $showPhotoPicker,
            selection: $pickerItems,
            maxSelectionCount: 12,
            matching: .any(of: [.images, .videos])
        )
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stagePhotoItems(items)
        }
        .fileImporter(
            isPresented: $showFilePicker,
            allowedContentTypes: [.image, .movie, .audio],
            allowsMultipleSelection: true
        ) { result in
            if case .success(let urls) = result { stageFiles(urls) }
            if case .failure(let error) = result { store.error = error.localizedDescription }
        }
        .sheet(isPresented: $showModelPicker) {
            if let workspace, let deviceId {
                StudioModelPickerSheet(
                    store: store,
                    workspace: workspace,
                    deviceId: deviceId
                )
            }
        }
        .sheet(isPresented: Binding(
            get: { configuredModelId != nil },
            set: { if !$0 { configuredModelId = nil } }
        )) {
            if let workspace, let deviceId,
               let model = store.models.first(where: { $0.id == configuredModelId }) {
                StudioModelConfigurationSheet(
                    model: model,
                    store: store,
                    workspace: workspace,
                    deviceId: deviceId
                )
            }
        }
        .sheet(item: $shownConflict) { conflict in
            StudioConflictSheet(conflict: conflict) { action in
                guard let workspace, let deviceId else { return }
                store.applyConflictAction(action, workspace: workspace, deviceId: deviceId)
                shownConflict = nil
            }
        }
        .onChange(of: store.modelPickerRequested) { _, requested in
            guard requested else { return }
            store.modelPickerRequested = false
            showModelPicker = true
        }
    }

    private var tray: AnyView? {
        guard let evaluation = store.evaluation,
              evaluation.attachments.addEnabled || !evaluation.attachments.items.isEmpty else {
            return nil
        }
        return AnyView(StudioReferenceTray(
            attachments: evaluation.attachments.items,
            previews: store.previews,
            budgets: evaluation.budgets,
            addPhoto: { showPhotoPicker = true },
            addFile: { showFilePicker = true },
            remove: removeAttachment
        ))
    }

    private func switchMode() {
        guard let workspace, let deviceId else { return }
        store.setMode(
            store.snapshot.mode == .image ? .video : .image,
            workspace: workspace,
            deviceId: deviceId
        )
    }

    private func setDuration(_ value: StudioControlValue) {
        guard let workspace, let deviceId else { return }
        store.setDuration(value, workspace: workspace, deviceId: deviceId)
    }

    private func send() {
        guard let workspace, let deviceId else { return }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        store.send(workspace: workspace, deviceId: deviceId)
    }

    private func removeAttachment(_ id: String) {
        guard let workspace, let deviceId else { return }
        store.removeAttachment(id, workspace: workspace, deviceId: deviceId)
    }

    private func modelChipLabel(_ chip: StudioComposerChip) -> String {
        var readouts: [String] = []
        if store.snapshot.mode == .image { readouts.append("\(chip.outputCount)×") }
        for id in ["aspect_ratio", "resolution"] {
            if let value = chip.values[id] { readouts.append(value.label) }
        }
        return ([chip.displayName] + readouts).joined(separator: " · ")
    }

    private func stagePhotoItems(_ items: [PhotosPickerItem]) {
        guard let workspace, let deviceId else { return }
        Task {
            defer { pickerItems = [] }
            for item in items {
                guard let data = try? await item.loadTransferable(type: Data.self) else {
                    store.error = "A selected item could not be read."
                    continue
                }
                let contentType = item.supportedContentTypes.first(where: {
                    $0.conforms(to: .image) || $0.conforms(to: .movie)
                })
                let mime = contentType?.preferredMIMEType ?? StudioPickedMedia.sniff(data)?.mime
                guard let mime, let kind = StudioPickedMedia.kind(mime: mime) else {
                    store.error = "A selected item is not a supported Studio reference."
                    continue
                }
                let preview = kind == .image ? UIImage(data: data) : nil
                store.importAttachment(
                    data: data,
                    mimeType: mime,
                    kind: kind,
                    preview: preview,
                    workspace: workspace,
                    deviceId: deviceId
                )
            }
        }
    }

    private func stageFiles(_ urls: [URL]) {
        guard let workspace, let deviceId else { return }
        Task {
            for url in urls {
                let accessed = url.startAccessingSecurityScopedResource()
                defer { if accessed { url.stopAccessingSecurityScopedResource() } }
                do {
                    let data = try Data(contentsOf: url, options: .mappedIfSafe)
                    let type = UTType(filenameExtension: url.pathExtension)
                    let mime = type?.preferredMIMEType ?? StudioPickedMedia.sniff(data)?.mime
                    guard let mime, let kind = StudioPickedMedia.kind(mime: mime) else {
                        throw StudioPickedMedia.Error.unsupported
                    }
                    store.importAttachment(
                        data: data,
                        mimeType: mime,
                        kind: kind,
                        preview: kind == .image ? UIImage(data: data) : nil,
                        workspace: workspace,
                        deviceId: deviceId
                    )
                } catch {
                    store.error = error.localizedDescription
                }
            }
        }
    }
}

private struct StudioReferenceTray: View {
    let attachments: [StudioComposerAttachment]
    let previews: [String: UIImage]
    let budgets: [JSONValue]
    let addPhoto: () -> Void
    let addFile: () -> Void
    let remove: (String) -> Void

    var body: some View {
        HStack(spacing: 8) {
            ScrollView(.horizontal) {
                LazyHStack(spacing: 8) {
                    ForEach(attachments) { attachment in
                        StudioReferenceChip(
                            attachment: attachment,
                            preview: previews[attachment.id],
                            remove: { remove(attachment.id) }
                        )
                    }
                    Menu {
                        Button(action: addPhoto) {
                            Label("Photo Library", systemImage: "photo.on.rectangle")
                        }
                        Button(action: addFile) {
                            Label("Choose Files", systemImage: "folder")
                        }
                    } label: {
                        Image(systemName: "plus")
                            .font(.system(size: 16, weight: .medium))
                            .foregroundStyle(Theme.textMuted)
                            .frame(width: 48, height: 48)
                            .background(whiteAlpha(0.04), in: RoundedRectangle(cornerRadius: 9))
                            .overlay(RoundedRectangle(cornerRadius: 9)
                                .strokeBorder(whiteAlpha(0.09), lineWidth: 1))
                    }
                }
                .padding(.top, 5)
                .padding(.horizontal, 1)
            }
            .scrollIndicators(.hidden)
            .frame(height: 55)

            let labels = StudioBudgetLabels.labels(from: budgets)
            if !labels.isEmpty {
                VStack(alignment: .trailing, spacing: 2) {
                    ForEach(labels, id: \.self) { label in
                        Text(label)
                    }
                }
                .font(Theme.sans(10.5))
                .foregroundStyle(Theme.textMuted)
                .fixedSize()
            }
        }
    }
}

private struct StudioReferenceChip: View {
    let attachment: StudioComposerAttachment
    let preview: UIImage?
    let remove: () -> Void

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 9).fill(whiteAlpha(0.05))
            if let preview {
                Image(uiImage: preview).resizable().scaledToFill()
            } else {
                Image(systemName: attachment.kind == .video ? "film" : "waveform")
                    .foregroundStyle(Theme.textMuted)
            }
            if attachment.pending {
                Color.black.opacity(0.4)
                ProgressView().controlSize(.small).tint(.white)
            }
        }
        .frame(width: 48, height: 48)
        .clipShape(RoundedRectangle(cornerRadius: 9))
        .overlay(RoundedRectangle(cornerRadius: 9)
            .strokeBorder(whiteAlpha(0.1), lineWidth: 1))
        .overlay(alignment: .topTrailing) {
            Button(action: remove) {
                Image(systemName: "xmark")
                    .font(.system(size: 8, weight: .bold))
                    .foregroundStyle(.white)
                    .frame(width: 18, height: 18)
                    .background(.black.opacity(0.7), in: Circle())
            }
            .buttonStyle(.plain)
            .offset(x: 5, y: -5)
        }
    }
}

private struct StudioDurationChip: View {
    let value: StudioControlValue
    let choices: [StudioControlChoice]
    let select: (StudioControlValue) -> Void

    var body: some View {
        Menu {
            ForEach(Array(choices.enumerated()), id: \.offset) { _, choice in
                Button {
                    select(choice.value)
                } label: {
                    if choice.value == value {
                        Label(choice.label, systemImage: "checkmark")
                    } else {
                        Text(choice.label)
                    }
                }
            }
        } label: {
            HStack(spacing: 6) {
                Text(value.label)
                Image(systemName: "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
            }
            .font(Theme.sans(13, weight: .medium))
            .foregroundStyle(Theme.text.opacity(0.9))
            .padding(.horizontal, 13)
            .frame(height: 40)
            .background(whiteAlpha(0.08), in: Capsule())
            .overlay(Capsule().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
        }
    }
}

private struct StudioModelPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let store: StudioComposerStore
    let workspace: WorkspaceStore
    let deviceId: String
    @State private var search = ""

    private var visible: [StudioComposerModel] {
        let kind: StudioMediaKind = store.snapshot.mode == .image ? .image : .video
        return store.models.filter {
            $0.outputKind == kind && (search.isEmpty
                || $0.displayName.localizedCaseInsensitiveContains(search)
                || $0.operation.label.localizedCaseInsensitiveContains(search))
        }
    }

    var body: some View {
        NavigationStack {
            List(visible) { model in
                Button {
                    store.toggleModel(model, workspace: workspace, deviceId: deviceId)
                } label: {
                    HStack(spacing: 12) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(model.displayName)
                                .font(Theme.sans(15, weight: .medium))
                                .foregroundStyle(Theme.text)
                            Text(model.operation.label)
                                .font(Theme.sans(11.5))
                                .foregroundStyle(Theme.textMuted)
                        }
                        Spacer()
                        if store.snapshot.selected.contains(where: { $0.modelId == model.id }) {
                            Image(systemName: "checkmark.circle.fill")
                                .foregroundStyle(Theme.text)
                        }
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            .searchable(text: $search, prompt: "Search models")
            .navigationTitle("Models")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
        }
        .presentationDetents([.medium, .large])
    }
}

private struct StudioModelConfigurationSheet: View {
    @Environment(\.dismiss) private var dismiss
    let model: StudioComposerModel
    let store: StudioComposerStore
    let workspace: WorkspaceStore
    let deviceId: String

    private var chip: StudioComposerChip? {
        store.evaluation?.models.first { $0.modelId == model.id }
    }

    var body: some View {
        NavigationStack {
            Form {
                if store.snapshot.mode == .image,
                   let selected = store.snapshot.selected.first(where: { $0.modelId == model.id }) {
                    Section("Amount") {
                        HStack {
                            Button { adjust(-1) } label: { Image(systemName: "minus") }
                            Spacer()
                            Text("\(selected.outputCount)").monospacedDigit()
                            Spacer()
                            Button { adjust(1) } label: { Image(systemName: "plus") }
                        }
                    }
                }
                ForEach(chip?.controls ?? model.controls.filter { $0.id != "duration" }) { control in
                    StudioControlSection(
                        control: control,
                        value: chip?.values[control.id]
                            ?? store.snapshot.selected.first(where: { $0.modelId == model.id })?.controls[control.id],
                        select: { set(control.id, $0) }
                    )
                }
            }
            .navigationTitle(model.displayName)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
        }
        .presentationDetents([.medium, .large])
    }

    private func set(_ controlId: String, _ value: StudioControlValue) {
        store.setControl(
            modelId: model.id,
            controlId: controlId,
            value: value,
            workspace: workspace,
            deviceId: deviceId
        )
    }

    private func adjust(_ delta: Int) {
        store.adjustOutputCount(
            modelId: model.id,
            delta: delta,
            workspace: workspace,
            deviceId: deviceId
        )
    }
}

private struct StudioControlSection: View {
    let control: StudioModelControl
    let value: StudioControlValue?
    let select: (StudioControlValue) -> Void

    var body: some View {
        Section(control.label) {
            if control.kind == .boolean {
                Toggle("Enabled", isOn: Binding(
                    get: { value == .boolean(true) },
                    set: { select(.boolean($0)) }
                ))
            } else if !control.choices.isEmpty {
                Picker(control.label, selection: Binding(
                    get: { value ?? control.default ?? control.choices[0].value },
                    set: select
                )) {
                    ForEach(Array(control.choices.enumerated()), id: \.offset) { _, choice in
                        Text(choice.label).tag(choice.value)
                    }
                }
                .pickerStyle(.inline)
                .labelsHidden()
            } else {
                Text(value?.label ?? "Default")
                    .foregroundStyle(Theme.textMuted)
            }
            if let description = control.description, !description.isEmpty {
                Text(description)
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textMuted)
            }
        }
    }
}

private struct StudioConflictSheet: View {
    @Environment(\.dismiss) private var dismiss
    let conflict: StudioComposerConflict
    let resolve: (StudioConflictAction) -> Void

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 18) {
                Label(conflict.title, systemImage: "exclamationmark.triangle")
                    .font(Theme.sans(17, weight: .semibold))
                Text(conflict.explanation)
                    .font(Theme.sans(14))
                    .foregroundStyle(Theme.textMuted)
                VStack(spacing: 10) {
                    ForEach(Array(conflict.actions.enumerated()), id: \.offset) { _, action in
                        Button(action.label) { resolve(action) }
                            .buttonStyle(.borderedProminent)
                            .frame(maxWidth: .infinity)
                    }
                }
                Spacer()
            }
            .padding(20)
            .navigationTitle("Resolve")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
        .presentationDetents([.medium])
    }
}

private enum StudioBudgetLabels {
    static func labels(from budgets: [JSONValue]) -> [String] {
        budgets.compactMap { value in
            guard case .object(let object) = value,
                  let used = object["used"]?.int64Value,
                  let maximum = object["maximum"]?.int64Value,
                  case .object(let kind)? = object["kind"],
                  let roleValue = kind["role"] else { return nil }
            let role: String?
            if case .string(let direct) = roleValue {
                role = direct
            } else if case .object(let nested) = roleValue {
                role = nested["role"]?.stringValue
            } else {
                role = nil
            }
            let noun: String
            switch role {
            case "source", "last_frame": noun = "frames"
            case "reference_video": noun = "videos"
            case "reference_audio", "audio": noun = "audio"
            default: noun = "images"
            }
            return "\(used)/\(maximum) \(noun)"
        }
    }
}

private enum StudioPickedMedia {
    struct Sniffed { let mime: String }
    enum Error: LocalizedError {
        case unsupported
        var errorDescription: String? { "This file is not a supported Studio reference." }
    }

    static func kind(mime: String) -> StudioComposerMediaKind? {
        if mime.hasPrefix("image/") { return .image }
        if mime.hasPrefix("video/") { return .video }
        if mime.hasPrefix("audio/") { return .audio }
        return nil
    }

    static func sniff(_ data: Data) -> Sniffed? {
        let bytes = [UInt8](data.prefix(16))
        if bytes.starts(with: [0xFF, 0xD8, 0xFF]) { return Sniffed(mime: "image/jpeg") }
        if bytes.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return Sniffed(mime: "image/png") }
        if bytes.count >= 12,
           String(bytes: bytes[0..<4], encoding: .ascii) == "RIFF",
           String(bytes: bytes[8..<12], encoding: .ascii) == "WEBP" {
            return Sniffed(mime: "image/webp")
        }
        if bytes.count >= 12, String(bytes: bytes[4..<8], encoding: .ascii) == "ftyp" {
            return Sniffed(mime: "video/mp4")
        }
        if bytes.starts(with: [0x52, 0x49, 0x46, 0x46]) { return Sniffed(mime: "audio/wav") }
        if bytes.starts(with: [0x49, 0x44, 0x33]) { return Sniffed(mime: "audio/mpeg") }
        return nil
    }
}
