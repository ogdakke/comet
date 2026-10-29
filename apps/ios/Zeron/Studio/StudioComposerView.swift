import PhotosUI
import UIKit
import UniformTypeIdentifiers

/// Studio's composer, built like the chat's `ComposerBar`: one glass surface
/// that rests as a capsule (prompt + send) and opens into a card while
/// composing — the reference tray, the full-width prompt, and a toolbar row
/// of chips: [image|video] [duration] [model · 2× · 16:9]… [Models] … [↑].
///
/// What can be sent, which chips show and what conflicts exist all come
/// from the host's evaluation of the draft (`StudioComposerStore`); this view
/// paints it in `updateProperties()`, which UIKit re-runs when it changes.
final class StudioComposerView: UIView, UITextViewDelegate, UIGestureRecognizerDelegate,
    PHPickerViewControllerDelegate, UIDocumentPickerDelegate, UIAdaptivePresentationControllerDelegate {
    var onHeightChange: (() -> Void)?

    private let store: StudioComposerStore
    private let browser: StudioBrowserStore
    private let service: StudioService

    private let errorLabel = UILabel()
    private let conflictButton = UIButton(type: .system)
    private let glass = Glass.surface(interactive: false, radius: 25)
    let textView = UITextView()
    private let placeholderLabel = UILabel()
    private let trayClip = UIView()
    private let tray = StudioReferenceTray()
    private let toolbar = UIView()
    private let chipScroll = FadingScrollView()
    private let chipStrip = UIStackView()
    private let modeSegment = StudioModeSegment()
    private let durationChip = StudioComposerView.chip("")
    private let modelsChip = StudioComposerView.chip("Models")
    private var modelChips: [UIButton] = []
    private var modelChipLabels: [String] = []
    private let actionButton = UIButton(type: .system)
    private var textHeight: NSLayoutConstraint!
    private var trayHeight: NSLayoutConstraint!
    private var toolbarHeight: NSLayoutConstraint!
    private var compactConstraints: [NSLayoutConstraint] = []
    private var cardConstraints: [NSLayoutConstraint] = []
    private(set) var isCard = false
    private var trayAvailable = false
    /// Sheets and pickers up from here. Presenting one ends editing; the card
    /// holds while they're up instead of collapsing under the sheet.
    private var openSheets: Set<ObjectIdentifier> = []
    private weak var modelPicker: UIViewController?

    private let font = Fonts.ui(.sans, UIFontMetrics(forTextStyle: .body).scaledValue(for: 16.5))
    private var maxLines: Int {
        if !isCard { return 2 }
        return traitCollection.verticalSizeClass == .compact ? 3 : 7
    }
    private static let control: CGFloat = 34
    private static let compactHeight: CGFloat = 50

    private var deviceId: String? { browser.selectedDeviceId }

    init(store: StudioComposerStore, browser: StudioBrowserStore, service: StudioService) {
        self.store = store
        self.browser = browser
        self.service = service
        super.init(frame: .zero)
        build()
    }

    required init?(coder: NSCoder) { fatalError() }

    private func build() {
        errorLabel.font = Fonts.ui(.sans, 11.5)
        errorLabel.textColor = Palette.danger
        errorLabel.numberOfLines = 2

        var conflict = UIButton.Configuration.plain()
        conflict.image = UIImage(systemName: "exclamationmark.circle", withConfiguration: UIImage.SymbolConfiguration(pointSize: 11.5, weight: .medium))
        conflict.imagePadding = 6
        conflict.baseForegroundColor = Palette.warning
        conflict.contentInsets = NSDirectionalEdgeInsets(top: 2, leading: 0, bottom: 2, trailing: 18)
        conflict.titleLineBreakMode = .byTruncatingTail
        conflict.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attrs in
            var a = attrs
            a.font = Fonts.ui(.sansMedium, 11.5)
            return a
        }
        conflictButton.configuration = conflict
        conflictButton.contentHorizontalAlignment = .leading
        let chevron = UIImageView(image: UIImage(systemName: "chevron.up", withConfiguration: UIImage.SymbolConfiguration(pointSize: 9, weight: .semibold)))
        chevron.tintColor = Palette.warning
        chevron.translatesAutoresizingMaskIntoConstraints = false
        conflictButton.addSubview(chevron)
        NSLayoutConstraint.activate([
            chevron.trailingAnchor.constraint(equalTo: conflictButton.trailingAnchor),
            chevron.centerYAnchor.constraint(equalTo: conflictButton.centerYAnchor),
        ])
        conflictButton.addAction(UIAction { [weak self] _ in self?.showConflict() }, for: .touchUpInside)

        // The notice lines sit 24pt in from the screen edge, past the bar's 12.
        let notices = UIStackView(arrangedSubviews: [errorLabel, conflictButton])
        notices.axis = .vertical
        notices.isLayoutMarginsRelativeArrangement = true
        notices.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12)
        let stack = UIStackView(arrangedSubviews: [notices, glass])
        stack.axis = .vertical
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)

        let content = glass.contentView
        let tap = UITapGestureRecognizer(target: self, action: #selector(focusFromTap))
        tap.cancelsTouchesInView = false
        tap.delegate = self
        glass.addGestureRecognizer(tap)

        trayClip.clipsToBounds = true
        trayClip.translatesAutoresizingMaskIntoConstraints = false
        tray.translatesAutoresizingMaskIntoConstraints = false
        tray.addMenu = UIMenu(children: [
            UIAction(title: "Photo Library", image: UIImage(systemName: "photo.on.rectangle")) { [weak self] _ in self?.presentPhotos() },
            UIAction(title: "Choose Files", image: UIImage(systemName: "folder")) { [weak self] _ in self?.presentFiles() },
        ])
        tray.onRemove = { [weak self] id in
            guard let self, let deviceId = self.deviceId else { return }
            self.store.removeAttachment(id, workspace: self.service, deviceId: deviceId)
        }
        trayClip.addSubview(tray)
        content.addSubview(trayClip)

        textView.font = font
        textView.textColor = Palette.text
        textView.tintColor = Palette.text
        textView.backgroundColor = .clear
        textView.delegate = self
        textView.isScrollEnabled = false
        textView.textContainerInset = UIEdgeInsets(top: 14, left: 0, bottom: 14, right: 0)
        textView.textContainer.lineFragmentPadding = 0
        textView.keyboardDismissMode = .interactive
        textView.accessibilityIdentifier = "studio-composer-input"
        textView.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(textView)

        placeholderLabel.font = font
        placeholderLabel.textColor = Palette.tertiary
        placeholderLabel.isUserInteractionEnabled = false
        placeholderLabel.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(placeholderLabel)

        toolbar.clipsToBounds = true
        toolbar.translatesAutoresizingMaskIntoConstraints = false
        content.insertSubview(toolbar, at: 0)
        chipScroll.showsHorizontalScrollIndicator = false
        chipScroll.translatesAutoresizingMaskIntoConstraints = false
        chipStrip.axis = .horizontal
        chipStrip.spacing = 8
        chipStrip.alignment = .center
        chipStrip.translatesAutoresizingMaskIntoConstraints = false
        chipScroll.addSubview(chipStrip)
        toolbar.addSubview(chipScroll)

        modeSegment.onChange = { [weak self] mode in
            guard let self, let deviceId = self.deviceId else { return }
            self.store.setMode(mode, workspace: self.service, deviceId: deviceId)
        }
        durationChip.showsMenuAsPrimaryAction = true
        durationChip.accessibilityLabel = "Duration"
        modelsChip.addAction(UIAction { [weak self] _ in self?.showModelPicker() }, for: .touchUpInside)
        chipStrip.addArrangedSubview(modeSegment)
        chipStrip.addArrangedSubview(durationChip)
        chipStrip.addArrangedSubview(modelsChip)

        actionButton.translatesAutoresizingMaskIntoConstraints = false
        actionButton.accessibilityLabel = "Create"
        actionButton.accessibilityIdentifier = "studio-composer-send"
        actionButton.addAction(UIAction { [weak self] _ in self?.send() }, for: .touchUpInside)
        content.addSubview(actionButton)

        let c = Self.control
        textHeight = textView.heightAnchor.constraint(equalToConstant: Self.compactHeight)
        trayHeight = trayClip.heightAnchor.constraint(equalToConstant: 0)
        toolbarHeight = toolbar.heightAnchor.constraint(equalToConstant: 0)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),

            trayClip.topAnchor.constraint(equalTo: content.topAnchor),
            trayClip.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 12),
            trayClip.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -12),
            trayHeight,
            tray.leadingAnchor.constraint(equalTo: trayClip.leadingAnchor),
            tray.trailingAnchor.constraint(equalTo: trayClip.trailingAnchor),
            tray.bottomAnchor.constraint(equalTo: trayClip.bottomAnchor),
            tray.heightAnchor.constraint(equalToConstant: StudioReferenceTray.height),

            textView.topAnchor.constraint(equalTo: trayClip.bottomAnchor),
            textHeight,
            toolbar.topAnchor.constraint(equalTo: textView.bottomAnchor),
            toolbar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            toolbar.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            toolbar.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            toolbarHeight,

            placeholderLabel.leadingAnchor.constraint(equalTo: textView.leadingAnchor),
            placeholderLabel.trailingAnchor.constraint(lessThanOrEqualTo: textView.trailingAnchor),
            placeholderLabel.topAnchor.constraint(equalTo: textView.topAnchor, constant: 14),

            actionButton.widthAnchor.constraint(equalToConstant: c),
            actionButton.heightAnchor.constraint(equalToConstant: c),

            chipScroll.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 10),
            chipScroll.trailingAnchor.constraint(equalTo: actionButton.leadingAnchor, constant: -8),
            chipScroll.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),
            chipScroll.heightAnchor.constraint(equalToConstant: c),
            chipStrip.topAnchor.constraint(equalTo: chipScroll.contentLayoutGuide.topAnchor),
            chipStrip.bottomAnchor.constraint(equalTo: chipScroll.contentLayoutGuide.bottomAnchor),
            chipStrip.leadingAnchor.constraint(equalTo: chipScroll.contentLayoutGuide.leadingAnchor),
            chipStrip.trailingAnchor.constraint(equalTo: chipScroll.contentLayoutGuide.trailingAnchor),
            chipStrip.heightAnchor.constraint(equalTo: chipScroll.frameLayoutGuide.heightAnchor),
        ])
        // Resting capsule: the prompt, then send at the capsule's end.
        compactConstraints = [
            actionButton.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -8),
            actionButton.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -8),
            textView.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 18),
            textView.trailingAnchor.constraint(equalTo: actionButton.leadingAnchor, constant: -8),
        ]
        // Card: full-width prompt, send pinned at the end of the chip row.
        cardConstraints = [
            actionButton.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -10),
            actionButton.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),
            textView.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            textView.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -16),
        ]
        NSLayoutConstraint.activate(compactConstraints)
        chipScroll.alpha = 0
        tray.alpha = 0
    }

    // MARK: Rendering

    override func updateProperties() {
        super.updateProperties()
        let snapshot = store.snapshot
        let evaluation = store.evaluation

        if textView.text != snapshot.prompt, textView.markedTextRange == nil {
            textView.text = snapshot.prompt
            textChanged()
        }
        placeholderLabel.text = snapshot.mode == .image
            ? "Describe the image you want to create"
            : "Describe the video you want to create"

        if let error = store.error {
            errorLabel.text = error
            errorLabel.isHidden = false
            conflictButton.isHidden = true
        } else if let conflict = evaluation?.conflicts.first {
            errorLabel.isHidden = true
            conflictButton.configuration?.title = conflict.title
            conflictButton.isHidden = false
        } else {
            errorLabel.isHidden = true
            conflictButton.isHidden = true
        }
        // No notice, no gap above the glass.
        errorLabel.superview?.isHidden = errorLabel.isHidden && conflictButton.isHidden

        modeSegment.mode = snapshot.mode
        if snapshot.mode == .video, let duration = evaluation?.globals.duration {
            durationChip.configuration?.title = duration.label
            let choices = evaluation?.globals.durationChoices ?? []
            durationChip.menu = UIMenu(children: choices.map { choice in
                UIAction(title: choice.label, state: choice.value == duration ? .on : .off) { [weak self] _ in
                    guard let self, let deviceId = self.deviceId else { return }
                    self.store.setDuration(choice.value, workspace: self.service, deviceId: deviceId)
                }
            })
            durationChip.isHidden = false
        } else {
            durationChip.isHidden = true
        }
        let chips = evaluation?.models ?? []
        let labels = chips.map { modelChipLabel($0, mode: snapshot.mode) }
        if labels != modelChipLabels {
            modelChipLabels = labels
            modelChips.forEach { $0.removeFromSuperview() }
            modelChips = zip(chips, labels).map { chip, label in
                let button = Self.chip(label)
                let modelId = chip.modelId
                button.addAction(UIAction { [weak self] _ in self?.showConfiguration(modelId: modelId) }, for: .touchUpInside)
                return button
            }
            for (offset, button) in modelChips.enumerated() {
                chipStrip.insertArrangedSubview(button, at: 2 + offset)
            }
            // Place the new chips now: left to the card's animated layout
            // pass they stayed at the strip's origin, under the switch.
            UIView.performWithoutAnimation { chipStrip.layoutIfNeeded() }
        }

        if let evaluation, evaluation.attachments.addEnabled || !evaluation.attachments.items.isEmpty {
            trayAvailable = true
            tray.update(
                attachments: evaluation.attachments.items,
                previews: store.previews,
                budgets: StudioBudgetLabels.labels(from: evaluation.budgets)
            )
        } else {
            trayAvailable = false
        }

        var action = UIButton.Configuration.filled()
        action.cornerStyle = .capsule
        action.contentInsets = .zero
        let enabled = store.canSend
        action.baseBackgroundColor = enabled ? Palette.accent : Palette.controlFill
        action.baseForegroundColor = enabled ? .white : Palette.tertiary
        if store.sending {
            action.showsActivityIndicator = true
        } else {
            action.image = UIImage(systemName: "arrow.up", withConfiguration: UIImage.SymbolConfiguration(pointSize: 15, weight: .bold))
        }
        actionButton.configuration = action
        actionButton.isEnabled = enabled

        // An evaluation (or a conflict action) can ask for the model picker.
        if store.modelPickerRequested {
            store.modelPickerRequested = false
            DispatchQueue.main.async { [weak self] in self?.showModelPicker() }
        }
        updateMode(animated: window != nil)
        updateTray(animated: window != nil)
    }

    private func modelChipLabel(_ chip: StudioComposerChip, mode: StudioComposerMode) -> String {
        var readouts: [String] = []
        if mode == .image { readouts.append("\(chip.outputCount)×") }
        for id in ["aspect_ratio", "resolution"] {
            if let value = chip.values[id] { readouts.append(value.label) }
        }
        return ([chip.displayName] + readouts).joined(separator: " · ")
    }

    /// A toolbar chip: label + chevron in a capsule.
    private static func chip(_ title: String) -> UIButton {
        var config = UIButton.Configuration.filled()
        config.title = title
        config.image = UIImage(systemName: "chevron.down", withConfiguration: UIImage.SymbolConfiguration(pointSize: 9, weight: .semibold))
        config.imagePlacement = .trailing
        config.imagePadding = 6
        config.imageColorTransformer = UIConfigurationColorTransformer { _ in Palette.color(.textFaint) }
        config.baseForegroundColor = Palette.text
        config.baseBackgroundColor = Palette.controlFill
        config.cornerStyle = .capsule
        config.contentInsets = NSDirectionalEdgeInsets(top: 7, leading: 13, bottom: 7, trailing: 13)
        config.titleLineBreakMode = .byTruncatingTail
        config.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attrs in
            var a = attrs
            a.font = Fonts.ui(.sansMedium, 13)
            return a
        }
        let button = UIButton(configuration: config)
        button.setContentHuggingPriority(.required, for: .horizontal)
        button.setContentCompressionResistancePriority(.required, for: .horizontal)
        return button
    }

    // MARK: Resting ↔ card

    /// Composing (focused), holding references, or a sheet up from here.
    private var wantsCard: Bool {
        textView.isFirstResponder || !openSheets.isEmpty || !store.snapshot.attachments.isEmpty
    }

    private func updateMode(animated: Bool) {
        let card = wantsCard
        guard card != isCard else { return }
        isCard = card
        let change = {
            if card {
                NSLayoutConstraint.deactivate(self.compactConstraints)
                NSLayoutConstraint.activate(self.cardConstraints)
            } else {
                NSLayoutConstraint.deactivate(self.cardConstraints)
                NSLayoutConstraint.activate(self.compactConstraints)
            }
            self.toolbarHeight.constant = card ? 50 : 0
            self.chipScroll.alpha = card ? 1 : 0
            self.textView.textContainerInset = card ? UIEdgeInsets(top: 14, left: 0, bottom: 6, right: 0) : UIEdgeInsets(top: 14, left: 0, bottom: 14, right: 0)
            self.glass.cornerConfiguration = .uniformCorners(radius: .fixed(card ? 26 : 25))
            self.applyTray()
            self.recomputeTextHeight()
            self.onHeightChange?()
            self.superview?.layoutIfNeeded()
        }
        guard animated, window != nil, !UIAccessibility.isReduceMotionEnabled else { return change() }
        UIView.animate(withDuration: 0.42, delay: 0, usingSpringWithDamping: 0.86, initialSpringVelocity: 0, options: [.allowUserInteraction, .beginFromCurrentState], animations: change)
    }

    /// The reference tray shows inside the card only.
    private var trayShown: Bool { isCard && trayAvailable }

    private func applyTray() {
        let shown = trayShown
        trayHeight.constant = shown ? StudioReferenceTray.height + 12 : 0
        tray.alpha = shown ? 1 : 0
    }

    private func updateTray(animated: Bool) {
        let target: CGFloat = trayShown ? StudioReferenceTray.height + 12 : 0
        guard abs(trayHeight.constant - target) > 0.5 else { return }
        let change = {
            self.applyTray()
            self.onHeightChange?()
            self.superview?.layoutIfNeeded()
        }
        guard animated, !UIAccessibility.isReduceMotionEnabled else { return change() }
        UIView.animate(withDuration: 0.3, delay: 0, options: [.beginFromCurrentState, .allowUserInteraction], animations: change)
    }

    // MARK: Focus

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        var v = touch.view
        while let view = v, view !== glass {
            if view is UIControl { return false }
            v = view.superview
        }
        return true
    }

    @objc private func focusFromTap() {
        if !textView.isFirstResponder { textView.becomeFirstResponder() }
    }

    @discardableResult
    override func becomeFirstResponder() -> Bool { textView.becomeFirstResponder() }

    @discardableResult
    override func resignFirstResponder() -> Bool { textView.resignFirstResponder() }

    func textViewDidBeginEditing(_ textView: UITextView) {
        updateMode(animated: true)
    }

    func textViewDidEndEditing(_ textView: UITextView) {
        updateMode(animated: true)
        textView.setContentOffset(.zero, animated: false)
    }

    // MARK: Text

    func textViewDidChange(_ textView: UITextView) {
        store.prompt = textView.text
        textChanged()
    }

    private func recomputeTextHeight() {
        // At rest a draft is a preview: whole lines ending in "…".
        let preview = !isCard
        let lines = preview ? maxLines : 0
        let breakMode: NSLineBreakMode = preview ? .byTruncatingTail : .byWordWrapping
        if textView.textContainer.maximumNumberOfLines != lines || textView.textContainer.lineBreakMode != breakMode {
            textView.textContainer.maximumNumberOfLines = lines
            textView.textContainer.lineBreakMode = breakMode
        }
        let width = max(40, textView.bounds.width)
        let fitting = textView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude)).height
        let insets = textView.textContainerInset.top + textView.textContainerInset.bottom
        let maxHeight = font.lineHeight * CGFloat(maxLines) + insets
        let minHeight: CGFloat = isCard ? 46 : Self.compactHeight
        textView.isScrollEnabled = !preview && fitting > maxHeight
        if preview { textView.contentOffset = .zero }
        textHeight.constant = preview ? max(minHeight, fitting) : min(max(minHeight, fitting), maxHeight)
    }

    private func textChanged() {
        placeholderLabel.isHidden = !textView.text.isEmpty
        let before = textHeight.constant
        recomputeTextHeight()
        if abs(textHeight.constant - before) > 0.5 {
            UIView.animate(withDuration: 0.18, delay: 0, options: [.beginFromCurrentState, .allowUserInteraction]) {
                self.onHeightChange?()
                self.superview?.layoutIfNeeded()
            }
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if textView.bounds.width > 0, !textView.text.isEmpty, abs(textView.contentSize.height - textHeight.constant) > 1 {
            recomputeTextHeight()
        }
    }

    private func send() {
        guard let deviceId, store.canSend else { return }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        store.send(workspace: service, deviceId: deviceId)
    }

    // External keyboards: ⌘↩ sends.
    override var keyCommands: [UIKeyCommand]? {
        [UIKeyCommand(title: "Create", action: #selector(commandReturn), input: "\r", modifierFlags: .command)]
    }

    @objc private func commandReturn() { send() }

    // MARK: Sheets

    /// Presents over whatever is already up, holding the card meanwhile.
    private func presentSheet(_ controller: UIViewController) {
        guard var host = findViewController() else { return }
        while let presented = host.presentedViewController { host = presented }
        openSheets.insert(ObjectIdentifier(controller))
        controller.presentationController?.delegate = self
        if let sheet = controller as? StudioSheetController {
            sheet.onDismiss = { [weak self, weak controller] in
                guard let controller else { return }
                self?.sheetClosed(controller)
            }
        }
        host.present(controller, animated: true)
        updateMode(animated: true)
    }

    private func sheetClosed(_ controller: UIViewController) {
        guard openSheets.remove(ObjectIdentifier(controller)) != nil else { return }
        updateMode(animated: true)
    }

    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
        sheetClosed(presentationController.presentedViewController)
    }

    private func showModelPicker() {
        guard modelPicker == nil, let deviceId else { return }
        let picker = StudioModelPickerController(store: store, service: service, deviceId: deviceId)
        let sheet = StudioSheetController(rootViewController: picker)
        modelPicker = sheet
        presentSheet(sheet)
    }

    private func showConfiguration(modelId: String) {
        guard let deviceId, let model = store.models.first(where: { $0.id == modelId }) else { return }
        let configuration = StudioModelConfigurationController(model: model, store: store, service: service, deviceId: deviceId)
        presentSheet(StudioSheetController(rootViewController: configuration))
    }

    private func showConflict() {
        guard let conflict = store.evaluation?.conflicts.first else { return }
        let controller = StudioConflictController(conflict: conflict) { [weak self] action in
            guard let self, let deviceId = self.deviceId else { return }
            self.store.applyConflictAction(action, workspace: self.service, deviceId: deviceId)
        }
        presentSheet(StudioSheetController(rootViewController: controller, detents: [.medium()]))
    }

    // MARK: References

    private func presentPhotos() {
        var config = PHPickerConfiguration(photoLibrary: .shared())
        config.filter = .any(of: [.images, .videos])
        config.selectionLimit = 12
        config.preferredAssetRepresentationMode = .current
        let picker = PHPickerViewController(configuration: config)
        picker.delegate = self
        presentSheet(picker)
    }

    private func presentFiles() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.image, .movie, .audio], asCopy: true)
        picker.allowsMultipleSelection = true
        picker.delegate = self
        presentSheet(picker)
    }

    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        sheetClosed(picker)
        guard !results.isEmpty else { return }
        Task { [weak self] in
            // One at a time, in the order picked.
            for result in results {
                let loaded = await Self.loadMedia(result.itemProvider)
                guard let self, let deviceId = self.deviceId else { return }
                guard let (data, type) = loaded else {
                    self.store.error = "A selected item could not be read."
                    continue
                }
                let mime = type?.preferredMIMEType ?? StudioPickedMedia.sniff(data)?.mime
                guard let mime, let kind = StudioPickedMedia.kind(mime: mime) else {
                    self.store.error = "A selected item is not a supported Studio reference."
                    continue
                }
                self.store.importAttachment(
                    data: data,
                    mimeType: mime,
                    kind: kind,
                    preview: kind == .image ? UIImage(data: data) : nil,
                    workspace: self.service,
                    deviceId: deviceId
                )
            }
        }
    }

    /// The item's bytes in its first image or movie representation.
    private static func loadMedia(_ provider: NSItemProvider) async -> (Data, UTType?)? {
        let type = provider.registeredTypeIdentifiers
            .compactMap { UTType($0) }
            .first { $0.conforms(to: .image) || $0.conforms(to: .movie) }
        guard let type else { return nil }
        return await withCheckedContinuation { continuation in
            provider.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, _ in
                continuation.resume(returning: data.map { ($0, type) })
            }
        }
    }

    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        sheetClosed(controller)
        guard let deviceId else { return }
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
                    workspace: service,
                    deviceId: deviceId
                )
            } catch {
                store.error = error.localizedDescription
            }
        }
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        sheetClosed(controller)
    }
}

