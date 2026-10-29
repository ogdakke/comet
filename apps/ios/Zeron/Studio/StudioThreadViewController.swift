import Observation
import UIKit

/// One thread off the host's watch stream.
@MainActor
@Observable
private final class StudioThreadStore {
    var thread: StudioThread?
    var loading = true
    var error: String?

    func watch(service: StudioService, deviceId: String, threadId: String) async {
        loading = true
        error = nil
        do {
            let stream = try service.watchStudioThread(deviceId: deviceId, threadId: threadId)
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

/// One Studio thread: each turn's prompt, its runs (model, state, a
/// two-column grid of what they made), and the composer docked below.
/// Thumbnails open the viewer, zooming from the tapped tile.
///
/// The stores are @Observable; `updateProperties()` reads them, so UIKit
/// re-runs it whenever what it read changes.
final class StudioThreadViewController: UIViewController, UICollectionViewDelegate {
    private enum Section: Hashable {
        case prompt(String)
        case run(String)
        case grid(String)
    }

    private enum Item: Hashable {
        case prompt(String)
        case run(String)
        case artifact(String)
    }

    private let threadId: String
    private let browser: StudioBrowserStore
    private let service: StudioService
    private let transitionSource: StudioGalleryTransitionSource
    private let openViewer: (StudioViewerSession) -> Void
    private let store = StudioThreadStore()
    private let composerStore: StudioComposerStore

    private var collectionView: UICollectionView!
    private var dataSource: UICollectionViewDiffableDataSource<Section, Item>!
    private var composer: StudioComposerView!
    private let loadingView = StudioLoadingView("Loading thread…")
    private let bottom = UIStackView()
    private let status = StudioThreadStatusView()
    private let bottomFade = EdgeFadeOverlay()
    private let header = StudioThreadHeaderView()
    private var headerWidth: NSLayoutConstraint!

    /// The device + thread the watch tasks run for; a new device restarts them.
    private var watchKey: String?
    private var watchTasks: [Task<Void, Never>] = []
    private var shown: StudioThread?
    private var turns: [String: StudioTurn] = [:]
    private var runs: [String: (run: StudioRun, turn: StudioTurn)] = [:]
    private var artifacts: [String: (artifact: StudioArtifact, run: StudioRun, turn: StudioTurn)] = [:]

    init(
        threadId: String,
        browser: StudioBrowserStore,
        service: StudioService,
        transitionSource: StudioGalleryTransitionSource,
        openViewer: @escaping (StudioViewerSession) -> Void
    ) {
        self.threadId = threadId
        self.browser = browser
        self.service = service
        self.transitionSource = transitionSource
        self.openViewer = openViewer
        composerStore = StudioComposerStore(threadId: threadId)
        super.init(nibName: nil, bundle: nil)
        hidesBottomBarWhenPushed = true
    }

    required init?(coder: NSCoder) { fatalError() }

    deinit {
        watchTasks.forEach { $0.cancel() }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.largeTitleDisplayMode = .never
        // The title and "Studio @ device" lead the bar instead of centering.
        navigationItem.titleView = UIView()
        navigationItem.leftItemsSupplementBackButton = true
        header.translatesAutoresizingMaskIntoConstraints = false
        headerWidth = header.widthAnchor.constraint(equalToConstant: 140)
        headerWidth.isActive = true
        let headerItem = UIBarButtonItem(customView: header)
        headerItem.hidesSharedBackground = true
        navigationItem.leftBarButtonItem = headerItem

        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: makeLayout())
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.alwaysBounceVertical = true
        collectionView.keyboardDismissMode = .interactive
        // A run's error row appearing (a constraint change, not a content
        // size one) must grow its cell, not draw over the header.
        collectionView.selfSizingInvalidation = .enabledIncludingConstraints
        collectionView.delegate = self
        collectionView.accessibilityIdentifier = "studio-thread"
        view.addSubview(collectionView)
        setContentScrollView(collectionView, for: .top)
        collectionView.topEdgeEffect.style = .soft
        collectionView.bottomEdgeEffect.isHidden = true
        makeDataSource()

        loadingView.frame = view.bounds
        loadingView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(loadingView)

        composer = StudioComposerView(store: composerStore, browser: browser, service: service)
        composer.onHeightChange = { [weak self] in self?.view.setNeedsLayout() }
        bottom.axis = .vertical
        bottom.alignment = .fill
        bottom.translatesAutoresizingMaskIntoConstraints = false
        let statusRow = UIView()
        status.translatesAutoresizingMaskIntoConstraints = false
        statusRow.addSubview(status)
        bottom.addArrangedSubview(statusRow)
        bottom.addArrangedSubview(composer)
        bottom.isHidden = true
        view.addSubview(bottom)
        // The feed fades into the background behind the composer.
        bottomFade.translatesAutoresizingMaskIntoConstraints = false
        view.insertSubview(bottomFade, belowSubview: bottom)
        NSLayoutConstraint.activate([
            statusRow.heightAnchor.constraint(equalToConstant: 24),
            status.leadingAnchor.constraint(equalTo: statusRow.leadingAnchor, constant: 14),
            status.trailingAnchor.constraint(lessThanOrEqualTo: statusRow.trailingAnchor),
            status.centerYAnchor.constraint(equalTo: statusRow.centerYAnchor),

            bottomFade.topAnchor.constraint(equalTo: bottom.topAnchor, constant: -44),
            bottomFade.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            bottomFade.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            bottomFade.bottomAnchor.constraint(equalTo: view.bottomAnchor),

            // Full width on phones; a centered column on iPad/landscape.
            bottom.centerXAnchor.constraint(equalTo: view.safeAreaLayoutGuide.centerXAnchor),
            bottom.leadingAnchor.constraint(greaterThanOrEqualTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 12),
            bottom.widthAnchor.constraint(lessThanOrEqualToConstant: 768),
            {
                let fill = bottom.widthAnchor.constraint(equalTo: view.safeAreaLayoutGuide.widthAnchor, constant: -24)
                fill.priority = .defaultHigh
                return fill
            }(),
        ])
        // 8pt above the keyboard; at rest 8pt above the safe area.
        view.keyboardLayoutGuide.usesBottomSafeArea = false
        let down = bottom.bottomAnchor.constraint(equalTo: view.bottomAnchor)
        down.priority = .defaultLow
        NSLayoutConstraint.activate([
            bottom.bottomAnchor.constraint(lessThanOrEqualTo: view.keyboardLayoutGuide.topAnchor, constant: -8),
            bottom.bottomAnchor.constraint(lessThanOrEqualTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -8),
            down,
        ])
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        guard isMovingFromParent || navigationController?.isBeingDismissed == true else { return }
        watchTasks.forEach { $0.cancel() }
        watchTasks = []
        watchKey = nil
    }

    override func viewWillLayoutSubviews() {
        super.viewWillLayoutSubviews()
        headerWidth.constant = max(140, view.bounds.width - 170)
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        guard !bottom.isHidden else { return }
        let covered = view.bounds.maxY - bottom.frame.minY
        let inset = max(0, covered - view.safeAreaInsets.bottom)
        guard abs(collectionView.contentInset.bottom - inset) > 0.5 else { return }
        let pinned = isAtBottom
        collectionView.contentInset.bottom = inset
        collectionView.verticalScrollIndicatorInsets.bottom = inset
        if pinned { scrollToBottom() }
    }

    // MARK: Rendering

    override func updateProperties() {
        super.updateProperties()
        let deviceId = browser.selectedDeviceId
        restartWatchIfNeeded(deviceId: deviceId)

        let thread = store.thread
        title = thread?.conversation.title ?? "Thread"
        header.isHidden = thread == nil
        if let thread {
            let subtitle = deviceId.map { id in
                "Studio @ \(service.hosts.first { $0.id == id }?.name ?? "desktop")"
            }
            header.update(title: thread.conversation.title, subtitle: subtitle)
        }

        loadingView.isHidden = !(store.loading && thread == nil)
        if let error = store.error, thread == nil {
            var unavailable = UIContentUnavailableConfiguration.empty()
            unavailable.image = UIImage(systemName: "exclamationmark.triangle")
            unavailable.text = "Thread unavailable"
            unavailable.secondaryText = error
            contentUnavailableConfiguration = unavailable
        } else {
            contentUnavailableConfiguration = nil
        }

        collectionView.isHidden = thread == nil
        bottom.isHidden = thread == nil
        bottomFade.isHidden = thread == nil
        guard let thread else { return }
        let active = thread.turns.flatMap(\.runs).first { $0.state.isCreating }
        status.update(label: active?.state.label)
        if thread != shown { apply(thread) }
    }

    private func restartWatchIfNeeded(deviceId: String?) {
        let key = "\(deviceId ?? "none")-\(threadId)"
        guard key != watchKey else { return }
        watchKey = key
        watchTasks.forEach { $0.cancel() }
        watchTasks = []
        guard let deviceId else { return }
        let (store, service, composer, threadId) = (store, service, composerStore, threadId)
        watchTasks = [
            Task {
                await service.markStudioThreadSeen(deviceId: deviceId, threadId: threadId)
                await store.watch(service: service, deviceId: deviceId, threadId: threadId)
            },
            Task { await composer.load(workspace: service, deviceId: deviceId) },
        ]
    }

    private func apply(_ thread: StudioThread) {
        let first = shown == nil
        let pinned = first || isAtBottom
        shown = thread
        turns = [:]
        runs = [:]
        artifacts = [:]
        var snapshot = NSDiffableDataSourceSnapshot<Section, Item>()
        for turn in thread.turns.sorted(by: { $0.position < $1.position }) {
            turns[turn.id] = turn
            snapshot.appendSections([.prompt(turn.id)])
            snapshot.appendItems([.prompt(turn.id)])
            for run in turn.runs.sorted(by: { $0.position < $1.position }) {
                runs[run.id] = (run, turn)
                snapshot.appendSections([.run(run.id)])
                snapshot.appendItems([.run(run.id)])
                guard !run.artifacts.isEmpty else { continue }
                snapshot.appendSections([.grid(run.id)])
                let sorted = run.artifacts.sorted { $0.outputPosition < $1.outputPosition }
                for artifact in sorted { artifacts[artifact.id] = (artifact, run, turn) }
                snapshot.appendItems(sorted.map { .artifact($0.id) })
            }
        }
        // Rows keep their identity; a run's state or a prompt can still change.
        snapshot.reconfigureItems(snapshot.itemIdentifiers)
        dataSource.apply(snapshot, animatingDifferences: !first) { [weak self] in
            if pinned { self?.scrollToBottom() }
        }
        if first {
            collectionView.layoutIfNeeded()
            scrollToBottom()
        }
    }

    private var isAtBottom: Bool {
        collectionView.contentOffset.y >= maxOffsetY - 40
    }

    private var maxOffsetY: CGFloat {
        max(
            -collectionView.adjustedContentInset.top,
            collectionView.contentSize.height + collectionView.adjustedContentInset.bottom - collectionView.bounds.height
        )
    }

    private func scrollToBottom() {
        collectionView.contentOffset.y = maxOffsetY
    }

    // MARK: Layout

    private func makeLayout() -> UICollectionViewCompositionalLayout {
        UICollectionViewCompositionalLayout { [weak self] index, environment in
            guard let self, let section = self.dataSource?.sectionIdentifier(for: index) else { return nil }
            let last = index == self.dataSource.snapshot().numberOfSections - 1
            let width = environment.container.effectiveContentSize.width - 32
            let layoutSection: NSCollectionLayoutSection
            var top: CGFloat
            switch section {
            case .prompt:
                layoutSection = Self.fullWidthSection()
                // 28 between turns.
                top = index == 0 ? 18 : 28
            case .run:
                layoutSection = Self.fullWidthSection()
                top = 16
            case .grid:
                let side = ((width - 3) / 2).rounded(.down)
                let item = NSCollectionLayoutItem(layoutSize: .init(
                    widthDimension: .absolute(side),
                    heightDimension: .absolute(side)
                ))
                let group = NSCollectionLayoutGroup.horizontal(
                    layoutSize: .init(widthDimension: .fractionalWidth(1), heightDimension: .absolute(side)),
                    repeatingSubitem: item,
                    count: 2
                )
                group.interItemSpacing = .fixed(3)
                layoutSection = NSCollectionLayoutSection(group: group)
                layoutSection.interGroupSpacing = 3
                top = 10
            }
            layoutSection.contentInsets = NSDirectionalEdgeInsets(top: top, leading: 16, bottom: last ? 12 : 0, trailing: 16)
            return layoutSection
        }
    }

    private static func fullWidthSection() -> NSCollectionLayoutSection {
        let size = NSCollectionLayoutSize(widthDimension: .fractionalWidth(1), heightDimension: .estimated(40))
        let group = NSCollectionLayoutGroup.vertical(layoutSize: size, subitems: [NSCollectionLayoutItem(layoutSize: size)])
        return NSCollectionLayoutSection(group: group)
    }

    private func makeDataSource() {
        let prompts = UICollectionView.CellRegistration<StudioPromptCell, String> { [weak self] cell, _, turnId in
            cell.configure(prompt: self?.turns[turnId]?.prompt ?? "")
        }
        let runCells = UICollectionView.CellRegistration<StudioRunCell, String> { [weak self] cell, _, runId in
            guard let run = self?.runs[runId]?.run else { return }
            cell.configure(run)
        }
        let artifactCells = UICollectionView.CellRegistration<StudioArtifactCell, String> { [weak self] cell, _, artifactId in
            guard let self, let entry = self.artifacts[artifactId] else { return }
            cell.configure(
                entry.artifact,
                modelName: entry.run.model.displayName,
                browser: self.browser,
                service: self.service,
                transitionSource: self.transitionSource
            )
        }
        dataSource = UICollectionViewDiffableDataSource(collectionView: collectionView) { collectionView, indexPath, item in
            switch item {
            case .prompt(let id): collectionView.dequeueConfiguredReusableCell(using: prompts, for: indexPath, item: id)
            case .run(let id): collectionView.dequeueConfiguredReusableCell(using: runCells, for: indexPath, item: id)
            case .artifact(let id): collectionView.dequeueConfiguredReusableCell(using: artifactCells, for: indexPath, item: id)
            }
        }
    }

    // MARK: Interaction

    func collectionView(_ collectionView: UICollectionView, shouldHighlightItemAt indexPath: IndexPath) -> Bool {
        if case .artifact = dataSource.itemIdentifier(for: indexPath) { return true }
        return false
    }

    /// Touch-down starts the display-sized original, so the viewer's first
    /// frame is often already sharp.
    func collectionView(_ collectionView: UICollectionView, didHighlightItemAt indexPath: IndexPath) {
        guard case .artifact(let id) = dataSource.itemIdentifier(for: indexPath),
              let entry = artifacts[id],
              let deviceId = browser.selectedDeviceId else { return }
        browser.preheatDisplayImage(
            artifact: StudioArtifactDetail(artifact: entry.artifact, turn: entry.turn, run: entry.run, conversationId: threadId),
            deviceId: deviceId,
            workspace: service
        )
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: false)
        guard case .artifact(let id) = dataSource.itemIdentifier(for: indexPath),
              let thread = shown,
              let artifact = artifacts[id]?.artifact else { return }
        openViewer(StudioViewerSession(
            artifacts: viewerArtifacts(in: thread),
            selectedId: id,
            openedFromGallery: false,
            openingPreview: browser.cachedPreview(artifactId: id)
                ?? browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio)
        ))
    }

