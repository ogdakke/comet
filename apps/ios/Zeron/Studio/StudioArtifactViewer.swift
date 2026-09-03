import AVKit
import UIKit

enum StudioViewerSnapResolver {
    static func targetOffset(
        current: CGFloat,
        proposed: CGFloat,
        velocity: CGFloat,
        peek: CGFloat
    ) -> CGFloat {
        let proposed = max(0, proposed)
        if current < peek * 0.55 {
            return velocity > 0.28 || proposed > peek * 0.45 ? peek : 0
        }
        if current <= peek + 50 {
            if velocity < -0.28 { return 0 }
            if velocity > 0.62, proposed > peek + 60 { return proposed }
            return peek
        }
        return proposed < peek + 45 ? peek : proposed
    }
}

@MainActor
final class StudioViewerMediaStore {
    var changed: ((String) -> Void)?

    private let browser: StudioBrowserStore
    private var previews: [String: UIImage] = [:]
    private var images: [String: UIImage] = [:]
    private var players: [String: AVPlayer] = [:]
    private var videoStreams: [String: StudioVideoStream] = [:]
    private var previewTasks: [String: Task<Void, Never>] = [:]
    private var originalTasks: [String: Task<Void, Never>] = [:]
    private var retainedIds: Set<String> = []
    private var selectedId: String?

    init(
        browser: StudioBrowserStore,
        openingPreview: UIImage?,
        openingPreviewArtifactId: String
    ) {
        self.browser = browser
        if let openingPreview { previews[openingPreviewArtifactId] = openingPreview }
    }

    func displayImage(for artifact: StudioArtifactDetail) -> UIImage? {
        images[artifact.id]
            ?? browser.cachedDisplayImage(artifactId: artifact.id)
            ?? previews[artifact.id]
            ?? browser.cachedPreview(artifactId: artifact.id)
            ?? browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio)
    }

    func player(for artifactId: String) -> AVPlayer? { players[artifactId] }

    func prepare(
        selectedId: String,
        artifacts: [StudioArtifactDetail],
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard let selectedIndex = artifacts.firstIndex(where: { $0.id == selectedId }) else { return }
        self.selectedId = selectedId

        let neighborhood = [0, -1, 1, -2, 2].compactMap { offset -> StudioArtifactDetail? in
            let index = selectedIndex + offset
            return artifacts.indices.contains(index) ? artifacts[index] : nil
        }
        retainedIds = Set(neighborhood.map(\.id))
        trimOutsideNeighborhood()

        for player in players.values { player.pause() }
        players[selectedId]?.play()

        for artifact in neighborhood {
            loadPreview(artifact, workspace: workspace, deviceId: deviceId)
        }
        if let selected = neighborhood.first {
            loadOriginal(selected, workspace: workspace, deviceId: deviceId)
        }
    }

    func reset() {
        previewTasks.values.forEach { $0.cancel() }
        originalTasks.values.forEach { $0.cancel() }
        previewTasks.removeAll()
        originalTasks.removeAll()
        for player in players.values { player.pause() }
        videoStreams.values.forEach { $0.cancel() }
        videoStreams.removeAll()
        previews.removeAll(keepingCapacity: false)
        images.removeAll(keepingCapacity: false)
        players.removeAll(keepingCapacity: false)
        retainedIds.removeAll(keepingCapacity: false)
    }

    private func loadPreview(
        _ artifact: StudioArtifactDetail,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        if previews[artifact.id] == nil,
           let immediate = browser.cachedPreview(artifactId: artifact.id)
                ?? browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio) {
            previews[artifact.id] = immediate
            changed?(artifact.id)
        }
        guard previews[artifact.id] == nil, previewTasks[artifact.id] == nil else { return }
        let artifactId = artifact.id
        previewTasks[artifactId] = Task { [weak self] in
            guard let self else { return }
            defer { self.previewTasks.removeValue(forKey: artifactId) }
            let image = await browser.preview(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            )
            guard !Task.isCancelled, retainedIds.contains(artifactId), let image else { return }
            previews[artifactId] = image
            changed?(artifactId)
        }
    }

    private func loadOriginal(
        _ artifact: StudioArtifactDetail,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        if artifact.mediaKind == .video {
            loadVideo(artifact, workspace: workspace, deviceId: deviceId)
            return
        }
        guard images[artifact.id] == nil, originalTasks[artifact.id] == nil else { return }
        let artifactId = artifact.id
        if let cached = browser.cachedDisplayImage(artifactId: artifactId) {
            images[artifactId] = cached
            changed?(artifactId)
            return
        }

        let request = browser.beginDisplayImageRequest(
            artifact: artifact,
            deviceId: deviceId,
            workspace: workspace
        )
        originalTasks[artifactId] = Task { [weak self] in
            guard let self else { return }
            defer {
                self.originalTasks.removeValue(forKey: artifactId)
                browser.endDisplayImageRequest(artifactId: artifactId)
            }
            let image = await request.value
            guard !Task.isCancelled, retainedIds.contains(artifactId), let image else { return }
            images[artifactId] = image
            changed?(artifactId)
        }
    }

    private func loadVideo(
        _ artifact: StudioArtifactDetail,
        workspace: WorkspaceStore,
        deviceId: String
    ) {
        guard videoStreams[artifact.id] == nil else {
            players[artifact.id]?.play()
            return
        }
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
        stream.player.play()
        changed?(artifactId)
    }

    private func trimOutsideNeighborhood() {
        for id in previewTasks.keys.filter({ !retainedIds.contains($0) }) {
            previewTasks[id]?.cancel()
            previewTasks.removeValue(forKey: id)
        }
        for id in originalTasks.keys.filter({ !retainedIds.contains($0) }) {
            originalTasks[id]?.cancel()
            originalTasks.removeValue(forKey: id)
            browser.endDisplayImageRequest(artifactId: id)
        }
        for id in previews.keys.filter({ !retainedIds.contains($0) }) { previews.removeValue(forKey: id) }
        for id in images.keys.filter({ !retainedIds.contains($0) }) { images.removeValue(forKey: id) }
        for id in players.keys.filter({ $0 != selectedId }) {
            players[id]?.pause()
            players.removeValue(forKey: id)
            videoStreams.removeValue(forKey: id)?.cancel()
        }
    }
}