// MARK: - Toolbar pieces

/// Image | video, icon-only.
private final class StudioModeSegment: UISegmentedControl {
    var onChange: ((StudioComposerMode) -> Void)?

    var mode: StudioComposerMode {
        get { selectedSegmentIndex == 1 ? .video : .image }
        set { selectedSegmentIndex = newValue == .video ? 1 : 0 }
    }

    init() {
        let image = UIImage(systemName: "photo")
        image?.accessibilityLabel = "Image"
        let video = UIImage(systemName: "video")
        video?.accessibilityLabel = "Video"
        super.init(items: [image as Any, video as Any])
        selectedSegmentIndex = 0
        accessibilityLabel = "Media type"
        translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            widthAnchor.constraint(equalToConstant: 84),
            heightAnchor.constraint(equalToConstant: 34),
        ])
        addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.onChange?(self.mode)
        }, for: .valueChanged)
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// The card's reference strip: staged references, a "+" menu (Photos,
/// Files), and the host's budget readouts ("1/3 images") at the end.
private final class StudioReferenceTray: UIView {
    static let height: CGFloat = 55

    var addMenu: UIMenu? {
        get { addButton.menu }
        set { addButton.menu = newValue }
    }
    var onRemove: ((String) -> Void)?

    private let scroll = UIScrollView()
    private let strip = UIStackView()
    private let addButton = UIButton(type: .system)
    private let budgets = UIStackView()
    private var signature: [String] = []