    func collectionView(_ collectionView: UICollectionView, contextMenuConfigurationForItemsAt indexPaths: [IndexPath], point: CGPoint) -> UIContextMenuConfiguration? {
        guard indexPaths.count == 1,
              case .prompt(let turnId) = dataSource.itemIdentifier(for: indexPaths[0]),
              let prompt = turns[turnId]?.prompt else { return nil }
        return UIContextMenuConfiguration(actionProvider: { _ in
            UIMenu(children: [UIAction(title: "Copy", image: UIImage(systemName: "doc.on.doc")) { _ in
                UIPasteboard.general.string = prompt
            }])
        })
    }

    private func viewerArtifacts(in thread: StudioThread) -> [StudioArtifactDetail] {
        thread.turns
            .sorted { $0.position < $1.position }
            .flatMap { turn in
                turn.runs
                    .sorted { $0.position < $1.position }
                    .flatMap { run in
                        run.artifacts
                            .sorted { $0.outputPosition < $1.outputPosition }
                            .map {
                                StudioArtifactDetail(
                                    artifact: $0,
                                    turn: turn,
                                    run: run,
                                    conversationId: thread.conversation.id
                                )
                            }
                    }
            }
    }
}

// MARK: - Pieces

/// The bar's leading title: a Studio glyph over the thread title and its host.
private final class StudioThreadHeaderView: UIView {
    private let titleLabel = UILabel()
    private let subtitleLabel = UILabel()

