import Observation
import UIKit

/// One thread off the host's watch stream. A dropped stream (the desktop
/// went offline, the link reconnected) is reopened with backoff; meanwhile
/// `error` says why, over the last thread seen.
@MainActor
@Observable
private final class StudioThreadStore {
    var thread: StudioThread?
    var loading = true
    var error: String?

    /// Another desktop: what showed belongs to the old one.
    func reset() {
        thread = nil
        loading = true
        error = nil
    }

    /// A send's reply, ahead of the watch. Only taken when it has more turns
    /// than what shows: an older snapshot would rewind runs the watch has
    /// already moved on.
    func adopt(_ reply: StudioThread) {
        guard reply.conversation.id == thread?.conversation.id,
              reply.turns.count > thread?.turns.count ?? 0 else { return }
        thread = reply
    }

    /// A retry's reply: the run queued again. Taken while what shows still
    /// has it finished; once the watch shows it creating, the watch is newer.
    func adoptRetry(_ reply: StudioThread, runId: String) {
        guard reply.conversation.id == thread?.conversation.id,
              let shown = thread?.turns.flatMap(\.runs).first(where: { $0.id == runId }),
              !shown.state.isCreating else { return }
        thread = reply
    }

    func watch(service: StudioService, deviceId: String, threadId: String) async {
        loading = thread == nil
        var delay = Duration.seconds(1)
        while !Task.isCancelled {
            do {
                let stream = try service.watchStudioThread(deviceId: deviceId, threadId: threadId)
                for try await thread in stream {
                    guard !Task.isCancelled else { return }
                    self.thread = thread
                    loading = false
                    error = nil
                    delay = .seconds(1)
                }
                guard !Task.isCancelled else { return }
                error = "The desktop stopped sending updates"
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error.localizedDescription
            }
            loading = false
            try? await Task.sleep(for: delay)
            delay = min(delay * 2, .seconds(30))
        }
    }
}