    override init(frame: CGRect) {
        super.init(frame: frame)
        scroll.showsHorizontalScrollIndicator = false
        scroll.clipsToBounds = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        strip.axis = .horizontal
        strip.spacing = 8
        strip.alignment = .center
        strip.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(strip)
        addSubview(scroll)

        var add = UIButton.Configuration.filled()
        add.image = UIImage(systemName: "plus", withConfiguration: UIImage.SymbolConfiguration(pointSize: 16, weight: .medium))
        add.baseForegroundColor = Palette.secondary
        add.baseBackgroundColor = Palette.controlFill
        add.background.cornerRadius = 9
        add.background.strokeColor = Palette.hairline
        add.background.strokeWidth = 1
        add.contentInsets = .zero
        addButton.configuration = add
        addButton.showsMenuAsPrimaryAction = true
        addButton.accessibilityLabel = "Add reference"
        addButton.translatesAutoresizingMaskIntoConstraints = false
        addButton.widthAnchor.constraint(equalToConstant: 48).isActive = true
        addButton.heightAnchor.constraint(equalToConstant: 48).isActive = true
        strip.addArrangedSubview(addButton)

        budgets.axis = .vertical
        budgets.alignment = .trailing
        budgets.spacing = 2
        budgets.setContentHuggingPriority(.required, for: .horizontal)
        budgets.setContentCompressionResistancePriority(.required, for: .horizontal)
        budgets.translatesAutoresizingMaskIntoConstraints = false
        addSubview(budgets)

        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: budgets.leadingAnchor, constant: -8),
            budgets.trailingAnchor.constraint(equalTo: trailingAnchor),
            budgets.centerYAnchor.constraint(equalTo: centerYAnchor),
            // 5pt above the chips leaves room for their remove badges.
            strip.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 5),
            strip.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -2),
            strip.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 1),
            strip.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            strip.heightAnchor.constraint(equalToConstant: 48),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func update(attachments: [StudioComposerAttachment], previews: [String: UIImage], budgets labels: [String]) {
        let next = attachments.map { "\($0.id):\($0.pending):\(previews[$0.id] != nil)" }
        if next != signature {
            signature = next
            strip.arrangedSubviews.filter { $0 !== addButton }.forEach { $0.removeFromSuperview() }
            for (index, attachment) in attachments.enumerated() {
                let chip = StudioReferenceChip(attachment: attachment, preview: previews[attachment.id])
                let id = attachment.id
                chip.onRemove = { [weak self] in self?.onRemove?(id) }
                strip.insertArrangedSubview(chip, at: index)
            }
        }
        let shown = budgets.arrangedSubviews.compactMap { ($0 as? UILabel)?.text }
        guard shown != labels else { return }
        budgets.arrangedSubviews.forEach { $0.removeFromSuperview() }
        for text in labels {
            let label = UILabel()
            label.text = text
            label.font = Fonts.ui(.sans, 10.5)
            label.textColor = Palette.secondary
            budgets.addArrangedSubview(label)
        }
    }
}