    override init(frame: CGRect) {
        super.init(frame: frame)
        let glyph = UIImageView(image: UIImage(systemName: "photo.stack", withConfiguration: UIImage.SymbolConfiguration(pointSize: 12, weight: .semibold)))
        glyph.tintColor = Palette.text
        glyph.setContentHuggingPriority(.required, for: .horizontal)
        titleLabel.font = Fonts.ui(.sansMedium, 13)
        titleLabel.textColor = Palette.text
        subtitleLabel.font = Fonts.ui(.sans, 10.5)
        subtitleLabel.textColor = Palette.secondary.withAlphaComponent(0.6)
        let text = UIStackView(arrangedSubviews: [titleLabel, subtitleLabel])
        text.axis = .vertical
        text.spacing = 1
        let row = UIStackView(arrangedSubviews: [glyph, text])
        row.spacing = 6
        row.alignment = .top
        row.translatesAutoresizingMaskIntoConstraints = false
        addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: leadingAnchor),
            row.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
            row.topAnchor.constraint(equalTo: topAnchor),
            row.bottomAnchor.constraint(equalTo: bottomAnchor),
            glyph.topAnchor.constraint(equalTo: row.topAnchor, constant: 2),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func update(title: String, subtitle: String?) {
        titleLabel.text = title
        subtitleLabel.text = subtitle
        subtitleLabel.isHidden = subtitle == nil
    }
}