/// One Studio thread: each turn's prompt, its runs (model, state, a
/// two-column grid of what they made), and the composer docked below.
/// Thumbnails open the viewer, zooming from the tapped tile.
///
/// Built for threads of a thousand outputs: rows are plain arrays (no
/// diffable snapshot, whose per-section appends go quadratic), and every
/// frame is computed here, prompts measured once (`StudioThreadLayout`), so
/// opening, scrolling and a progress tick never run a layout solver over
/// the whole thread.
///
/// The stores are @Observable; `updateProperties()` reads them, so UIKit
/// re-runs it whenever what it read changes.
final class StudioThreadViewController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate,
    UICollectionViewDataSourcePrefetching,
    StudioGalleryTransitionSourceProvider
{
    private enum Section: Hashable {
        case prompt(String)
        case run(String)
        case grid(String)
    }

    private enum Item: Hashable {
        case prompt(String)
        case run(String)
        case artifact(String)
        /// A tile still being made: the run's id and the output's slot.
        case pending(String, Int)
    }

    private let threadId: String
    /// "Show in Thread": the creation the thread opens on, instead of the
    /// newest turn at the bottom.
    private let revealArtifactId: String?
    private let browser: StudioBrowserStore
    private let service: StudioService
    private let transitionSource: StudioGalleryTransitionSource
    private let openViewer: (StudioViewerSession) -> Void
    private let store = StudioThreadStore()
    private let composerStore: StudioComposerStore

    private var collectionView: UICollectionView!
    private let layout = StudioThreadLayout()
    private var cellProvider: ((UICollectionView, IndexPath, Item) -> UICollectionViewCell)!
    private var composer: StudioComposerView!
    private let loadingView = StudioLoadingView("Loading thread…")
    private let bottom = UIStackView()
    private let status = StudioThreadStatusView()
    private let statusRow = UIView()
    private let archivedBar = StudioArchivedBar()
    private let bottomFade = EdgeFadeOverlay()
    private lazy var menuItem: UIBarButtonItem = {
        let item = UIBarButtonItem(image: UIImage(systemName: "ellipsis"), menu: UIMenu(children: [
            // Built as it opens: the title and archived state change.
            UIDeferredMenuElement.uncached { [weak self] done in done(self?.threadMenu() ?? []) },
        ]))
        item.accessibilityLabel = "Thread"
        item.accessibilityIdentifier = "studio-thread-menu"
        return item
    }()

    /// The device + thread the watch tasks run for; a new device restarts them.
    private var watchKey: String?
    private var watchTasks: [Task<Void, Never>] = []
    private var shown: StudioThread?
    private var turns: [String: StudioTurn] = [:]
    private var runs: [String: (run: StudioRun, turn: StudioTurn)] = [:]
    private var artifacts: [String: (artifact: StudioArtifact, run: StudioRun, turn: StudioTurn)] = [:]
    /// What's shown, in order: sections and each one's items.
    private var sections: [Section] = []
    private var sectionItems: [[Item]] = []
    private var itemPaths: [Item: IndexPath] = [:]
    /// Measured prompt and error heights, by width and text.
    private var textHeights: [String: CGFloat] = [:]
    private var measuredWidth: CGFloat = 0
    /// Previews requested ahead of the scroll, held until their tile shows.
    private var prefetchedIds: Set<String> = []
    /// Runs whose retry is on its way: their row shows a spinner, not Retry.
    private var retrying: Set<String> = []

    init(
        threadId: String,
        revealing artifactId: String? = nil,
        browser: StudioBrowserStore,
        service: StudioService,
        transitionSource: StudioGalleryTransitionSource,
        openViewer: @escaping (StudioViewerSession) -> Void
    ) {
        self.threadId = threadId
        revealArtifactId = artifactId
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
        // The bar's own title over "Studio @ device": it sits in the bar's
        // scroll-edge pocket, legible over the tiles scrolling under it.
        navigationItem.title = ""
        navigationItem.rightBarButtonItem = menuItem

        layout.frameProvider = { [weak self] in self?.computeFrames() ?? [] }
        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: layout)
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.alwaysBounceVertical = true
        collectionView.keyboardDismissMode = .interactive
        // Every height is computed (`frames`): cells never size themselves.
        collectionView.selfSizingInvalidation = .disabled
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.prefetchDataSource = self
        collectionView.accessibilityIdentifier = "studio-thread"
        view.addSubview(collectionView)
        setContentScrollView(collectionView, for: .top)
        collectionView.topEdgeEffect.style = .soft
        collectionView.bottomEdgeEffect.isHidden = true
        makeCellProvider()
        // Dynamic Type changes the prompts' size: measure again.
        registerForTraitChanges([UITraitPreferredContentSizeCategory.self]) { (self: Self, _) in
            self.textHeights = [:]
            self.relayout()
        }

        loadingView.frame = view.bounds
        loadingView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(loadingView)

        composer = StudioComposerView(store: composerStore, browser: browser, service: service)
        composer.onHeightChange = { [weak self] in self?.view.setNeedsLayout() }
        composerStore.onSent = { [weak self] thread in self?.store.adopt(thread) }
        bottom.axis = .vertical
        bottom.alignment = .fill
        bottom.translatesAutoresizingMaskIntoConstraints = false
        status.translatesAutoresizingMaskIntoConstraints = false
        statusRow.addSubview(status)
        // The row is what's hidden (not just the status in it): a hidden
        // arranged subview gives its 24pt back.
        statusRow.isHidden = true
        bottom.addArrangedSubview(statusRow)
        bottom.addArrangedSubview(composer)
        archivedBar.onUnarchive = { [weak self] in
            guard let self, let thread = self.store.thread?.conversation else { return }
            StudioThreadActions.setArchived(false, thread, from: self, browser: self.browser, service: self.service)
        }
        bottom.isHidden = true
        view.addSubview(bottom)
        // The feed fades into the background behind the composer.
        bottomFade.translatesAutoresizingMaskIntoConstraints = false
        view.insertSubview(bottomFade, belowSubview: bottom)
        NSLayoutConstraint.activate([
            // Below required: hidden, the stack pins the row to zero.
            {
                let height = statusRow.heightAnchor.constraint(equalToConstant: 24)
                height.priority = .init(999)
                return height
            }(),
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
                // Over any content's own hugging (the archived bar's label).
                fill.priority = .init(999)
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

    /// The viewer closes back into this thread's tiles while it's on top.
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        transitionSource.provider = self
    }

    func transitionImageView(for artifactId: String) -> UIView? {
        guard let indexPath = indexPath(for: .artifact(artifactId)) else { return nil }
        return (collectionView.cellForItem(at: indexPath) as? StudioArtifactCell)?.transitionView
    }

    func prepareTransitionImageView(for artifactId: String) {
        guard let indexPath = indexPath(for: .artifact(artifactId)),
              collectionView.cellForItem(at: indexPath) == nil else { return }
        collectionView.scrollToItem(at: indexPath, at: .centeredVertically, animated: false)
        collectionView.layoutIfNeeded()
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        guard isMovingFromParent || navigationController?.isBeingDismissed == true else { return }
        // Runs that finished while this was open were seen: no "Done" badge.
        if watchKey != nil, let deviceId = browser.selectedDeviceId {
            let (service, threadId) = (service, threadId)
            Task { await service.markStudioThreadSeen(deviceId: deviceId, threadId: threadId) }
        }
        watchTasks.forEach { $0.cancel() }
        watchTasks = []
        watchKey = nil
        prefetchedIds.forEach { browser.endPreviewRequest(artifactId: $0) }
        prefetchedIds = []
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
        let name = thread?.conversation.title.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        // Names this screen in the back button's menu, too.
        title = thread == nil ? "" : (name.isEmpty ? "Untitled thread" : name)
        navigationItem.subtitle = thread == nil ? nil : deviceId.map { id in
            "Studio @ \(service.hosts.first { $0.id == id }?.name ?? "desktop")"
        }
        menuItem.isHidden = thread == nil

        loadingView.isHidden = !(store.loading && thread == nil)
        if let error = store.error, thread == nil {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "Thread unavailable",
                message: error,
                symbol: "exclamationmark.triangle"
            )
        } else if deviceId == nil {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "No desktop",
                message: "Studio runs on a desktop signed in to this account.",
                symbol: "desktopcomputer"
            )
        } else {
            contentUnavailableConfiguration = nil
        }

        collectionView.isHidden = thread == nil
        bottom.isHidden = thread == nil
        bottomFade.isHidden = thread == nil
        guard let thread else { return }
        let active = thread.turns.flatMap(\.runs).first { $0.state.isCreating }
        // A dropped watch shows over the last thread seen while it reconnects.
        let statusText = store.error != nil ? "Reconnecting…" : active?.state.label
        status.update(label: statusText)
        // An archived thread takes no new turns (the desktop refuses them):
        // it says so, with the way back, in the composer's place.
        let archived = thread.conversation.archived
        if statusRow.isHidden != (statusText == nil) { statusRow.isHidden = statusText == nil }
        // Swapped, not hidden: a hidden arranged subview is pinned to zero
        // height, which the composer's own fixed heights would fight.
        let wanted: UIView = archived ? archivedBar : composer
        if wanted.superview !== bottom {
            if archived { composer.resignFirstResponder() }
            let other: UIView = archived ? composer : archivedBar
            bottom.removeArrangedSubview(other)
            other.removeFromSuperview()
            bottom.addArrangedSubview(wanted)
            view.setNeedsLayout()
        }
        if thread != shown { apply(thread) }
    }

    private func restartWatchIfNeeded(deviceId: String?) {
        let key = "\(deviceId ?? "none")-\(threadId)"
        guard key != watchKey else { return }
        let switched = watchKey != nil
        watchKey = key
        watchTasks.forEach { $0.cancel() }
        watchTasks = []
        if switched {
            store.reset()
            shown = nil
            retrying = []
        }
        guard let deviceId else {
            store.loading = false
            return
        }
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
        // A new turn (this phone's send, or the desktop's) is followed down.
        let newTurn = !first && thread.turns.count > shown?.turns.count ?? 0
        let reveal = first ? revealArtifactId.flatMap { id in
            thread.turns.contains { $0.runs.contains { $0.artifacts.contains { $0.id == id } } } ? Item.artifact(id) : nil
        } : nil
        let pinned = (first && reveal == nil) || newTurn || isAtBottom
        shown = thread
        let (oldTurns, oldRuns, oldArtifacts) = (turns, runs, artifacts)
        turns = [:]
        runs = [:]
        artifacts = [:]
        var newSections: [Section] = []
        var newItems: [[Item]] = []
        for turn in thread.turns.sorted(by: { $0.position < $1.position }) {
            turns[turn.id] = turn
            newSections.append(.prompt(turn.id))
            newItems.append([.prompt(turn.id)])
            for run in turn.runs.sorted(by: { $0.position < $1.position }) {
                runs[run.id] = (run, turn)
                // A retry is done once its run shows creating again.
                if run.state.isCreating { retrying.remove(run.id) }
                newSections.append(.run(run.id))
                newItems.append([.run(run.id)])
                if run.artifacts.isEmpty {
                    // Still being made: a tile for each output asked for.
                    let pending = run.state.isCreating ? Int(min(run.outputCount ?? 1, 8)) : 0
                    guard pending > 0 else { continue }
                    newSections.append(.grid(run.id))
                    newItems.append((0..<pending).map { .pending(run.id, $0) })
                    continue
                }
                let sorted = run.artifacts.sorted { $0.outputPosition < $1.outputPosition }
                for artifact in sorted { artifacts[artifact.id] = (artifact, run, turn) }
                newSections.append(.grid(run.id))
                newItems.append(sorted.map { .artifact($0.id) })
            }
        }
        let (oldSections, oldItems) = (sections, sectionItems)
        let structural = oldSections != newSections || oldItems != newItems
        // Rows keep their identity; a run's state or a prompt can still
        // change. Only rows that did are reconfigured: the stream sends the
        // whole thread on every progress tick.
        var changed: [Item] = []
        if !first {
            for items in newItems {
                for item in items {
                    let differs: Bool = switch item {
                    case .prompt(let id): oldTurns[id] != nil && oldTurns[id]?.prompt != turns[id]?.prompt
                    case .run(let id): oldRuns[id] != nil && oldRuns[id]?.run != runs[id]?.run
                    case .pending: false
                    case .artifact(let id):
                        oldArtifacts[id] != nil && (oldArtifacts[id]?.artifact != artifacts[id]?.artifact
                            || oldArtifacts[id]?.run.model.displayName != artifacts[id]?.run.model.displayName)
                    }
                    if differs { changed.append(item) }
                }
            }
        }
        guard first || structural || !changed.isEmpty else { return }
        // The model the collection view reads changes inside the batch
        // update (UIKit counts the old one before it); the frames are for
        // the new one.
        let commit = { [self] in
            sections = newSections
            sectionItems = newItems
        }
        itemPaths = [:]
        itemPaths.reserveCapacity(newItems.count * 2)
        for (section, items) in newItems.enumerated() {
            for (index, item) in items.enumerated() { itemPaths[item] = IndexPath(item: index, section: section) }
        }
        let oldFrames = layout.frames
        let frames = computeFrames(sections: newSections, items: newItems)

        if first {
            commit()
            layout.set(frames)
            collectionView.reloadData()
        } else if structural {
            // What changes is near the end: a new turn or run, tiles arriving
            // for a run (or a failed one's placeholders going). The sections
            // both share from the top stay (reloaded if their items
            // changed); past them the old ones go and the new ones come in,
            // animated.
            let shared = zip(oldSections, newSections).prefix { $0 == $1 }.count
            let reloaded = IndexSet((0..<shared).filter { oldItems[$0] != newItems[$0] })
            let deleted = IndexSet(shared..<oldSections.count)
            let inserted = IndexSet(shared..<newSections.count)
            collectionView.performBatchUpdates {
                commit()
                layout.set(frames)
                if !reloaded.isEmpty { collectionView.reloadSections(reloaded) }
                if !deleted.isEmpty { collectionView.deleteSections(deleted) }
                if !inserted.isEmpty { collectionView.insertSections(inserted) }
            }
            let rest = changed.compactMap { itemPaths[$0] }.filter { $0.section < shared && !reloaded.contains($0.section) }
            if !rest.isEmpty { collectionView.reconfigureItems(at: rest) }
        } else {
            // A progress tick, a state change: the rows in place. Only a
            // height change (an error line appearing) animates.
            commit()
            let paths = changed.compactMap { itemPaths[$0] }
            if frames != oldFrames {
                collectionView.performBatchUpdates { layout.set(frames) }
            }
            collectionView.reconfigureItems(at: paths)
        }
        if pinned {
            scrollToBottom()
            // Again once UIKit has settled the update (and the composer's
            // inset, which a run finishing changes with the status line).
            DispatchQueue.main.async { [weak self] in self?.scrollToBottom() }
        }
        if let reveal, let path = indexPath(for: reveal) {
            collectionView.layoutIfNeeded()
            collectionView.scrollToItem(at: path, at: .centeredVertically, animated: false)
            // Again once the push has given the list its final insets.
            DispatchQueue.main.async { [weak self] in
                guard let self, self.indexPath(for: reveal) == path else { return }
                self.collectionView.scrollToItem(at: path, at: .centeredVertically, animated: false)
            }
        }
        if first {
            runLaunchSteps()
            // An empty thread has one thing to do: describe what to make.
            if thread.turns.isEmpty {
                DispatchQueue.main.async { [weak self] in
                    guard let self, self.view.window != nil else { return }
                    self.composer.becomeFirstResponder()
                }
            }
        }
    }

    private func item(at indexPath: IndexPath) -> Item? {
        guard sectionItems.indices.contains(indexPath.section),
              sectionItems[indexPath.section].indices.contains(indexPath.item) else { return nil }
        return sectionItems[indexPath.section][indexPath.item]
    }

    private func indexPath(for item: Item) -> IndexPath? { itemPaths[item] }

    /// Launch arguments for screenshots, once per launch: `-studio-thread-top`
    /// (scroll to the first turn), `-studio-thread-failed` (to the first
    /// failed run), `-studio-thread-tile` (open the viewer on the first tile
    /// on screen), `-studio-thread-compose` (focus the composer),
    /// `-studio-thread-send <prompt>` (send it once the composer can).
    private static var launchSteps = Set(["-studio-thread-top", "-studio-thread-failed", "-studio-thread-tile", "-studio-thread-compose", "-studio-thread-send"]
        .filter(ProcessInfo.processInfo.arguments.contains))

    private func runLaunchSteps() {
        guard !Self.launchSteps.isEmpty else { return }
        let steps = Self.launchSteps
        Self.launchSteps = []
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in
            guard let self else { return }
            if steps.contains("-studio-thread-top") {
                self.collectionView.setContentOffset(CGPoint(x: 0, y: -self.collectionView.adjustedContentInset.top), animated: false)
                self.collectionView.layoutIfNeeded()
            }
            if steps.contains("-studio-thread-failed"),
               let failed = self.sectionItems.joined().first(where: {
                   if case .run(let id) = $0 { return self.runs[id]?.run.state == .failed }
                   return false
               }),
               let path = self.indexPath(for: failed) {
                self.collectionView.scrollToItem(at: path, at: .centeredVertically, animated: false)
            }
            if steps.contains("-studio-thread-compose") { self.composer.becomeFirstResponder() }
            let arguments = ProcessInfo.processInfo.arguments
            if steps.contains("-studio-thread-send"),
               let index = arguments.firstIndex(of: "-studio-thread-send"), arguments.indices.contains(index + 1) {
                self.composerStore.prompt = arguments[index + 1]
                self.sendWhenReady(attempts: 20)
            }
            if steps.contains("-studio-thread-tile"),
               let path = self.collectionView.indexPathsForVisibleItems.sorted().first(where: {
                   if case .artifact = self.item(at: $0) { return true }
                   return false
               }) {
                self.collectionView(self.collectionView, didSelectItemAt: path)
            }
        }
    }

    private func sendWhenReady(attempts: Int) {
        guard attempts > 0, let deviceId = browser.selectedDeviceId else { return }
        guard composerStore.canSend else {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in self?.sendWhenReady(attempts: attempts - 1) }
            return
        }
        composerStore.send(workspace: service, deviceId: deviceId)
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

    /// Every height is known, so the bottom is exact on the first try (once
    /// the content size has caught up with the frames).
    private func scrollToBottom() {
        collectionView.layoutIfNeeded()
        let target = maxOffsetY
        guard abs(collectionView.contentOffset.y - target) > 0.5 else { return }
        collectionView.contentOffset.y = target
    }

    // MARK: Layout

    static let tileSpacing: CGFloat = 4
    static let tileRadius: CGFloat = 12

    /// The text size changed: every frame again.
    private func relayout() {
        guard !sections.isEmpty else { return }
        let pinned = isAtBottom
        layout.set(computeFrames())
        if pinned { scrollToBottom() }
    }

    /// Every row's frame, top to bottom. Phones: 16pt gutters. Wider (iPad,
    /// landscape): a centered column as wide as the composer's glass, less
    /// its 4pt. 28pt between turns, then a run's header, then its tiles.
    private func computeFrames() -> [[CGRect]] {
        computeFrames(sections: sections, items: sectionItems)
    }

    private func computeFrames(sections: [Section], items sectionItems: [[Item]]) -> [[CGRect]] {
        let insets = collectionView.safeAreaInsets
        let available = max(0, collectionView.bounds.width - insets.left - insets.right)
        let gutter = max(16, ((available - 760) / 2).rounded(.down))
        let width = max(1, available - gutter * 2)
        let x = insets.left + gutter
        if width != measuredWidth {
            measuredWidth = width
            textHeights = [:]
        }
        var y: CGFloat = 0
        var frames: [[CGRect]] = []
        frames.reserveCapacity(sections.count)
        for (index, section) in sections.enumerated() {
            switch section {
            case .prompt(let id):
                y += index == 0 ? 18 : 28
                let height = promptHeight(turns[id]?.prompt ?? "", width: width)
                frames.append([CGRect(x: x, y: y, width: width, height: height)])
                y += height
            case .run(let id):
                // The 30pt header row centers its text: 16 above it, 13 below.
                y += 9
                let height = runs[id].map { runHeight($0.run, width: width) } ?? StudioRunCell.headHeight
                frames.append([CGRect(x: x, y: y, width: width, height: height)])
                y += height
            case .grid(let runId):
                y += 6
                let count = sectionItems[index].count
                let tile = tileSize(runId: runId, width: width, count: count)
                let columns = count <= 1 ? 1 : 2
                var tiles: [CGRect] = []
                tiles.reserveCapacity(count)
                for i in 0..<count {
                    let (row, column) = (i / columns, i % columns)
                    tiles.append(CGRect(
                        x: x + CGFloat(column) * (tile.width + Self.tileSpacing),
                        y: y + CGFloat(row) * (tile.height + Self.tileSpacing),
                        width: tile.width, height: tile.height
                    ))
                }
                frames.append(tiles)
                let rows = (count + columns - 1) / columns
                y += CGFloat(rows) * tile.height + CGFloat(max(0, rows - 1)) * Self.tileSpacing
            }
        }
        layout.contentHeight = sections.isEmpty ? 0 : y + 12
        return frames
    }

    /// A run's outputs in their own shape (the aspect they were asked for):
    /// one alone as a single tile, at most as tall as it is wide; more two
    /// across, a panorama kept from shrinking into a sliver.
    private func tileSize(runId: String, width: CGFloat, count: Int) -> CGSize {
        let run = runs[runId]?.run
        let aspect = run?.displayAspect ?? run?.artifacts.first?.aspectRatio ?? 1
        if count <= 1 {
            // Portrait narrows rather than towering past the screen.
            let aspect = max(aspect, 0.56)
            let height = (width / max(aspect, 1)).rounded(.down)
            return CGSize(width: min(width, (height * aspect).rounded(.down)), height: height)
        }
        let side = ((width - Self.tileSpacing) / 2).rounded(.down)
        return CGSize(width: side, height: (side / min(max(aspect, 0.5), 2)).rounded(.down))
    }

    private func promptHeight(_ prompt: String, width: CGFloat) -> CGFloat {
        let key = "p" + prompt
        if let height = textHeights[key] { return height }
        let height = StudioPromptCell.height(prompt: prompt, width: width)
        textHeights[key] = height
        return height
    }

    private func runHeight(_ run: StudioRun, width: CGFloat) -> CGFloat {
        guard let message = StudioRunCell.errorMessage(run) else { return StudioRunCell.headHeight }
        let key = "e" + message
        if let height = textHeights[key] { return height }
        let height = StudioRunCell.height(error: message, width: width)
        textHeights[key] = height
        return height
    }

    private func makeCellProvider() {
        let prompts = UICollectionView.CellRegistration<StudioPromptCell, String> { [weak self] cell, _, turnId in
            cell.configure(prompt: self?.turns[turnId]?.prompt ?? "")
        }
        let runCells = UICollectionView.CellRegistration<StudioRunCell, String> { [weak self] cell, _, runId in
            guard let self, let run = self.runs[runId]?.run else { return }
            cell.configure(run, retrying: self.retrying.contains(runId)) { [weak self] in
                self?.retry(runId: runId)
            }
        }
        let artifactCells = UICollectionView.CellRegistration<StudioArtifactCell, String> { [weak self] cell, _, artifactId in
            guard let self, let entry = self.artifacts[artifactId] else { return }
            cell.configure(
                entry.artifact,
                modelName: entry.run.model.displayName,
                prompt: entry.turn.prompt,
                browser: self.browser,
                service: self.service,
                transitionSource: self.transitionSource
            )
        }
        let pendingCells = UICollectionView.CellRegistration<StudioPendingTileCell, String> { [weak self] cell, _, runId in
            cell.configure(video: self?.runs[runId]?.run.model.isVideo ?? false)
        }
        cellProvider = { collectionView, indexPath, item in
            switch item {
            case .prompt(let id): collectionView.dequeueConfiguredReusableCell(using: prompts, for: indexPath, item: id)
            case .run(let id): collectionView.dequeueConfiguredReusableCell(using: runCells, for: indexPath, item: id)
            case .artifact(let id): collectionView.dequeueConfiguredReusableCell(using: artifactCells, for: indexPath, item: id)
            case .pending(let runId, _): collectionView.dequeueConfiguredReusableCell(using: pendingCells, for: indexPath, item: runId)
            }
        }
    }

    func numberOfSections(in collectionView: UICollectionView) -> Int { sectionItems.count }

    func collectionView(_ collectionView: UICollectionView, numberOfItemsInSection section: Int) -> Int {
        sectionItems.indices.contains(section) ? sectionItems[section].count : 0
    }

    func collectionView(_ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath) -> UICollectionViewCell {
        cellProvider(collectionView, indexPath, sectionItems[indexPath.section][indexPath.item])
    }

    // MARK: Prefetching

    /// As the gallery: thumbhashes decoded and previews asked for ahead of
    /// the scroll, so a tile has its picture on its first frame.
    func collectionView(_ collectionView: UICollectionView, prefetchItemsAt indexPaths: [IndexPath]) {
        let upcoming = indexPaths.compactMap { path -> StudioArtifact? in
            guard case .artifact(let id) = item(at: path) else { return nil }
            return artifacts[id]?.artifact
        }
        guard !upcoming.isEmpty else { return }
        let hashes = upcoming.compactMap { artifact in artifact.thumbhash.map { ($0, artifact.aspectRatio) } }
        if !hashes.isEmpty {
            Task.detached(priority: .userInitiated) {
                for (hash, aspect) in hashes { StudioThumbHashCache.shared.decode(hash, aspectRatio: aspect) }
            }
        }
        guard let deviceId = browser.selectedDeviceId else { return }
        for artifact in upcoming where browser.cachedPreview(artifactId: artifact.id) == nil
            && prefetchedIds.insert(artifact.id).inserted {
            _ = browser.beginPreviewRequest(artifactId: artifact.id, deviceId: deviceId, workspace: service, priority: .prefetch)
        }
    }

    func collectionView(_ collectionView: UICollectionView, cancelPrefetchingForItemsAt indexPaths: [IndexPath]) {
        for path in indexPaths {
            if case .artifact(let id) = item(at: path) { endPrefetch(id) }
        }
    }

    func collectionView(_ collectionView: UICollectionView, willDisplay cell: UICollectionViewCell, forItemAt indexPath: IndexPath) {
        // After the tile's own (visible) hold began, so the read isn't dropped.
        if case .artifact(let id) = item(at: indexPath) { endPrefetch(id) }
    }

    private func endPrefetch(_ artifactId: String) {
        guard prefetchedIds.remove(artifactId) != nil else { return }
        browser.endPreviewRequest(artifactId: artifactId)
    }

    // MARK: Interaction

    func collectionView(_ collectionView: UICollectionView, shouldHighlightItemAt indexPath: IndexPath) -> Bool {
        if case .artifact = item(at: indexPath) { return true }
        return false
    }

    /// Touch-down starts the display-sized original, so the viewer's first
    /// frame is often already sharp.
    func collectionView(_ collectionView: UICollectionView, didHighlightItemAt indexPath: IndexPath) {
        guard case .artifact(let id) = item(at: indexPath),
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
        guard case .artifact(let id) = item(at: indexPath),
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
        guard indexPaths.count == 1, let item = item(at: indexPaths[0]) else { return nil }
        switch item {
        case .prompt(let turnId):
            guard let prompt = turns[turnId]?.prompt else { return nil }
            return UIContextMenuConfiguration(actionProvider: { [weak self] _ in
                var actions = [UIAction(title: "Copy Prompt", image: UIImage(systemName: "doc.on.doc")) { _ in
                    UIPasteboard.general.string = prompt
                }]
                if self?.store.thread?.conversation.archived == false {
                    actions.append(UIAction(title: "Edit as New Prompt", image: UIImage(systemName: "square.and.pencil")) { _ in
                        self?.reusePrompt(prompt)
                    })
                }
                return UIMenu(children: actions)
            })
        case .run(let runId):
            guard let run = runs[runId]?.run else { return nil }
            let retryable = (run.state == .failed || run.state == .cancelled) && !retrying.contains(runId)
            let error = run.error.flatMap { $0.isEmpty ? nil : $0 }
            guard retryable || error != nil else { return nil }
            return UIContextMenuConfiguration(actionProvider: { [weak self] _ in
                var actions: [UIAction] = []
                if retryable {
                    actions.append(UIAction(title: "Retry", image: UIImage(systemName: "arrow.clockwise")) { _ in
                        self?.retry(runId: runId)
                    })
                }
                if let error {
                    actions.append(UIAction(title: "Copy Error", image: UIImage(systemName: "doc.on.doc")) { _ in
                        UIPasteboard.general.string = error
                    })
                }
                return UIMenu(children: actions)
            })
        case .artifact(let id):
            guard let entry = artifacts[id] else { return nil }
            let detail = StudioArtifactDetail(artifact: entry.artifact, turn: entry.turn, run: entry.run, conversationId: threadId)
            return UIContextMenuConfiguration(identifier: id as NSString, previewProvider: nil) { [weak self] _ in
                UIMenu(children: [
                    UIAction(title: "Save to Photos", image: UIImage(systemName: "square.and.arrow.down")) { _ in
                        self?.download(detail)
                    },
                    UIAction(title: "Copy Prompt", image: UIImage(systemName: "doc.on.doc")) { _ in
                        UIPasteboard.general.string = detail.prompt
                    },
                    UIMenu(options: .displayInline, children: [
                        UIAction(title: "Delete", image: UIImage(systemName: "trash"), attributes: .destructive) { _ in
                            self?.confirmDelete(detail)
                        },
                    ]),
                ])
            }
        case .pending:
            return nil
        }
    }

    /// The tile itself lifts out of the grid, its rounded shape intact.
    func collectionView(_ collectionView: UICollectionView, contextMenuConfiguration configuration: UIContextMenuConfiguration, highlightPreviewForItemAt indexPath: IndexPath) -> UITargetedPreview? {
        guard case .artifact = item(at: indexPath),
              let cell = collectionView.cellForItem(at: indexPath) else { return nil }
        let parameters = UIPreviewParameters()
        parameters.visiblePath = UIBezierPath(roundedRect: cell.contentView.bounds, cornerRadius: Self.tileRadius)
        return UITargetedPreview(view: cell.contentView, parameters: parameters)
    }

    /// A past prompt back in the composer, to change and send again.
    private func reusePrompt(_ prompt: String) {
        composerStore.prompt = prompt
        composer.becomeFirstResponder()
    }

    // MARK: Artifact actions

    /// Into Photos, saying so: an original (a video especially) can take a
    /// while to come over the relay.
    private func download(_ artifact: StudioArtifactDetail) {
        guard let deviceId = browser.selectedDeviceId else { return }
        Toast.show(artifact.mediaKind == .video ? "Saving video to Photos…" : "Saving to Photos…", in: view.window)
        let service = service
        Task { [weak self] in
            do {
                try await StudioArtifactActions.download(artifact, workspace: service, deviceId: deviceId)
                Toast.show("Saved to Photos", in: self?.view.window)
            } catch is CancellationError {
            } catch {
                self?.showActionFailure(error.localizedDescription)
            }
        }
    }

    private func confirmDelete(_ artifact: StudioArtifactDetail) {
        let desktop = browser.selectedDeviceId.flatMap { id in service.hosts.first { $0.id == id }?.name }
        let alert = UIAlertController(
            title: artifact.mediaKind == .video ? "Delete this video?" : "Delete this image?",
            message: "This removes it from Studio on \(desktop ?? "the desktop"). It can't be undone.",
            preferredStyle: .alert
        )
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak self] _ in
            guard let self, let deviceId = self.browser.selectedDeviceId else { return }
            let (service, browser) = (self.service, self.browser)
            Task { [weak self] in
                do {
                    try await service.deleteStudioArtifact(deviceId: deviceId, artifactId: artifact.id)
                    // The gallery drops it now; the thread's watch follows.
                    browser.removeArtifact(artifact.id)
                } catch {
                    self?.showActionFailure(error.localizedDescription)
                }
            }
        })
        presentOnTop(alert)
    }

    private func showActionFailure(_ message: String) {
        let alert = UIAlertController(title: "Studio action failed", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        presentOnTop(alert)
    }

    // MARK: Thread menu

    private func threadMenu() -> [UIMenuElement] {
        guard let thread = store.thread?.conversation else { return [] }
        return StudioThreadActions.menuElements(for: thread, from: self, browser: browser, service: service) { [weak self] in
            // Gone from the desktop: nothing left to show here.
            guard let self, let nav = self.navigationController else { return }
            if nav.topViewController === self { nav.popViewController(animated: true) }
        }
    }

    // MARK: Retry

    /// Queues a failed or cancelled run again. The host refuses an uncertain
    /// submission (interrupted mid-send: the provider may have the work)
    /// until it's confirmed, since that can be charged twice.
    private func retry(runId: String, anyway: Bool = false) {
        guard !retrying.contains(runId), let deviceId = browser.selectedDeviceId else { return }
        setRetrying(runId, true)
        let service = service
        Task { [weak self] in
            do {
                let thread = try await service.retryStudioRun(deviceId: deviceId, runId: runId, retryAnyway: anyway)
                guard let self else { return }
                self.store.adoptRetry(thread, runId: runId)
                // Otherwise the row holds its spinner until the run shows
                // creating (`apply`): no flash of Retry in between.
                if self.store.thread?.turns.flatMap(\.runs).first(where: { $0.id == runId })?.state.isCreating != true {
                    self.setRetrying(runId, false)
                }
            } catch {
                guard let self else { return }
                self.setRetrying(runId, false)
                let message = error.localizedDescription
                if !anyway, message.contains("explicit confirmation is required") {
                    self.confirmUncertainRetry(runId: runId)
                } else {
                    self.showRetryFailure(message)
                }
            }
        }
    }

    private func setRetrying(_ runId: String, _ on: Bool) {
        guard (on ? retrying.insert(runId).inserted : retrying.remove(runId) != nil) else { return }
        guard let path = indexPath(for: .run(runId)) else { return }
        collectionView.reconfigureItems(at: [path])
    }

    private func confirmUncertainRetry(runId: String) {
        let alert = UIAlertController(
            title: "Retry anyway?",
            message: "This run was interrupted while it was being sent, so the provider may already have it. Retrying could run it, and charge for it, twice.",
            preferredStyle: .alert
        )
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Retry Anyway", style: .destructive) { [weak self] _ in
            self?.retry(runId: runId, anyway: true)
        })
        presentOnTop(alert)
    }

    private func showRetryFailure(_ message: String) {
        let alert = UIAlertController(title: "Couldn't retry", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        presentOnTop(alert)
    }

    /// Over whatever is up (the viewer, a composer sheet); not at all once
    /// this screen is gone.
    private func presentOnTop(_ controller: UIViewController) {
        guard viewIfLoaded?.window != nil || presentedViewController != nil else { return }
        var host: UIViewController = self
        while let presented = host.presentedViewController { host = presented }
        host.present(controller, animated: true)
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

/// In the composer's place on an archived thread: it takes no new turns.
private final class StudioArchivedBar: UIView {
    var onUnarchive: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        let glass = Glass.surface(interactive: false, radius: 25)
        glass.translatesAutoresizingMaskIntoConstraints = false
        addSubview(glass)
        let glyph = UIImageView(image: UIImage(systemName: "archivebox", withConfiguration: UIImage.SymbolConfiguration(pointSize: 14, weight: .medium)))
        glyph.tintColor = Palette.secondary
        glyph.setContentHuggingPriority(.required, for: .horizontal)
        let label = UILabel()
        label.text = "This thread is archived."
        label.font = UIFontMetrics(forTextStyle: .subheadline).scaledFont(for: Fonts.ui(.sans, 14))
        label.adjustsFontForContentSizeCategory = true
        label.textColor = Palette.secondary
        label.numberOfLines = 0
        var config = UIButton.Configuration.filled()
        config.title = "Unarchive"
        config.baseForegroundColor = Palette.text
        config.baseBackgroundColor = Palette.controlFill
        config.cornerStyle = .capsule
        config.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 14, bottom: 8, trailing: 14)
        config.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attrs in
            var a = attrs
            a.font = Fonts.ui(.sansMedium, 13)
            return a
        }
        let button = UIButton(configuration: config, primaryAction: UIAction { [weak self] _ in self?.onUnarchive?() })
        button.accessibilityIdentifier = "studio-thread-unarchive"
        button.setContentHuggingPriority(.required, for: .horizontal)
        button.setContentCompressionResistancePriority(.required, for: .horizontal)
        let row = UIStackView(arrangedSubviews: [glyph, label, button])
        row.spacing = 10
        row.alignment = .center
        row.translatesAutoresizingMaskIntoConstraints = false
        glass.contentView.addSubview(row)
        NSLayoutConstraint.activate([
            glass.topAnchor.constraint(equalTo: topAnchor),
            glass.bottomAnchor.constraint(equalTo: bottomAnchor),
            glass.leadingAnchor.constraint(equalTo: leadingAnchor),
            glass.trailingAnchor.constraint(equalTo: trailingAnchor),
            glass.heightAnchor.constraint(greaterThanOrEqualToConstant: 50),
            row.leadingAnchor.constraint(equalTo: glass.contentView.leadingAnchor, constant: 18),
            row.trailingAnchor.constraint(equalTo: glass.contentView.trailingAnchor, constant: -8),
            row.topAnchor.constraint(equalTo: glass.contentView.topAnchor, constant: 8),
            row.bottomAnchor.constraint(equalTo: glass.contentView.bottomAnchor, constant: -8),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }
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
        isAccessibilityElement = true
        accessibilityTraits = .updatesFrequently
    }

    override var accessibilityLabel: String? {
        get { label.text }
        set {}
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
        label.font = Self.font()
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

    /// Scales with the text size, like the composer it was typed in.
    static func font() -> UIFont {
        Fonts.ui(.sans, UIFontMetrics(forTextStyle: .body).scaledValue(for: 16))
    }

    /// The bubble's height at `width` (the bubble is at most 80% of it).
    static func height(prompt: String, width: CGFloat) -> CGFloat {
        let textWidth = max(1, (width * 0.8).rounded(.down) - 32)
        return StudioThreadLayout.textHeight(prompt, font: font(), width: textWidth) + 20
    }

    func configure(prompt: String) {
        let font = Self.font()
        if label.font != font { label.font = font }
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
    private let retryButton = UIButton(type: .system)
    private var onRetry: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        model.font = Fonts.ui(.sansMedium, 12)
        model.textColor = Palette.secondary
        model.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        spinner.transform = CGAffineTransform(scaleX: 0.6, y: 0.6)
        spinner.color = Palette.color(.textFaint)
        progress.progressTintColor = Palette.color(.textFaint)
        progress.widthAnchor.constraint(equalToConstant: 46).isActive = true
        var retry = UIButton.Configuration.filled()
        retry.title = "Retry"
        retry.image = UIImage(systemName: "arrow.clockwise", withConfiguration: UIImage.SymbolConfiguration(pointSize: 10, weight: .semibold))
        retry.imagePadding = 4
        retry.baseForegroundColor = Palette.text
        retry.baseBackgroundColor = Palette.controlFill
        retry.cornerStyle = .capsule
        retry.contentInsets = NSDirectionalEdgeInsets(top: 7, leading: 12, bottom: 7, trailing: 13)
        retry.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attrs in
            var a = attrs
            a.font = Fonts.ui(.sansMedium, 11.5)
            return a
        }
        retryButton.configuration = retry
        retryButton.addAction(UIAction { [weak self] _ in self?.onRetry?() }, for: .touchUpInside)
        let status = UIStackView(arrangedSubviews: [progress, spinner, state, retryButton])
        status.spacing = 7
        status.alignment = .center
        status.setContentHuggingPriority(.required, for: .horizontal)
        let head = UIStackView(arrangedSubviews: [model, UIView(), status])
        head.spacing = 8
        head.alignment = .center
        head.heightAnchor.constraint(equalToConstant: Self.headHeight).isActive = true

        let warning = UIImageView(image: UIImage(systemName: "exclamationmark.triangle", withConfiguration: UIImage.SymbolConfiguration(pointSize: 11, weight: .medium)))
        warning.tintColor = Palette.color(.dangerSoft)
        warning.contentMode = .left
        warning.widthAnchor.constraint(equalToConstant: Self.warningWidth).isActive = true
        error.font = Self.errorFont
        error.textColor = Palette.color(.dangerSoft)
        error.numberOfLines = 0
        errorRow.addArrangedSubview(warning)
        errorRow.addArrangedSubview(error)
        errorRow.spacing = 6
        errorRow.alignment = .firstBaseline

        let stack = UIStackView(arrangedSubviews: [head, errorRow])
        stack.axis = .vertical
        stack.spacing = Self.errorSpacing
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(stack)
        // The thread computes this cell's height (`height(error:width:)`):
        // the stack hangs from the top.
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: contentView.topAnchor),
            stack.leadingAnchor.constraint(equalTo: contentView.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: contentView.trailingAnchor),
        ])
        isAccessibilityElement = true
    }

    required init?(coder: NSCoder) { fatalError() }

    static let headHeight: CGFloat = 30
    private static let errorSpacing: CGFloat = 8
    private static let warningWidth: CGFloat = 17
    private static let errorFont = Fonts.ui(.sans, 12)

    /// The provider's error, shown when the run made nothing.
    static func errorMessage(_ run: StudioRun) -> String? {
        run.artifacts.isEmpty ? run.error.flatMap { $0.isEmpty ? nil : $0 } : nil
    }

    static func height(error: String, width: CGFloat) -> CGFloat {
        headHeight + errorSpacing
            + StudioThreadLayout.textHeight(error, font: errorFont, width: max(1, width - warningWidth - 6))
    }

    /// `retrying`: a retry is on its way; the row spins in its place.
    func configure(_ run: StudioRun, retrying: Bool, onRetry: @escaping () -> Void) {
        self.onRetry = onRetry
        model.text = run.model.displayName
        state.text = run.state.label
        let retryable = run.state == .failed || run.state == .cancelled
        retryButton.isHidden = !retryable || retrying
        if retrying {
            state.font = Fonts.ui(.sans, 11)
            state.textColor = Palette.color(.textFaint)
            state.text = "Retrying"
            progress.isHidden = true
            spinner.isHidden = false
            spinner.startAnimating()
        } else if run.state.isCreating {
            state.font = Fonts.ui(.sans, 11)
            state.textColor = Palette.color(.textFaint)
            progress.isHidden = run.progress == nil
            progress.progress = run.progress ?? 0
            spinner.isHidden = run.progress != nil
            if run.progress == nil { spinner.startAnimating() } else { spinner.stopAnimating() }
        } else {
            state.font = Fonts.ui(.sansMedium, 11)
            state.textColor = switch run.state {
            case .succeeded: Palette.success
            case .failed: Palette.color(.dangerSoft)
            default: Palette.color(.textFaint)
            }
            progress.isHidden = true
            spinner.stopAnimating()
            spinner.isHidden = true
        }
        let message = Self.errorMessage(run)
        error.text = message
        errorRow.isHidden = message == nil
        accessibilityLabel = [run.model.displayName, state.text, message].compactMap(\.self).joined(separator: ", ")
        // The row reads as one element, so its button is an action on it.
        accessibilityCustomActions = retryable && !retrying
            ? [UIAccessibilityCustomAction(name: "Retry") { [weak self] _ in
                self?.onRetry?()
                return true
            }]
            : nil
        accessibilityValue = run.state.isCreating ? run.progress.map { "\(Int(($0 * 100).rounded())) percent" } : nil
    }
}