/// One staged reference: its thumbnail (or a media glyph), dimmed with a
/// spinner while it uploads, and a remove badge on its corner.
private final class StudioReferenceChip: UIView {
    var onRemove: (() -> Void)?
    private let remove = UIButton(type: .system)

    init(attachment: StudioComposerAttachment, preview: UIImage?) {
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        let tile = UIView()
        tile.backgroundColor = Palette.controlFill
        tile.layer.cornerRadius = 9
        tile.layer.cornerCurve = .continuous
        tile.layer.borderWidth = 1
        tile.layer.borderColor = Palette.hairline.cgColor
        tile.clipsToBounds = true
        tile.frame = CGRect(x: 0, y: 0, width: 48, height: 48)
        addSubview(tile)
        if let preview {
            let image = UIImageView(image: preview)
            image.contentMode = .scaleAspectFill
            image.frame = tile.bounds
            tile.addSubview(image)
        } else {
            let glyph = UIImageView(image: UIImage(systemName: attachment.kind == .video ? "film" : "waveform"))
            glyph.tintColor = Palette.secondary
            glyph.contentMode = .center
            glyph.frame = tile.bounds
            tile.addSubview(glyph)
        }
        if attachment.pending {
            let dim = UIView(frame: tile.bounds)
            dim.backgroundColor = UIColor.black.withAlphaComponent(0.4)
            tile.addSubview(dim)
            let spinner = UIActivityIndicatorView(style: .medium)
            spinner.color = .white
            spinner.transform = CGAffineTransform(scaleX: 0.75, y: 0.75)
            spinner.center = CGPoint(x: 24, y: 24)
            spinner.startAnimating()
            tile.addSubview(spinner)
        }
        var x = UIButton.Configuration.filled()
        x.image = UIImage(systemName: "xmark", withConfiguration: UIImage.SymbolConfiguration(pointSize: 8, weight: .bold))
        x.baseBackgroundColor = UIColor.black.withAlphaComponent(0.7)
        x.baseForegroundColor = .white
        x.cornerStyle = .capsule
        x.contentInsets = .zero
        remove.configuration = x
        remove.accessibilityLabel = "Remove reference"
        remove.frame = CGRect(x: 48 - 18 + 5, y: -5, width: 18, height: 18)
        remove.addAction(UIAction { [weak self] _ in self?.onRemove?() }, for: .touchUpInside)
        addSubview(remove)
        NSLayoutConstraint.activate([
            widthAnchor.constraint(equalToConstant: 48),
            heightAnchor.constraint(equalToConstant: 48),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// The remove badge hangs past the corner; it still takes taps there.
    override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
        bounds.insetBy(dx: -6, dy: -6).contains(point)
    }
}

// MARK: - Sheets

/// A sheet's navigation controller: medium/large detents, and a callback
/// when it goes away however it's dismissed.
private final class StudioSheetController: UINavigationController {
    var onDismiss: (() -> Void)?

    init(rootViewController: UIViewController, detents: [UISheetPresentationController.Detent] = [.medium(), .large()]) {
        super.init(rootViewController: rootViewController)
        sheetPresentationController?.detents = detents
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isBeingDismissed { onDismiss?() }
    }
}

/// Models for the current mode, searchable; tapping one adds or removes it.
private final class StudioModelPickerController: UITableViewController, UISearchResultsUpdating {
    private let store: StudioComposerStore
    private let service: StudioService
    private let deviceId: String
    private var visible: [StudioComposerModel] = []
    private var selected: Set<String> = []
    private var search = "" { didSet { setNeedsUpdateProperties() } }

    init(store: StudioComposerStore, service: StudioService, deviceId: String) {
        self.store = store
        self.service = service
        self.deviceId = deviceId
        super.init(style: .insetGrouped)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Models"
        navigationItem.largeTitleDisplayMode = .never
        navigationItem.rightBarButtonItem = UIBarButtonItem(systemItem: .done, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        let searchController = UISearchController(searchResultsController: nil)
        searchController.searchResultsUpdater = self
        searchController.obscuresBackgroundDuringPresentation = false
        searchController.searchBar.placeholder = "Search models"
        navigationItem.searchController = searchController
        navigationItem.hidesSearchBarWhenScrolling = false
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: "model")
    }

    func updateSearchResults(for searchController: UISearchController) {
        search = searchController.searchBar.text ?? ""
    }

    override func updateProperties() {
        super.updateProperties()
        let kind: StudioMediaKind = store.snapshot.mode == .image ? .image : .video
        visible = store.models.filter {
            $0.outputKind == kind && (search.isEmpty
                || $0.displayName.localizedCaseInsensitiveContains(search)
                || $0.operation.label.localizedCaseInsensitiveContains(search))
        }
        selected = Set(store.snapshot.selected.map(\.modelId))
        tableView.reloadData()
    }

    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        visible.count
    }

    override func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let cell = tableView.dequeueReusableCell(withIdentifier: "model", for: indexPath)
        let model = visible[indexPath.row]
        var content = UIListContentConfiguration.subtitleCell()
        content.text = model.displayName
        content.textProperties.font = Fonts.ui(.sansMedium, 15)
        content.textProperties.color = Palette.text
        content.secondaryText = model.operation.label
        content.secondaryTextProperties.font = Fonts.ui(.sans, 11.5)
        content.secondaryTextProperties.color = Palette.secondary
        content.textToSecondaryTextVerticalPadding = 3
        cell.contentConfiguration = content
        if selected.contains(model.id) {
            let check = UIImageView(image: UIImage(systemName: "checkmark.circle.fill"))
            check.tintColor = Palette.text
            cell.accessoryView = check
            cell.accessibilityTraits.insert(.selected)
        } else {
            cell.accessoryView = nil
            cell.accessibilityTraits.remove(.selected)
        }
        return cell
    }