@MainActor
final class StudioGalleryViewerController: UIViewController,
    UIScrollViewDelegate,
    StudioViewerPagerDelegate,
    StudioViewerFilmstripDelegate,
    UIContextMenuInteractionDelegate
{
    let session: StudioViewerSession

    private let browser: StudioBrowserStore
    private let workspace: WorkspaceStore?
    private let deviceId: String?
    private let requestDismissal: () -> Void
    private let requestThread: (String) -> Void
    private let selectedArtifactChanged: (String) -> Void
    private let media: StudioViewerMediaStore
    private let pager: StudioViewerPagerController
    private let filmstrip: StudioViewerFilmstripController

    private let backdrop = UIView()
    private let scrollView = UIScrollView()
    private let detailsView = UIView()
    private let promptLabel = UILabel()
    private let detailsStack = UIStackView()
    private let topChrome = UIView()
    private let bottomChrome = UIView()
    private let closeButton = UIButton(type: .system)
    private let moreButton = UIButton(type: .system)
    private let downloadButton = UIButton(type: .system)
    private let deleteButton = UIButton(type: .system)

    private var artifacts: [StudioArtifactDetail]
    private var selectedId: String
    private var detailsHeight: CGFloat = 520
    private var isDismissing = false
    private var isLoadingOlder = false
    private var saving = false
    private var mediaPrepareTask: Task<Void, Never>?
    private var didLayOutInitialSelection = false

    init(
        session: StudioViewerSession,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore?,
        deviceId: String?,
        requestDismissal: @escaping () -> Void,
        requestThread: @escaping (String) -> Void,
        selectedArtifactChanged: @escaping (String) -> Void = { _ in }
    ) {
        self.session = session
        self.browser = browser
        self.workspace = workspace
        self.deviceId = deviceId
        self.requestDismissal = requestDismissal
        self.requestThread = requestThread
        self.selectedArtifactChanged = selectedArtifactChanged
        artifacts = session.artifacts
        selectedId = session.selectedId

        let media = StudioViewerMediaStore(
            browser: browser,
            openingPreview: session.openingPreview,
            openingPreviewArtifactId: session.openingPreviewArtifactId
        )
        self.media = media
        pager = StudioViewerPagerController(
            artifacts: session.artifacts,
            selectedId: session.selectedId,
            media: media
        )
        filmstrip = StudioViewerFilmstripController(
            artifacts: session.artifacts,
            selectedId: session.selectedId,
            browser: browser,
            workspace: workspace,
            deviceId: deviceId
        )
        super.init(nibName: nil, bundle: nil)
        modalPresentationCapturesStatusBarAppearance = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override var prefersStatusBarHidden: Bool { false }
    override var preferredStatusBarStyle: UIStatusBarStyle { .lightContent }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear

        backdrop.backgroundColor = .black
        backdrop.alpha = 0.97
        view.addSubview(backdrop)

        scrollView.backgroundColor = .clear
        scrollView.alwaysBounceVertical = true
        scrollView.isDirectionalLockEnabled = true
        scrollView.showsVerticalScrollIndicator = false
        scrollView.contentInsetAdjustmentBehavior = .never
        scrollView.decelerationRate = .normal
        scrollView.delegate = self
        view.addSubview(scrollView)

        addChild(pager)
        scrollView.addSubview(pager.view)
        pager.didMove(toParent: self)
        pager.delegate = self
        pager.view.addInteraction(UIContextMenuInteraction(delegate: self))

        configureDetailsView()
        scrollView.addSubview(detailsView)

        addChild(filmstrip)
        bottomChrome.addSubview(filmstrip.view)
        filmstrip.didMove(toParent: self)
        filmstrip.delegate = self

        configureChrome()
        view.addSubview(topChrome)
        view.addSubview(bottomChrome)

        media.changed = { [weak self] artifactId in
            self?.pager.refresh(artifactId)
            self?.filmstrip.refresh(artifactId)
        }
        prepareSelectedMedia()
        rebuildDetails()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        let bounds = view.bounds
        backdrop.frame = bounds
        scrollView.frame = bounds
        pager.view.frame = CGRect(origin: .zero, size: bounds.size)

        detailsHeight = layoutDetails(width: bounds.width)
        detailsView.frame = CGRect(x: 0, y: bounds.height, width: bounds.width, height: detailsHeight)
        scrollView.contentSize = CGSize(width: bounds.width, height: bounds.height + detailsHeight)

        let safe = view.safeAreaInsets
        topChrome.frame = CGRect(x: 0, y: 0, width: bounds.width, height: safe.top + 58)
        closeButton.frame = CGRect(x: 14, y: safe.top + 7, width: 42, height: 42)
        moreButton.frame = CGRect(x: bounds.width - 56, y: safe.top + 7, width: 42, height: 42)

        let bottomHeight = safe.bottom + 116
        bottomChrome.frame = CGRect(
            x: 0,
            y: bounds.height - bottomHeight,
            width: bounds.width,
            height: bottomHeight
        )
        filmstrip.view.frame = CGRect(x: 0, y: 0, width: bounds.width, height: 58)
        downloadButton.frame = CGRect(x: 14, y: 68, width: 42, height: 42)
        deleteButton.frame = CGRect(x: bounds.width - 56, y: 68, width: 42, height: 42)

        if !didLayOutInitialSelection {
            didLayOutInitialSelection = true
            pager.select(selectedId, animated: false)
            filmstrip.select(selectedId, animated: false)
        }
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        session.presentationActive = true
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if presentingViewController == nil {
            session.presentationActive = false
            mediaPrepareTask?.cancel()
            media.reset()
        }
    }

    func viewerPager(_ pager: StudioViewerPagerController, settledOn artifactId: String) {
        select(artifactId, source: .pager)
    }

    func viewerFilmstrip(
        _ filmstrip: StudioViewerFilmstripController,
        centeredOn artifactId: String,
        settled: Bool
    ) {
        select(artifactId, source: .filmstrip, prepareImmediately: settled)
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        let offset = scrollView.contentOffset.y
        let chrome = max(0, min(1, 1 - max(offset, 0) / 88))
        topChrome.alpha = chrome
        bottomChrome.alpha = chrome
        backdrop.alpha = 0.97
    }

    func scrollViewWillEndDragging(
        _ scrollView: UIScrollView,
        withVelocity velocity: CGPoint,
        targetContentOffset: UnsafeMutablePointer<CGPoint>
    ) {
        let current = scrollView.contentOffset.y
        targetContentOffset.pointee.y = StudioViewerSnapResolver.targetOffset(
            current: current,
            proposed: targetContentOffset.pointee.y,
            velocity: velocity.y,
            peek: detailsPeekOffset
        )
    }

    func contextMenuInteraction(
        _ interaction: UIContextMenuInteraction,
        configurationForMenuAtLocation location: CGPoint
    ) -> UIContextMenuConfiguration? {
        UIContextMenuConfiguration(identifier: selectedId as NSString, previewProvider: nil) { [weak self] _ in
            guard let self else { return nil }
            return UIMenu(children: actions())
        }
    }

    func transitionImageFrame(in coordinateView: UIView) -> CGRect {
        pager.transitionImageFrame(in: coordinateView)
    }

    var canBeginInteractiveZoomDismissal: Bool {
        !isDismissing
            && scrollView.contentOffset.y <= 1
            && !pager.isCurrentPageZoomed
    }

    private enum SelectionSource: Equatable { case pager, filmstrip }

    private var selected: StudioArtifactDetail? {
        artifacts.first(where: { $0.id == selectedId })
    }

    private var detailsPeekOffset: CGFloat {
        min(280, max(190, view.bounds.height * 0.28))
    }

    private func select(
        _ artifactId: String,
        source: SelectionSource,
        prepareImmediately: Bool = true
    ) {
        guard artifacts.contains(where: { $0.id == artifactId }) else { return }
        if artifactId == selectedId {
            if prepareImmediately {
                mediaPrepareTask?.cancel()
                rebuildDetails()
                prepareSelectedMedia()
                loadOlderIfNeeded()
            }
            return
        }
        selectedId = artifactId
        session.selectedId = artifactId
        if prepareImmediately { selectedArtifactChanged(artifactId) }
        if source != .pager { pager.select(artifactId, animated: source != .filmstrip) }
        if source != .filmstrip { filmstrip.select(artifactId, animated: true) }
        if source != .filmstrip || prepareImmediately { rebuildDetails() }
        if prepareImmediately {
            mediaPrepareTask?.cancel()
            prepareSelectedMedia()
            loadOlderIfNeeded()
        } else {
            // The cached thumbnail switches synchronously. Delay only relay
            // work so a fast fling does not download every crossed original.
            pager.refresh(artifactId)
            mediaPrepareTask?.cancel()
            mediaPrepareTask = Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(100))
                guard !Task.isCancelled, let self, self.selectedId == artifactId else { return }
                self.prepareSelectedMedia()
                self.loadOlderIfNeeded()
            }
        }
    }

    private func prepareSelectedMedia() {
        guard let workspace, let deviceId else { return }
        media.prepare(
            selectedId: selectedId,
            artifacts: artifacts,
            workspace: workspace,
            deviceId: deviceId
        )
        pager.refresh(selectedId)
    }

    private func loadOlderIfNeeded() {
        guard session.openedFromGallery,
              !isLoadingOlder,
              let item = browser.gallery.first(where: { $0.id == selectedId }),
              browser.shouldLoadMore(after: item),
              let workspace,
              let deviceId else { return }
        isLoadingOlder = true
        Task { [weak self] in
            guard let self else { return }
            await browser.loadMoreGallery(workspace: workspace, deviceId: deviceId)
            guard !Task.isCancelled else { return }
            let updated = browser.gallery.map(StudioArtifactDetail.init(item:))
            artifacts = updated
            session.replaceArtifacts(with: updated)
            pager.replaceArtifacts(updated)
            filmstrip.replaceArtifacts(updated)
            isLoadingOlder = false
        }
    }

    private func configureChrome() {
        topChrome.backgroundColor = .clear
        bottomChrome.backgroundColor = .clear

        configureCircleButton(closeButton, systemName: "chevron.backward") { [weak self] in
            self?.dismissViewer()
        }
        closeButton.accessibilityLabel = "Close"
        topChrome.addSubview(closeButton)

        configureCircleButton(moreButton, systemName: "ellipsis") { }
        moreButton.showsMenuAsPrimaryAction = true
        moreButton.menu = UIMenu(children: actions())
        moreButton.accessibilityLabel = "More actions"
        topChrome.addSubview(moreButton)

        configureCircleButton(downloadButton, systemName: "square.and.arrow.down") { [weak self] in
            self?.downloadSelected()
        }
        downloadButton.accessibilityLabel = "Download"
        bottomChrome.addSubview(downloadButton)

        configureCircleButton(deleteButton, systemName: "trash", destructive: true) { [weak self] in
            self?.confirmDelete()
        }
        deleteButton.accessibilityLabel = "Delete"
        bottomChrome.addSubview(deleteButton)
    }

    private func configureCircleButton(
        _ button: UIButton,
        systemName: String,
        destructive: Bool = false,
        action: @escaping () -> Void
    ) {
        var configuration = UIButton.Configuration.glass()
        configuration.image = UIImage(systemName: systemName)
        configuration.baseForegroundColor = destructive ? .systemRed : .white
        configuration.cornerStyle = .capsule
        button.configuration = configuration
        button.addAction(UIAction { _ in action() }, for: .touchUpInside)
    }

    private func actions() -> [UIMenuElement] {
        var actions: [UIMenuElement] = [
            UIAction(title: "Download", image: UIImage(systemName: "square.and.arrow.down")) { [weak self] _ in
                self?.downloadSelected()
            },
        ]
        if let selected {
            actions.append(UIAction(title: "Show in Thread", image: UIImage(systemName: "rectangle.stack")) { [weak self] _ in
                self?.requestThread(selected.conversationId)
            })
        }
        actions.append(UIAction(
            title: "Delete",
            image: UIImage(systemName: "trash"),
            attributes: .destructive
        ) { [weak self] _ in
            self?.confirmDelete()
        })
        return actions
    }

    private func configureDetailsView() {
        detailsView.backgroundColor = UIColor(Theme.bg)
        promptLabel.numberOfLines = 0
        promptLabel.font = UIFont.systemFont(ofSize: 17, weight: .medium)
        promptLabel.textColor = UIColor(Theme.text)
        detailsView.addSubview(promptLabel)

        detailsStack.axis = .vertical
        detailsStack.spacing = 14
        detailsView.addSubview(detailsStack)
    }

    private func rebuildDetails() {
        promptLabel.text = selected?.prompt
        detailsStack.arrangedSubviews.forEach {
            detailsStack.removeArrangedSubview($0)
            $0.removeFromSuperview()
        }
        guard let selected else { return }
        detailsStack.addArrangedSubview(detailRow("Model", selected.modelDisplayName))
        if let width = selected.width, let height = selected.height {
            detailsStack.addArrangedSubview(detailRow("Dimensions", "\(width) × \(height)"))
        }
        if let seconds = selected.durationSeconds {
            let total = max(0, Int(seconds.rounded()))
            detailsStack.addArrangedSubview(detailRow("Duration", String(format: "%d:%02d", total / 60, total % 60)))
        }
        detailsStack.addArrangedSubview(detailRow(
            "Size",
            ByteCountFormatter.string(fromByteCount: Int64(selected.sizeBytes), countStyle: .file)
        ))
        detailsStack.addArrangedSubview(detailRow(
            "Created",
            selected.createdDate.formatted(.relative(presentation: .named))
        ))
        if isViewLoaded { view.setNeedsLayout() }
        moreButton.menu = UIMenu(children: actions())
    }

    private func detailRow(_ title: String, _ value: String) -> UIView {
        let row = UIView()
        let titleLabel = UILabel()
        titleLabel.text = title
        titleLabel.font = UIFont.systemFont(ofSize: 13)
        titleLabel.textColor = UIColor(Theme.textMuted)
        titleLabel.translatesAutoresizingMaskIntoConstraints = false
        row.addSubview(titleLabel)

        let valueLabel = UILabel()
        valueLabel.text = value
        valueLabel.font = UIFont.systemFont(ofSize: 13)
        valueLabel.textColor = UIColor(Theme.textMuted)
        valueLabel.textAlignment = .right
        valueLabel.translatesAutoresizingMaskIntoConstraints = false
        row.addSubview(valueLabel)

        NSLayoutConstraint.activate([
            titleLabel.leadingAnchor.constraint(equalTo: row.leadingAnchor),
            titleLabel.topAnchor.constraint(equalTo: row.topAnchor),
            titleLabel.bottomAnchor.constraint(equalTo: row.bottomAnchor),
            valueLabel.leadingAnchor.constraint(greaterThanOrEqualTo: titleLabel.trailingAnchor, constant: 12),
            valueLabel.trailingAnchor.constraint(equalTo: row.trailingAnchor),
            valueLabel.topAnchor.constraint(equalTo: row.topAnchor),
            valueLabel.bottomAnchor.constraint(equalTo: row.bottomAnchor),
            row.heightAnchor.constraint(greaterThanOrEqualToConstant: 18),
        ])
        return row
    }

    private func layoutDetails(width: CGFloat) -> CGFloat {
        let contentWidth = max(0, width - 40)
        let promptSize = promptLabel.sizeThatFits(CGSize(width: contentWidth, height: .greatestFiniteMagnitude))
        promptLabel.frame = CGRect(x: 20, y: 28, width: contentWidth, height: promptSize.height)
        let stackY = promptLabel.frame.maxY + 26
        let stackSize = detailsStack.systemLayoutSizeFitting(
            CGSize(width: contentWidth, height: UIView.layoutFittingCompressedSize.height),
            withHorizontalFittingPriority: .required,
            verticalFittingPriority: .fittingSizeLevel
        )
        detailsStack.frame = CGRect(x: 20, y: stackY, width: contentWidth, height: stackSize.height)
        return max(420, detailsStack.frame.maxY + view.safeAreaInsets.bottom + 120)
    }

    private func dismissViewer() {
        guard !isDismissing else { return }
        isDismissing = true
        scrollView.isScrollEnabled = false
        requestDismissal()
    }

    private func downloadSelected() {
        guard let selected, let workspace, let deviceId, !saving else { return }
        saving = true
        downloadButton.isEnabled = false
        Task { [weak self] in
            guard let self else { return }
            defer {
                saving = false
                downloadButton.isEnabled = true
            }
            do {
                try await StudioArtifactActions.download(
                    selected,
                    workspace: workspace,
                    deviceId: deviceId
                )
            } catch {
                presentError(error.localizedDescription)
            }
        }
    }

    private func confirmDelete() {
        guard selected != nil else { return }
        let alert = UIAlertController(
            title: "Delete this creation?",
            message: "This removes it from Studio on the selected machine.",
            preferredStyle: .actionSheet
        )
        alert.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak self] _ in
            self?.deleteSelected()
        })
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        present(alert, animated: true)
    }

    private func deleteSelected() {
        guard let selected, let workspace, let deviceId else { return }
        Task { [weak self] in
            guard let self else { return }
            do {
                try await workspace.deleteStudioArtifact(
                    deviceId: deviceId,
                    artifactId: selected.id
                )
                browser.removeArtifact(selected.id)
                dismissViewer()
            } catch {
                presentError(error.localizedDescription)
            }
        }
    }

    private func presentError(_ message: String) {
        let alert = UIAlertController(title: "Studio action failed", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }

#if DEBUG
    func selectFilmstripItemForTesting(at index: Int) {
        filmstrip.selectItemForTesting(at: index)
    }

    func centerFilmstripItemUsingCollectionGeometryForTesting(at index: Int) {
        filmstrip.centerItemUsingCollectionGeometryForTesting(at: index)
    }

    func runPerformanceFixtureSequence() {
        guard let initialIndex = artifacts.firstIndex(where: { $0.id == selectedId }) else { return }
        filmstrip.animateToItemForTesting(at: min(artifacts.count - 1, initialIndex + 24)) { [weak self] in
            guard let self else { return }
            self.filmstrip.animateToItemForTesting(at: initialIndex) { [weak self] in
                self?.runVerticalPerformanceFixtureSequence()
            }
        }
    }

    private func runVerticalPerformanceFixtureSequence() {
        let peek = detailsPeekOffset
        animateFixtureScroll(to: peek, duration: 0.5, damping: 0.84) { [weak self] in
            guard let self else { return }
            self.animateFixtureScroll(to: peek + 360, duration: 0.65, damping: 0.9) { [weak self] in
                guard let self else { return }
                self.animateFixtureScroll(to: peek, duration: 0.55, damping: 0.84) { [weak self] in
                    guard let self else { return }
                    self.animateFixtureScroll(to: 0, duration: 0.5, damping: 0.84) { [weak self] in
                        guard let self else { return }
                        self.animateFixtureScroll(to: -150, duration: 0.38, damping: 0.9) { [weak self] in
                            self?.dismissViewer()
                        }
                    }
                }
            }
        }
    }

    private func animateFixtureScroll(
        to offset: CGFloat,
        duration: TimeInterval,
        damping: CGFloat,
        completion: @escaping () -> Void
    ) {
        let animator = UIViewPropertyAnimator(duration: duration, dampingRatio: damping) {
            self.scrollView.contentOffset.y = offset
        }
        animator.addCompletion { _ in completion() }
        animator.startAnimation()
    }

    var selectedArtifactIdForTesting: String { selectedId }
    var residentPageCellCountForTesting: Int { pager.residentCellCountForTesting }
    var residentFilmstripCellCountForTesting: Int { filmstrip.residentCellCountForTesting }
#endif
}