/// Above the composer while a run is creating: spinner + its state.
private final class StudioThreadStatusView: UIStackView {
    private let spinner = UIActivityIndicatorView(style: .medium)
    private let label = UILabel()

    override init(frame: CGRect) {
        super.init(frame: frame)
        spacing = 7
        alignment = .center
        spinner.transform = CGAffineTransform(scaleX: 0.6, y: 0.6)
        spinner.color = Palette.secondary
        label.font = Fonts.ui(.sans, 11)
        label.textColor = Palette.secondary
        addArrangedSubview(spinner)
        addArrangedSubview(label)
    }

    required init(coder: NSCoder) { fatalError() }

    func update(label text: String?) {
        isHidden = text == nil
        label.text = text
        if text == nil { spinner.stopAnimating() } else { spinner.startAnimating() }
    }
}

/// A turn's prompt, right-aligned in the user bubble.
private final class StudioPromptCell: UICollectionViewCell {
    private let bubble = UIView()
    private let label = UILabel()

    override init(frame: CGRect) {
        super.init(frame: frame)
        bubble.backgroundColor = Palette.userBubble
        bubble.layer.cornerRadius = 18
        bubble.layer.cornerCurve = .continuous
        bubble.translatesAutoresizingMaskIntoConstraints = false
        label.font = Fonts.ui(.sans, 16)
        label.textColor = Palette.text
        label.numberOfLines = 0
        label.translatesAutoresizingMaskIntoConstraints = false
        bubble.addSubview(label)
        contentView.addSubview(bubble)
        NSLayoutConstraint.activate([
            bubble.topAnchor.constraint(equalTo: contentView.topAnchor),
            bubble.bottomAnchor.constraint(equalTo: contentView.bottomAnchor),
            bubble.trailingAnchor.constraint(equalTo: contentView.trailingAnchor),
            bubble.widthAnchor.constraint(lessThanOrEqualTo: contentView.widthAnchor, multiplier: 0.8),
            label.topAnchor.constraint(equalTo: bubble.topAnchor, constant: 10),
            label.bottomAnchor.constraint(equalTo: bubble.bottomAnchor, constant: -10),
            label.leadingAnchor.constraint(equalTo: bubble.leadingAnchor, constant: 16),
            label.trailingAnchor.constraint(equalTo: bubble.trailingAnchor, constant: -16),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(prompt: String) {
        label.text = prompt
    }
}

/// A run's header: its model, its state (spinner or progress while
/// creating), and the provider's error when it made nothing.
private final class StudioRunCell: UICollectionViewCell {
    private let model = UILabel()
    private let state = UILabel()
    private let spinner = UIActivityIndicatorView(style: .medium)
    private let progress = UIProgressView(progressViewStyle: .default)
    private let error = UILabel()
    private let errorRow = UIStackView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        model.font = Fonts.ui(.sansMedium, 12)
        model.textColor = Palette.secondary
        model.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        spinner.transform = CGAffineTransform(scaleX: 0.6, y: 0.6)
        spinner.color = Palette.color(.textFaint)
        progress.progressTintColor = Palette.color(.textFaint)
        progress.widthAnchor.constraint(equalToConstant: 46).isActive = true
        let status = UIStackView(arrangedSubviews: [progress, spinner, state])
        status.spacing = 7
        status.alignment = .center
        status.setContentHuggingPriority(.required, for: .horizontal)
        let head = UIStackView(arrangedSubviews: [model, UIView(), status])
        head.spacing = 8
        head.alignment = .center

        let warning = UIImageView(image: UIImage(systemName: "exclamationmark.triangle", withConfiguration: UIImage.SymbolConfiguration(pointSize: 11, weight: .medium)))
        warning.tintColor = Palette.color(.dangerSoft)
        warning.setContentHuggingPriority(.required, for: .horizontal)
        error.font = Fonts.ui(.sans, 12)
        error.textColor = Palette.color(.dangerSoft)
        error.numberOfLines = 0
        errorRow.addArrangedSubview(warning)
        errorRow.addArrangedSubview(error)
        errorRow.spacing = 6
        errorRow.alignment = .firstBaseline

        let stack = UIStackView(arrangedSubviews: [head, errorRow])
        stack.axis = .vertical
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: contentView.topAnchor),
            stack.bottomAnchor.constraint(equalTo: contentView.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: contentView.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: contentView.trailingAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(_ run: StudioRun) {
        model.text = run.model.displayName
        state.text = run.state.label
        if run.state.isCreating {
            state.font = Fonts.ui(.sans, 11)
            state.textColor = Palette.color(.textFaint)
            progress.isHidden = run.progress == nil
            progress.progress = run.progress ?? 0
            spinner.isHidden = run.progress != nil
            if run.progress == nil { spinner.startAnimating() } else { spinner.stopAnimating() }
        } else {
            state.font = Fonts.ui(.sansMedium, 11)
            state.textColor = run.state == .succeeded ? Palette.success : Palette.color(.textFaint)
            progress.isHidden = true
            spinner.stopAnimating()
            spinner.isHidden = true
        }
        let message = run.artifacts.isEmpty ? run.error.flatMap { $0.isEmpty ? nil : $0 } : nil
        error.text = message
        errorRow.isHidden = message == nil
    }
}

/// One square thumbnail; videos carry a play glyph.
private final class StudioArtifactCell: UICollectionViewCell {
    private var preview: StudioMediaPreviewView?
    private let play = UIImageView(image: UIImage(systemName: "play.fill", withConfiguration: UIImage.SymbolConfiguration(pointSize: 11, weight: .semibold)))

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = Palette.chip
        contentView.clipsToBounds = true
        play.tintColor = .white
        play.layer.shadowColor = UIColor.black.cgColor
        play.layer.shadowOpacity = 0.5
        play.layer.shadowRadius = 2
        play.layer.shadowOffset = .zero
        play.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(play)
        NSLayoutConstraint.activate([
            play.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -7),
            play.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -7),
        ])
        isAccessibilityElement = true
        accessibilityTraits = .button
    }