/// One output tile in its own shape, rounded; videos carry a duration
/// badge, as in the gallery. A press dims it.
private final class StudioArtifactCell: UICollectionViewCell {
    private var preview: StudioMediaPreviewView?
    var transitionView: UIView? { preview?.transitionView }
    private let highlight = UIView()
    private let videoBadge = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterialDark))
    private let videoLabel = UILabel()

    override var isHighlighted: Bool {
        didSet { highlight.isHidden = !isHighlighted }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = Palette.chip
        // The viewer's zoom lands into this radius (it reads the ancestors').
        contentView.layer.cornerRadius = StudioThreadViewController.tileRadius
        contentView.layer.cornerCurve = .continuous
        contentView.clipsToBounds = true
        highlight.frame = contentView.bounds
        highlight.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        highlight.backgroundColor = UIColor.black.withAlphaComponent(0.25)
        highlight.isHidden = true
        contentView.addSubview(highlight)
        videoBadge.layer.cornerRadius = 10
        videoBadge.clipsToBounds = true
        videoLabel.font = Fonts.ui(.sansSemibold, 10)
        videoLabel.textColor = .white
        videoLabel.textAlignment = .center
        videoBadge.contentView.addSubview(videoLabel)
        videoBadge.isHidden = true
        contentView.addSubview(videoBadge)
        isAccessibilityElement = true
        accessibilityTraits = [.button, .image]
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layoutSubviews() {
        super.layoutSubviews()
        guard !videoBadge.isHidden else { return }
        let fit = videoLabel.sizeThatFits(CGSize(width: 90, height: 22))
        let width = max(34, ceil(fit.width) + 14)
        videoBadge.frame = CGRect(x: contentView.bounds.width - width - 7, y: contentView.bounds.height - 27, width: width, height: 20)
        videoLabel.frame = videoBadge.bounds
    }

    /// What the preview was last configured with.
    private var shownId: String?
    private var shownThumbhash: String?

    override func prepareForReuse() {
        super.prepareForReuse()
        preview?.reset()
        shownId = nil
    }

    func configure(
        _ artifact: StudioArtifact,
        modelName: String,
        prompt: String,
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
        // A fresh artifact shows before its preview is derived; the preview
        // view only loads once per artifact, so its thumbhash arriving (the
        // preview now exists) restarts the load.
        if shownId == artifact.id, shownThumbhash != artifact.thumbhash {
            preview.reset()
        }
        shownId = artifact.id
        shownThumbhash = artifact.thumbhash
        preview.configure(
            artifactId: artifact.id,
            mediaKind: artifact.mediaKind,
            thumbhash: artifact.thumbhash,
            aspectRatio: artifact.aspectRatio,
            contentMode: .scaleAspectFill,
            transitionSource: transitionSource
        )
        let video = artifact.mediaKind == .video
        let duration = video ? StudioGalleryCell.duration(artifact.durationSeconds) : nil
        videoBadge.isHidden = !video
        if videoLabel.text != duration {
            videoLabel.text = duration
            setNeedsLayout()
        }
        accessibilityLabel = [video ? "Video" : "Image", duration, modelName, prompt]
            .compactMap { $0?.isEmpty == false ? $0 : nil }.joined(separator: ", ")
    }
}