@MainActor
protocol StudioViewerFilmstripDelegate: AnyObject {
    func viewerFilmstrip(
        _ filmstrip: StudioViewerFilmstripController,
        centeredOn artifactId: String,
        settled: Bool
    )
}

private final class StudioCenteredFilmstripLayout: UICollectionViewFlowLayout {
    override func targetContentOffset(
        forProposedContentOffset proposedContentOffset: CGPoint,
        withScrollingVelocity velocity: CGPoint
    ) -> CGPoint {
        guard let collectionView else { return proposedContentOffset }
        let proposedCenter = proposedContentOffset.x + collectionView.bounds.width / 2
        let rect = CGRect(
            x: proposedContentOffset.x,
            y: 0,
            width: collectionView.bounds.width,
            height: collectionView.bounds.height
        )
        guard let closest = layoutAttributesForElements(in: rect)?
            .filter({ $0.representedElementCategory == .cell })
            .min(by: { abs($0.center.x - proposedCenter) < abs($1.center.x - proposedCenter) }) else {
            return proposedContentOffset
        }
        return CGPoint(x: closest.center.x - collectionView.bounds.width / 2, y: proposedContentOffset.y)
    }
}

@MainActor
final class StudioViewerFilmstripController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate
{
    weak var delegate: StudioViewerFilmstripDelegate?