    required init?(coder: NSCoder) { fatalError() }

    override func prepareForReuse() {
        super.prepareForReuse()
        preview?.reset()
    }

    func configure(
        _ artifact: StudioArtifact,
        modelName: String,
        browser: StudioBrowserStore,
        service: StudioService,
        transitionSource: StudioGalleryTransitionSource
    ) {
        let preview = preview ?? {
            let view = StudioMediaPreviewView(browser: browser, service: service)
            view.frame = contentView.bounds
            view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            contentView.insertSubview(view, at: 0)
            self.preview = view
            return view
        }()
        preview.configure(
            artifactId: artifact.id,
            mediaKind: artifact.mediaKind,
            thumbhash: artifact.thumbhash,
            aspectRatio: artifact.aspectRatio,
            contentMode: .scaleAspectFill,
            transitionSource: transitionSource
        )
        play.isHidden = artifact.mediaKind != .video
        accessibilityLabel = "Open \(artifact.mediaKind.rawValue) from \(modelName)"
    }
}

// MARK: - Artifact details

/// One artifact's details: its preview, prompt, model and file facts.
final class StudioArtifactViewController: UIViewController {
    private let artifact: StudioArtifactDetail
    private let browser: StudioBrowserStore
    private let service: StudioService

    init(artifact: StudioArtifactDetail, browser: StudioBrowserStore, service: StudioService) {
        self.artifact = artifact
        self.browser = browser
        self.service = service
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        title = artifact.mediaKind == .video ? "Video" : "Image"
        navigationItem.largeTitleDisplayMode = .never

        let scroll = UIScrollView(frame: view.bounds)
        scroll.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        scroll.alwaysBounceVertical = true
        view.addSubview(scroll)
        setContentScrollView(scroll, for: .top)
        scroll.topEdgeEffect.style = .soft

        let media = StudioMediaPreviewView(browser: browser, service: service)
        media.configure(
            artifactId: artifact.id,
            mediaKind: artifact.mediaKind,
            thumbhash: artifact.thumbhash,
            aspectRatio: artifact.aspectRatio,
            contentMode: .scaleAspectFit
        )
        media.layer.cornerRadius = 14
        media.layer.cornerCurve = .continuous
        media.translatesAutoresizingMaskIntoConstraints = false
        media.widthAnchor.constraint(equalTo: media.heightAnchor, multiplier: max(artifact.aspectRatio, 0.01)).isActive = true
        if artifact.mediaKind == .video {
            let badge = Glass.surface()
            badge.translatesAutoresizingMaskIntoConstraints = false
            let glyph = UIImageView(image: UIImage(systemName: "play.slash", withConfiguration: UIImage.SymbolConfiguration(pointSize: 28, weight: .medium)))
            glyph.tintColor = .white
            glyph.translatesAutoresizingMaskIntoConstraints = false
            badge.contentView.addSubview(glyph)
            media.addSubview(badge)
            NSLayoutConstraint.activate([
                badge.centerXAnchor.constraint(equalTo: media.centerXAnchor),
                badge.centerYAnchor.constraint(equalTo: media.centerYAnchor),
                badge.widthAnchor.constraint(equalTo: glyph.widthAnchor, constant: 32),
                badge.heightAnchor.constraint(equalTo: badge.widthAnchor),
                glyph.centerXAnchor.constraint(equalTo: badge.contentView.centerXAnchor),
                glyph.centerYAnchor.constraint(equalTo: badge.contentView.centerYAnchor),
            ])
        }

        let prompt = UILabel()
        prompt.text = artifact.prompt
        prompt.font = Fonts.ui(.sansMedium, 16)
        prompt.textColor = Palette.text
        prompt.numberOfLines = 0
        let facts = UIStackView(arrangedSubviews: [prompt] + rows().map(Self.row))
        facts.axis = .vertical
        facts.spacing = 12

        let caption = UILabel()
        caption.text = artifact.mediaKind == .video
            ? "Video playback is not available on iPhone yet. This is the optimized preview."
            : "Optimized preview"
        caption.font = Fonts.ui(.sans, 11)
        caption.textColor = Palette.color(.textFaint)
        caption.numberOfLines = 0

        let stack = UIStackView(arrangedSubviews: [media, facts, caption])
        stack.axis = .vertical
        stack.spacing = 18
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 16),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -16),
            stack.leadingAnchor.constraint(equalTo: scroll.frameLayoutGuide.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: scroll.frameLayoutGuide.trailingAnchor, constant: -16),
        ])
    }

    private func rows() -> [(String, String)] {
        var rows = [("Model", artifact.modelDisplayName)]
        if let width = artifact.width, let height = artifact.height {
            rows.append(("Dimensions", "\(width) × \(height)"))
        }
        if let seconds = artifact.durationSeconds {
            let total = max(0, Int(seconds.rounded()))
            rows.append(("Duration", String(format: "%d:%02d", total / 60, total % 60)))
        }
        rows.append(("Size", ByteCountFormatter.string(fromByteCount: Int64(artifact.sizeBytes), countStyle: .file)))
        rows.append(("Created", artifact.createdDate.formatted(.relative(presentation: .named))))
        return rows
    }

    private static func row(_ entry: (String, String)) -> UIView {
        let label = UILabel()
        label.text = entry.0
        let value = UILabel()
        value.text = entry.1
        value.textAlignment = .right
        for l in [label, value] {
            l.font = Fonts.ui(.sans, 12)
            l.textColor = Palette.secondary
        }
        label.setContentHuggingPriority(.required, for: .horizontal)
        let row = UIStackView(arrangedSubviews: [label, value])
        row.spacing = 12
        return row
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