/// An output still being made: its tile in place, softly pulsing.
private final class StudioPendingTileCell: UICollectionViewCell {
    private let glyph = UIImageView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = Palette.chip
        contentView.layer.cornerRadius = StudioThreadViewController.tileRadius
        contentView.layer.cornerCurve = .continuous
        contentView.clipsToBounds = true
        glyph.tintColor = Palette.color(.textFaint).withAlphaComponent(0.6)
        glyph.preferredSymbolConfiguration = UIImage.SymbolConfiguration(pointSize: 20, weight: .light)
        glyph.contentMode = .center
        glyph.frame = contentView.bounds
        glyph.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        contentView.addSubview(glyph)
        // The run's header reads its state; the tiles are decoration.
        isAccessibilityElement = false
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(video: Bool) {
        glyph.image = UIImage(systemName: video ? "film" : "photo")
        pulse()
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        pulse()
    }

    private func pulse() {
        contentView.layer.removeAnimation(forKey: "pulse")
        guard window != nil, !UIAccessibility.isReduceMotionEnabled else { return }
        let animation = CABasicAnimation(keyPath: "opacity")
        animation.fromValue = 1
        animation.toValue = 0.55
        animation.duration = 0.9
        animation.autoreverses = true
        animation.repeatCount = .infinity
        animation.isRemovedOnCompletion = false
        animation.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        contentView.layer.add(animation, forKey: "pulse")
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

/// The thread's frames, computed by its controller (`computeFrames`) and
/// handed over whole; this only answers UIKit's queries: a binary search
/// for what's in a rect, so a thousand tiles cost what a screenful does.
final class StudioThreadLayout: UICollectionViewLayout {
    private(set) var frames: [[CGRect]] = []
    var contentHeight: CGFloat = 0
    /// Frames for a new width (asked from `prepare`).
    var frameProvider: (() -> [[CGRect]])?
    private var width: CGFloat = 0
    /// Every item in order; their bottoms never decrease.
    private var flat: [(path: IndexPath, frame: CGRect)] = []

    func set(_ frames: [[CGRect]]) {
        store(frames)
        invalidateLayout()
    }

    private func store(_ frames: [[CGRect]]) {
        self.frames = frames
        flat = []
        flat.reserveCapacity(frames.reduce(0) { $0 + $1.count })
        for (section, items) in frames.enumerated() {
            for (item, frame) in items.enumerated() {
                flat.append((IndexPath(item: item, section: section), frame))
            }
        }
    }

    /// Text in a label of `width`: what UILabel will need, rounded up.
    static func textHeight(_ text: String, font: UIFont, width: CGFloat) -> CGFloat {
        let rect = (text as NSString).boundingRect(
            with: CGSize(width: width, height: .greatestFiniteMagnitude),
            options: [.usesLineFragmentOrigin, .usesFontLeading],
            attributes: [.font: font],
            context: nil
        )
        return ceil(max(rect.height, font.lineHeight))
    }

    override func prepare() {
        super.prepare()
        guard let collectionView, collectionView.bounds.width != width else { return }
        width = collectionView.bounds.width
        if let frames = frameProvider?(), !frames.isEmpty { store(frames) }
    }

    override var collectionViewContentSize: CGSize {
        CGSize(width: collectionView?.bounds.width ?? 0, height: contentHeight)
    }

    override func layoutAttributesForElements(in rect: CGRect) -> [UICollectionViewLayoutAttributes]? {
        // The first item reaching into the rect, by its bottom.
        var low = 0
        var high = flat.count
        while low < high {
            let mid = (low + high) / 2
            if flat[mid].frame.maxY < rect.minY { low = mid + 1 } else { high = mid }
        }
        var result: [UICollectionViewLayoutAttributes] = []
        var index = low
        while index < flat.count, flat[index].frame.minY <= rect.maxY {
            if flat[index].frame.intersects(rect) { result.append(attributes(flat[index].path, flat[index].frame)) }
            index += 1
        }
        return result
    }

    override func layoutAttributesForItem(at indexPath: IndexPath) -> UICollectionViewLayoutAttributes? {
        guard frames.indices.contains(indexPath.section),
              frames[indexPath.section].indices.contains(indexPath.item) else { return nil }
        return attributes(indexPath, frames[indexPath.section][indexPath.item])
    }

    private func attributes(_ path: IndexPath, _ frame: CGRect) -> UICollectionViewLayoutAttributes {
        let attributes = UICollectionViewLayoutAttributes(forCellWith: path)
        attributes.frame = frame
        return attributes
    }

    override func shouldInvalidateLayout(forBoundsChange newBounds: CGRect) -> Bool {
        newBounds.width != collectionView?.bounds.width
    }
}