    private let layout = StudioCenteredFilmstripLayout()
    private lazy var collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
    private let browser: StudioBrowserStore
    private let workspace: WorkspaceStore?
    private let deviceId: String?
    private var artifacts: [StudioArtifactDetail]
    private var artifactIndex: [String: Int]
    private var selectedId: String
    private var pendingSelectionId: String?
    private var lastWidth: CGFloat = 0
    private var userDriving = false
    private var lastReportedCenteredId: String?

    init(
        artifacts: [StudioArtifactDetail],
        selectedId: String,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore?,
        deviceId: String?
    ) {
        self.artifacts = artifacts
        self.selectedId = selectedId
        self.browser = browser
        self.workspace = workspace
        self.deviceId = deviceId
        artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        layout.scrollDirection = .horizontal
        layout.itemSize = CGSize(width: 48, height: 48)
        layout.minimumLineSpacing = 6

        collectionView.frame = view.bounds
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.showsHorizontalScrollIndicator = false
        collectionView.contentInsetAdjustmentBehavior = .never
        collectionView.decelerationRate = .normal
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.register(
            StudioFilmstripCell.self,
            forCellWithReuseIdentifier: StudioFilmstripCell.reuseIdentifier
        )
        view.addSubview(collectionView)
        pendingSelectionId = selectedId
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        guard view.bounds.width > 0, view.bounds.width != lastWidth else { return }
        lastWidth = view.bounds.width
        let inset = max(0, (view.bounds.width - layout.itemSize.width) / 2)
        layout.sectionInset = UIEdgeInsets(top: 5, left: inset, bottom: 5, right: inset)
        layout.invalidateLayout()
        collectionView.layoutIfNeeded()
        positionPendingSelection()
    }