    override func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
        tableView.deselectRow(at: indexPath, animated: true)
        store.toggleModel(visible[indexPath.row], workspace: service, deviceId: deviceId)
    }
}

/// One selected model's settings: how many to make (images), then each of
/// its controls as a section — a switch, an inline choice list, or a value.
private final class StudioModelConfigurationController: UITableViewController {
    private enum Row {
        case amount(UInt32)
        case toggle(controlId: String, on: Bool)
        case choice(controlId: String, StudioControlChoice, selected: Bool)
        case value(String)
    }

    private struct Section {
        var title: String
        var rows: [Row]
        var footer: String?
    }

    private let model: StudioComposerModel
    private let store: StudioComposerStore
    private let service: StudioService
    private let deviceId: String
    private var sections: [Section] = []

    init(model: StudioComposerModel, store: StudioComposerStore, service: StudioService, deviceId: String) {
        self.model = model
        self.store = store
        self.service = service
        self.deviceId = deviceId
        super.init(style: .insetGrouped)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = model.displayName
        navigationItem.largeTitleDisplayMode = .never
        navigationItem.rightBarButtonItem = UIBarButtonItem(systemItem: .done, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: "row")
        tableView.register(StudioAmountCell.self, forCellReuseIdentifier: "amount")
    }

    override func updateProperties() {
        super.updateProperties()
        let chip = store.evaluation?.models.first { $0.modelId == model.id }
        let selected = store.snapshot.selected.first { $0.modelId == model.id }
        var sections: [Section] = []
        if store.snapshot.mode == .image, let selected {
            sections.append(Section(title: "Amount", rows: [.amount(selected.outputCount)]))
        }
        for control in chip?.controls ?? model.controls.filter({ $0.id != "duration" }) {
            let value = chip?.values[control.id] ?? selected?.controls[control.id]
            let rows: [Row]
            if control.kind == .boolean {
                rows = [.toggle(controlId: control.id, on: value == .boolean(true))]
            } else if !control.choices.isEmpty {
                let current = value ?? control.default ?? control.choices[0].value
                rows = control.choices.map { .choice(controlId: control.id, $0, selected: $0.value == current) }
            } else {
                rows = [.value(value?.label ?? "Default")]
            }
            let description = control.description.flatMap { $0.isEmpty ? nil : $0 }
            sections.append(Section(title: control.label, rows: rows, footer: description))
        }
        self.sections = sections
        tableView.reloadData()
    }