    func replaceArtifacts(_ artifacts: [StudioArtifactDetail]) {
        guard self.artifacts != artifacts else { return }
        self.artifacts = artifacts
        artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
        collectionView.reloadData()
        pendingSelectionId = selectedId
        collectionView.layoutIfNeeded()
        positionPendingSelection()
    }

    func select(_ artifactId: String, animated: Bool) {
        guard let index = artifactIndex[artifactId] else { return }
        selectedId = artifactId
        refreshVisibleSelection(centeredId: artifactId)
        guard isViewLoaded, collectionView.bounds.width > 0 else {
            pendingSelectionId = artifactId
            return
        }
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: animated
        )
    }

    func refresh(_ artifactId: String) {
        guard let index = artifactIndex[artifactId],
              let cell = collectionView.cellForItem(at: IndexPath(item: index, section: 0)) as? StudioFilmstripCell else {
            return
        }
        configure(cell, at: index)
    }

    func collectionView(
        _ collectionView: UICollectionView,
        numberOfItemsInSection section: Int
    ) -> Int { artifacts.count }

    func collectionView(
        _ collectionView: UICollectionView,
        cellForItemAt indexPath: IndexPath
    ) -> UICollectionViewCell {
        guard let cell = collectionView.dequeueReusableCell(
            withReuseIdentifier: StudioFilmstripCell.reuseIdentifier,
            for: indexPath
        ) as? StudioFilmstripCell,
        artifacts.indices.contains(indexPath.item) else { return UICollectionViewCell() }
        configure(cell, at: indexPath.item)
        return cell
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        guard artifacts.indices.contains(indexPath.item) else { return }
        let id = artifacts[indexPath.item].id
        selectedId = id
        lastReportedCenteredId = id
        refreshVisibleSelection(centeredId: id)
        collectionView.scrollToItem(at: indexPath, at: .centeredHorizontally, animated: true)
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: true)
    }

    func collectionView(
        _ collectionView: UICollectionView,
        didEndDisplaying cell: UICollectionViewCell,
        forItemAt indexPath: IndexPath
    ) {
        (cell as? StudioFilmstripCell)?.cancelPreviewRequest()
    }

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        userDriving = true
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        guard let id = centeredArtifactId() else { return }
        refreshVisibleSelection(centeredId: id)
        guard userDriving, id != lastReportedCenteredId else { return }
        lastReportedCenteredId = id
        selectedId = id
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: false)
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { settleSelection() }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) { settleSelection() }

    func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        refreshVisibleSelection(centeredId: selectedId)
    }

    private func positionPendingSelection() {
        guard let id = pendingSelectionId, let index = artifactIndex[id] else { return }
        pendingSelectionId = nil
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: false
        )
        refreshVisibleSelection(centeredId: id)
    }

    private func settleSelection() {
        guard let id = centeredArtifactId() else { return }
        selectedId = id
        lastReportedCenteredId = id
        refreshVisibleSelection(centeredId: id)
        userDriving = false
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: true)
    }

    private func centeredArtifactId() -> String? {
        let center = CGPoint(
            // UIScrollView moves its bounds origin with contentOffset, so
            // bounds.midX is already expressed in content coordinates.
            x: collectionView.bounds.midX,
            y: collectionView.bounds.midY
        )
        let indexPath = collectionView.indexPathForItem(at: center)
            ?? collectionView.indexPathsForVisibleItems.min { lhs, rhs in
                let left = collectionView.layoutAttributesForItem(at: lhs)?.center.x ?? 0
                let right = collectionView.layoutAttributesForItem(at: rhs)?.center.x ?? 0
                return abs(left - center.x) < abs(right - center.x)
            }
        guard let index = indexPath?.item, artifacts.indices.contains(index) else { return nil }
        return artifacts[index].id
    }

    private func refreshVisibleSelection(centeredId: String?) {
        for case let cell as StudioFilmstripCell in collectionView.visibleCells {
            guard let indexPath = collectionView.indexPath(for: cell),
                  artifacts.indices.contains(indexPath.item) else { continue }
            cell.setSelected(artifacts[indexPath.item].id == centeredId)
        }
    }

    private func configure(_ cell: StudioFilmstripCell, at index: Int) {
        let artifact = artifacts[index]
        cell.configure(
            artifact: artifact,
            selected: artifact.id == selectedId,
            browser: browser,
            workspace: workspace,
            deviceId: deviceId
        )
    }