    override func numberOfSections(in tableView: UITableView) -> Int { sections.count }

    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        sections[section].rows.count
    }

    override func tableView(_ tableView: UITableView, titleForHeaderInSection section: Int) -> String? {
        sections[section].title
    }

    override func tableView(_ tableView: UITableView, titleForFooterInSection section: Int) -> String? {
        sections[section].footer
    }

    override func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let row = sections[indexPath.section].rows[indexPath.row]
        if case .amount(let count) = row {
            let cell = tableView.dequeueReusableCell(withIdentifier: "amount", for: indexPath) as! StudioAmountCell
            cell.configure(count: count) { [weak self] delta in
                guard let self else { return }
                self.store.adjustOutputCount(modelId: self.model.id, delta: delta, workspace: self.service, deviceId: self.deviceId)
            }
            return cell
        }
        let cell = tableView.dequeueReusableCell(withIdentifier: "row", for: indexPath)
        var content = cell.defaultContentConfiguration()
        content.textProperties.color = Palette.text
        cell.accessoryView = nil
        cell.accessoryType = .none
        cell.selectionStyle = .default
        switch row {
        case .amount:
            break
        case .toggle(let controlId, let on):
            content.text = "Enabled"
            let toggle = UISwitch()
            toggle.isOn = on
            toggle.addAction(UIAction { [weak self, weak toggle] _ in
                guard let toggle else { return }
                self?.set(controlId, .boolean(toggle.isOn))
            }, for: .valueChanged)
            cell.accessoryView = toggle
            cell.selectionStyle = .none
        case .choice(_, let choice, let selected):
            content.text = choice.label
            cell.accessoryType = selected ? .checkmark : .none
        case .value(let label):
            content.text = label
            content.textProperties.color = Palette.secondary
            cell.selectionStyle = .none
        }
        cell.contentConfiguration = content
        return cell
    }

    override func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
        tableView.deselectRow(at: indexPath, animated: true)
        guard case .choice(let controlId, let choice, _) = sections[indexPath.section].rows[indexPath.row] else { return }
        set(controlId, choice.value)
    }

    private func set(_ controlId: String, _ value: StudioControlValue) {
        store.setControl(modelId: model.id, controlId: controlId, value: value, workspace: service, deviceId: deviceId)
    }
}