#if DEBUG
    func selectItemForTesting(at index: Int) {
        guard artifacts.indices.contains(index) else { return }
        let id = artifacts[index].id
        userDriving = true
        lastReportedCenteredId = id
        selectedId = id
        refreshVisibleSelection(centeredId: id)
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: false)
    }

    func centerItemUsingCollectionGeometryForTesting(at index: Int) {
        guard artifacts.indices.contains(index), collectionView.bounds.width > 0 else { return }
        collectionView.layoutIfNeeded()
        guard let attributes = collectionView.layoutAttributesForItem(
            at: IndexPath(item: index, section: 0)
        ) else { return }
        userDriving = true
        collectionView.contentOffset.x = attributes.center.x - collectionView.bounds.width / 2
        scrollViewDidScroll(collectionView)
    }

    var residentCellCountForTesting: Int { collectionView.visibleCells.count }

    func animateToItemForTesting(at index: Int, completion: @escaping () -> Void) {
        guard artifacts.indices.contains(index), collectionView.bounds.width > 0 else {
            completion()
            return
        }
        collectionView.layoutIfNeeded()
        let target = IndexPath(item: index, section: 0)
        guard let attributes = collectionView.layoutAttributesForItem(at: target) else {
            completion()
            return
        }
        userDriving = true
        let offset = attributes.center.x - collectionView.bounds.width / 2
        UIView.animate(
            withDuration: 1.05,
            delay: 0,
            options: [.curveEaseOut, .allowUserInteraction]
        ) {
            self.collectionView.contentOffset.x = offset
        } completion: { _ in
            self.settleSelection()
            completion()
        }
    }