/// − count + for how many outputs a model makes.
private final class StudioAmountCell: UITableViewCell {
    private let minus = UIButton(type: .system)
    private let plus = UIButton(type: .system)
    private let count = UILabel()
    private var adjust: ((Int) -> Void)?

    override init(style: UITableViewCell.CellStyle, reuseIdentifier: String?) {
        super.init(style: style, reuseIdentifier: reuseIdentifier)
        selectionStyle = .none
        minus.setImage(UIImage(systemName: "minus"), for: .normal)
        minus.accessibilityLabel = "Fewer"
        minus.addAction(UIAction { [weak self] _ in self?.adjust?(-1) }, for: .touchUpInside)
        plus.setImage(UIImage(systemName: "plus"), for: .normal)
        plus.accessibilityLabel = "More"
        plus.addAction(UIAction { [weak self] _ in self?.adjust?(1) }, for: .touchUpInside)
        count.font = UIFont.monospacedDigitSystemFont(ofSize: 17, weight: .regular)
        count.textColor = Palette.text
        count.textAlignment = .center
        let row = UIStackView(arrangedSubviews: [minus, count, plus])
        row.distribution = .equalCentering
        row.alignment = .center
        row.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: contentView.layoutMarginsGuide.leadingAnchor),
            row.trailingAnchor.constraint(equalTo: contentView.layoutMarginsGuide.trailingAnchor),
            row.topAnchor.constraint(equalTo: contentView.layoutMarginsGuide.topAnchor),
            row.bottomAnchor.constraint(equalTo: contentView.layoutMarginsGuide.bottomAnchor),
            minus.widthAnchor.constraint(equalToConstant: 44),
            plus.widthAnchor.constraint(equalToConstant: 44),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(count value: UInt32, adjust: @escaping (Int) -> Void) {
        count.text = "\(value)"
        self.adjust = adjust
    }
}

/// A conflict the host found in the draft: what's wrong, and its fixes.
private final class StudioConflictController: UIViewController {
    private let conflict: StudioComposerConflict
    private let resolve: (StudioConflictAction) -> Void

    init(conflict: StudioComposerConflict, resolve: @escaping (StudioConflictAction) -> Void) {
        self.conflict = conflict
        self.resolve = resolve
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        title = "Resolve"
        navigationItem.largeTitleDisplayMode = .never
        navigationItem.leftBarButtonItem = UIBarButtonItem(systemItem: .cancel, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })

        let icon = UIImageView(image: UIImage(systemName: "exclamationmark.triangle", withConfiguration: UIImage.SymbolConfiguration(pointSize: 17, weight: .semibold)))
        icon.tintColor = Palette.text
        icon.setContentHuggingPriority(.required, for: .horizontal)
        let title = UILabel()
        title.text = conflict.title
        title.font = Fonts.ui(.sansSemibold, 17)
        title.textColor = Palette.text
        title.numberOfLines = 0
        let heading = UIStackView(arrangedSubviews: [icon, title])
        heading.spacing = 8
        heading.alignment = .firstBaseline

        let explanation = UILabel()
        explanation.text = conflict.explanation
        explanation.font = Fonts.ui(.sans, 14)
        explanation.textColor = Palette.secondary
        explanation.numberOfLines = 0

        let actions = UIStackView()
        actions.axis = .vertical
        actions.spacing = 10
        for action in conflict.actions {
            var config = UIButton.Configuration.borderedProminent()
            config.title = action.label
            config.cornerStyle = .capsule
            let button = UIButton(configuration: config, primaryAction: UIAction { [weak self] _ in
                self?.resolve(action)
                self?.dismiss(animated: true)
            })
            actions.addArrangedSubview(button)
        }

        let stack = UIStackView(arrangedSubviews: [heading, explanation, actions])
        stack.axis = .vertical
        stack.spacing = 18
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 20),
            stack.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -20),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -20),
        ])
    }
}

// MARK: - Helpers

/// The host's attachment budgets as short readouts ("2/4 images").
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

/// A picked file's kind, by MIME type or, failing that, its magic bytes.
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