#endif
}

@MainActor
private final class StudioFilmstripCell: UICollectionViewCell {
    static let reuseIdentifier = "StudioFilmstripCell"

    private let imageView = UIImageView()
    private let placeholder = UIImageView()
    private var representedId: String?
    private var loadTask: Task<Void, Never>?
    private var browser: StudioBrowserStore?
    private var requestActive = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = UIColor(Theme.elementHover)
        contentView.layer.cornerRadius = 5
        contentView.clipsToBounds = true
        contentView.layer.borderColor = UIColor.white.cgColor

        imageView.frame = contentView.bounds
        imageView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        imageView.contentMode = .scaleAspectFill
        imageView.clipsToBounds = true
        contentView.addSubview(imageView)

        placeholder.frame = contentView.bounds
        placeholder.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        placeholder.contentMode = .center
        placeholder.tintColor = UIColor(Theme.textFaint.opacity(0.5))
        contentView.addSubview(placeholder)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func prepareForReuse() {
        super.prepareForReuse()
        cancelPreviewRequest()
        representedId = nil
        imageView.image = nil
        placeholder.image = nil
        placeholder.isHidden = false
        setSelected(false)
    }

    func configure(
        artifact: StudioArtifactDetail,
        selected: Bool,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore?,
        deviceId: String?
    ) {
        if representedId != artifact.id { cancelPreviewRequest() }
        representedId = artifact.id
        self.browser = browser
        setSelected(selected)
        placeholder.image = UIImage(systemName: artifact.mediaKind == .video ? "film" : "photo")
        if let cached = browser.cachedPreview(artifactId: artifact.id) {
            imageView.image = cached
            placeholder.isHidden = true
            return
        }

        let thumbhash = browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio)
        imageView.image = thumbhash
        placeholder.isHidden = thumbhash != nil
        guard loadTask == nil, let workspace, let deviceId else { return }
        let artifactId = artifact.id
        loadTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(90))
            guard !Task.isCancelled, let self, representedId == artifactId else { return }
            requestActive = true
            let request = browser.beginPreviewRequest(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            )
            let image = await request.value
            guard !Task.isCancelled, representedId == artifactId else { return }
            if let image { imageView.image = image }
            placeholder.isHidden = image != nil || thumbhash != nil
        }
    }

    func cancelPreviewRequest() {
        loadTask?.cancel()
        loadTask = nil
        if requestActive, let representedId {
            browser?.endPreviewRequest(artifactId: representedId)
        }
        requestActive = false
    }

    func setSelected(_ selected: Bool) {
        contentView.layer.borderWidth = selected ? 2 : 0
    }
}
