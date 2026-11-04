import AVKit
import Photos
import UIKit

/// One page of a thread's lightbox: a picture or video somewhere in the
/// transcript (`TranscriptMedia`), keyed by the reference its row loads.
struct ThreadMediaItem: Equatable {
    let id: String
    let isVideo: Bool
    /// A shared or sent file; nil for an inline attachment (loads by `id`).
    let file: SharedFile?

    init(_ media: TranscriptMedia) {
        id = media.reference
        isVideo = media.kind == .video
        file = media.file.map {
            SharedFile(name: $0.name, mimeType: $0.mimeType, path: media.reference, transferId: $0.transferId, size: $0.size)
        }
    }

    static func == (a: Self, b: Self) -> Bool { a.id == b.id }
}

/// Where the lightbox gets its media from (the chat's session source).
struct ThreadMediaLoader {
    /// An attachment's decoded picture (cached by the source).
    let image: (String) async -> UIImage?
    /// Load an attachment's picture into a thumbnail (the transcript's own
    /// loader; it claims the view so a reused cell never shows a stale one).
    let thumbnail: (String, UIImageView) -> Void
    /// A shared or sent file: a stream URL (`streaming`) or the file on disk.
    let fileURL: (SharedFile, _ streaming: Bool) async -> URL?
}

/// The lightbox's media: pictures for the pages around the selection, a
/// player for the selected video, posters for videos that can stream.
@MainActor
final class ThreadMediaStore {
    var changed: ((String) -> Void)?

    private let loader: ThreadMediaLoader
    private var images: [String: UIImage] = [:]
    private var tasks: [String: Task<Void, Never>] = [:]
    private var failed: Set<String> = []
    private var players: [String: AVPlayer] = [:]
    private var playerTasks: [String: Task<Void, Never>] = [:]
    private var retained: Set<String> = []
    private var selectedId: String?

    init(loader: ThreadMediaLoader, opening: (id: String, image: UIImage)?) {
        self.loader = loader
        if let opening { images[opening.id] = opening.image }
    }

    func image(for id: String) -> UIImage? { images[id] }
    func player(for id: String) -> AVPlayer? { players[id] }

    func state(for item: ThreadMediaItem) -> StudioViewerOriginalState {
        if failed.contains(item.id) { return .failed }
        if item.isVideo { return players[item.id] == nil && playerTasks[item.id] != nil ? .loading : .ready }
        return images[item.id] == nil && tasks[item.id] != nil ? .loading : .ready
    }

    func prepare(selected index: Int, in items: [ThreadMediaItem]) {
        guard items.indices.contains(index) else { return }
        let selected = items[index]
        selectedId = selected.id
        let neighborhood = [0, -1, 1, -2, 2].compactMap { items.indices.contains(index + $0) ? items[index + $0] : nil }
        retained = Set(neighborhood.map(\.id))
        trim()
        for (id, player) in players where id != selected.id { player.pause() }
        for item in neighborhood { load(item) }
        if selected.isVideo { loadPlayer(selected) }
    }

    func retry(_ item: ThreadMediaItem) {
        guard failed.remove(item.id) != nil else { return }
        load(item)
        if item.isVideo, item.id == selectedId { loadPlayer(item) }
        changed?(item.id)
    }

    func pausePlayback() { players.values.forEach { $0.pause() } }

    func resumeSelected() {
        guard let selectedId, let player = players[selectedId] else { return }
        player.play()
    }

    func reset() {
        tasks.values.forEach { $0.cancel() }
        playerTasks.values.forEach { $0.cancel() }
        tasks.removeAll()
        playerTasks.removeAll()
        players.values.forEach { $0.pause() }
        players.removeAll()
        images.removeAll()
        StudioViewerAudioSession.deactivate()
    }

    /// The picture (or a video's poster). Inline attachments decode from the
    /// source's cache; a file is fetched first, and a video's poster only
    /// when it can stream (a host-only video would download in full).
    private func load(_ item: ThreadMediaItem) {
        guard images[item.id] == nil, tasks[item.id] == nil, !failed.contains(item.id) else { return }
        let id = item.id
        let loader = loader
        if item.isVideo, item.file?.transferId == nil { return }
        tasks[id] = Task { [weak self] in
            let image: UIImage?
            if let file = item.file {
                if let url = await loader.fileURL(file, item.isVideo) {
                    image = item.isVideo ? await Self.poster(url) : await Self.decode(url)
                } else {
                    image = nil
                }
            } else {
                image = await loader.image(id)
            }
            guard let self, !Task.isCancelled else { return }
            tasks[id] = nil
            if let image {
                images[id] = image
            } else if !item.isVideo {
                failed.insert(id)
            }
            changed?(id)
        }
        changed?(id)
    }

    private func loadPlayer(_ item: ThreadMediaItem) {
        StudioViewerAudioSession.activate()
        if let player = players[item.id] {
            player.play()
            return
        }
        guard let file = item.file, playerTasks[item.id] == nil else { return }
        let id = item.id
        let loader = loader
        playerTasks[id] = Task { [weak self] in
            let url = await loader.fileURL(file, true)
            guard let self, !Task.isCancelled else { return }
            playerTasks[id] = nil
            guard let url else {
                failed.insert(id)
                changed?(id)
                return
            }
            let player = AVPlayer(url: url)
            players[id] = player
            if selectedId == id { player.play() }
            changed?(id)
            // The first frame stands in for the page while it buffers, and
            // for the filmstrip's thumbnail.
            if images[id] == nil, let poster = await Self.poster(url), !Task.isCancelled {
                images[id] = poster
                changed?(id)
            }
        }
        changed?(id)
    }

    private func trim() {
        for id in tasks.keys where !retained.contains(id) { tasks.removeValue(forKey: id)?.cancel() }
        for id in images.keys where !retained.contains(id) { images.removeValue(forKey: id) }
        for id in playerTasks.keys where id != selectedId { playerTasks.removeValue(forKey: id)?.cancel() }
        for id in players.keys where id != selectedId {
            players[id]?.pause()
            players.removeValue(forKey: id)
        }
        failed.formIntersection(retained)
    }

    nonisolated private static func decode(_ url: URL) async -> UIImage? {
        await Task.detached(priority: .userInitiated) {
            UIImage(contentsOfFile: url.path)?.preparingForDisplay()
        }.value
    }

    nonisolated private static func poster(_ url: URL) async -> UIImage? {
        let generator = AVAssetImageGenerator(asset: AVURLAsset(url: url))
        generator.appliesPreferredTrackTransform = true
        generator.maximumSize = CGSize(width: 1200, height: 1200)
        guard let image = try? await generator.image(at: .zero).image else { return nil }
        return UIImage(cgImage: image)
    }
}

/// A view that only takes touches meant for its subviews.
private final class ThreadMediaPassthroughView: UIView {
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        let hit = super.hitTest(point, with: event)
        return hit === self ? nil : hit
    }
}

/// A thread's lightbox: every picture and video in the transcript as one
/// gallery, paged sideways with a filmstrip under it — Studio's viewer,
/// minus the prompt card and deletion. It zooms out of the tapped picture
/// and back into whichever one is showing on close (a fade when that's
/// scrolled away), and a downward drag closes it.
///
/// Gestures: the dismiss pan takes a mostly-downward drag at minimum zoom;
/// the pager's paging pan waits for it to fail. Pages own pinch, double-tap
/// and panning while zoomed.
@MainActor
final class ThreadMediaViewerController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate,
    UIGestureRecognizerDelegate,
    UIScrollViewAccessibilityDelegate,
    UIContextMenuInteractionDelegate,
    MediaViewerTransitionHost
{
    var transitionController: StudioViewerTransitionController?

    private let items: [ThreadMediaItem]
    private var selectedIndex: Int
    private let loader: ThreadMediaLoader
    private let media: ThreadMediaStore
    /// The transition holds its source weakly; the viewer keeps it.
    private let transitionSource: StudioGalleryTransitionSource

    private let backdrop = UIView()
    private let pagerLayout = UICollectionViewFlowLayout()
    private lazy var pager = UICollectionView(frame: .zero, collectionViewLayout: pagerLayout)
    private let stripLayout = StudioCenteredFilmstripLayout()
    private lazy var strip = UICollectionView(frame: .zero, collectionViewLayout: stripLayout)
    private let topChrome = ThreadMediaPassthroughView()
    private let bottomChrome = ThreadMediaPassthroughView()
    private let closeButton = UIButton(type: .system)
    private let moreButton = UIButton(type: .system)
    private let shareButton = UIButton(type: .system)
    private let saveButton = UIButton(type: .system)
    private let counter = UILabel()
    private lazy var dismissPan = UIPanGestureRecognizer(target: self, action: #selector(handleDismissPan(_:)))

    private var chromeHidden = false
    private var transitionChrome: CGFloat = 1
    private var isDismissing = false
    private var activeDismissal: StudioViewerInteractiveDismissal?
    private var lastPageSize: CGSize = .zero
    private var lastStripWidth: CGFloat = 0
    private var stripDriving = false
    private var busy = false
    private var prepareTask: Task<Void, Never>?

    private static let topChromeHeight: CGFloat = 60
    private static let stripHeight: CGFloat = 58
    private static let buttonSide: CGFloat = 44

    /// `opening` is the tapped thumbnail's picture, so the zoom starts from
    /// what's already on screen.
    init(
        items: [ThreadMediaItem],
        selectedId: String,
        opening: UIImage?,
        loader: ThreadMediaLoader,
        transitionSource: StudioGalleryTransitionSource
    ) {
        self.items = items
        selectedIndex = items.firstIndex(where: { $0.id == selectedId }) ?? 0
        self.loader = loader
        self.transitionSource = transitionSource
        media = ThreadMediaStore(loader: loader, opening: opening.map { (selectedId, $0) })
        super.init(nibName: nil, bundle: nil)
        modalPresentationCapturesStatusBarAppearance = true
        overrideUserInterfaceStyle = .dark
        StudioViewerNativeTransition.configure(self, source: transitionSource)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override var modalPresentationStyle: UIModalPresentationStyle {
        get { transitionController == nil ? super.modalPresentationStyle : .custom }
        set { super.modalPresentationStyle = newValue }
    }

    override var prefersStatusBarHidden: Bool { chromeHidden }
    override var preferredStatusBarStyle: UIStatusBarStyle { .lightContent }
    override var preferredStatusBarUpdateAnimation: UIStatusBarAnimation { .fade }
    override var prefersHomeIndicatorAutoHidden: Bool { chromeHidden }

    private var selected: ThreadMediaItem { items[selectedIndex] }
    private var hasStrip: Bool { items.count > 1 }
    private var bottomChromeHeight: CGFloat { (hasStrip ? Self.stripHeight + 8 : 0) + Self.buttonSide + 8 }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        backdrop.backgroundColor = .black
        view.addSubview(backdrop)

        pagerLayout.scrollDirection = .horizontal
        pagerLayout.minimumLineSpacing = 0
        pagerLayout.minimumInteritemSpacing = 0
        pager.backgroundColor = .clear
        pager.isPagingEnabled = true
        pager.isDirectionalLockEnabled = true
        pager.showsHorizontalScrollIndicator = false
        pager.contentInsetAdjustmentBehavior = .never
        pager.decelerationRate = .fast
        pager.dataSource = self
        pager.delegate = self
        pager.register(StudioViewerPageCell.self, forCellWithReuseIdentifier: StudioViewerPageCell.reuseIdentifier)
        pager.accessibilityIdentifier = "thread-media-pager"
        pager.addInteraction(UIContextMenuInteraction(delegate: self))
        view.addSubview(pager)

        stripLayout.scrollDirection = .horizontal
        stripLayout.itemSize = CGSize(width: 48, height: 48)
        stripLayout.minimumLineSpacing = 6
        strip.backgroundColor = .clear
        strip.showsHorizontalScrollIndicator = false
        strip.contentInsetAdjustmentBehavior = .never
        strip.dataSource = self
        strip.delegate = self
        // The pager is how VoiceOver moves between pages.
        strip.accessibilityElementsHidden = true
        strip.register(ThreadFilmstripCell.self, forCellWithReuseIdentifier: ThreadFilmstripCell.reuseIdentifier)
        strip.isHidden = !hasStrip
        bottomChrome.addSubview(strip)

        configureChrome()
        view.addSubview(topChrome)
        view.addSubview(bottomChrome)

        dismissPan.delegate = self
        dismissPan.maximumNumberOfTouches = 1
        view.addGestureRecognizer(dismissPan)
        pager.panGestureRecognizer.require(toFail: dismissPan)

        media.changed = { [weak self] id in self?.refresh(id) }
        media.prepare(selected: selectedIndex, in: items)
        updateCounter()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        let bounds = view.bounds
        let safe = view.safeAreaInsets
        backdrop.frame = bounds
        pager.frame = bounds
        if bounds.size != lastPageSize {
            lastPageSize = bounds.size
            pagerLayout.itemSize = bounds.size
            pagerLayout.invalidateLayout()
            pager.layoutIfNeeded()
            pager.scrollToItem(at: IndexPath(item: selectedIndex, section: 0), at: .centeredHorizontally, animated: false)
        }

        let side = Self.buttonSide
        let leading = safe.left + 16
        let trailing = bounds.width - safe.right - 16 - side
        topChrome.frame = CGRect(x: 0, y: 0, width: bounds.width, height: safe.top + Self.topChromeHeight)
        closeButton.frame = CGRect(x: leading, y: safe.top + 8, width: side, height: side)
        moreButton.frame = CGRect(x: trailing, y: safe.top + 8, width: side, height: side)
        let counterSize = counter.sizeThatFits(CGSize(width: bounds.width / 2, height: side))
        counter.frame = CGRect(
            x: (bounds.width - counterSize.width) / 2,
            y: safe.top + 8 + (side - counterSize.height) / 2,
            width: counterSize.width,
            height: counterSize.height
        )

        let bottomHeight = safe.bottom + bottomChromeHeight
        bottomChrome.frame = CGRect(x: 0, y: bounds.height - bottomHeight, width: bounds.width, height: bottomHeight)
        strip.frame = CGRect(x: 0, y: 0, width: bounds.width, height: Self.stripHeight)
        if strip.bounds.width != lastStripWidth {
            lastStripWidth = strip.bounds.width
            let inset = max(0, (strip.bounds.width - stripLayout.itemSize.width) / 2)
            stripLayout.sectionInset = UIEdgeInsets(top: 5, left: inset, bottom: 5, right: inset)
            stripLayout.invalidateLayout()
            strip.layoutIfNeeded()
            strip.scrollToItem(at: IndexPath(item: selectedIndex, section: 0), at: .centeredHorizontally, animated: false)
        }
        let buttonY = hasStrip ? Self.stripHeight + 8 : 0
        shareButton.frame = CGRect(x: leading, y: buttonY, width: side, height: side)
        saveButton.frame = CGRect(x: trailing, y: buttonY, width: side, height: side)

        for case let cell as StudioViewerPageCell in pager.visibleCells {
            cell.playerChromeInsets = playerChromeInsets
        }
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isBeingDismissed || presentingViewController == nil {
            prepareTask?.cancel()
            media.reset()
        }
    }

    private var playerChromeInsets: UIEdgeInsets {
        UIEdgeInsets(top: Self.topChromeHeight, left: 0, bottom: bottomChromeHeight, right: 0)
    }

    // MARK: Transition host

    var transitionSelectedId: String { selected.id }

    var transitionContent: (image: UIImage?, player: AVPlayer?) {
        (media.image(for: selected.id), media.player(for: selected.id))
    }

    func transitionImageFrame(in coordinateView: UIView) -> CGRect {
        if let cell = pager.cellForItem(at: IndexPath(item: selectedIndex, section: 0)) as? StudioViewerPageCell {
            let frame = cell.visibleMediaFrame(in: coordinateView)
            if frame.width > 1, frame.height > 1 { return frame }
        }
        return view.convert(AVMakeRect(aspectRatio: CGSize(width: aspectRatio(selected), height: 1), insideRect: view.bounds), to: coordinateView)
    }

    func setTransitionState(backdrop backdropAlpha: CGFloat, chrome: CGFloat, mediaHidden: Bool) {
        backdrop.alpha = backdropAlpha
        transitionChrome = chrome
        pager.alpha = mediaHidden ? 0 : 1
        updateChromeAlpha()
    }

    func interactiveDismissalDidCancel() {
        activeDismissal = nil
        isDismissing = false
        media.resumeSelected()
        updateChromeAlpha()
        setNeedsStatusBarAppearanceUpdate()
    }

    // MARK: Collections

    func collectionView(_ collectionView: UICollectionView, numberOfItemsInSection section: Int) -> Int { items.count }

    func collectionView(_ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath) -> UICollectionViewCell {
        if collectionView === strip {
            let cell = collectionView.dequeueReusableCell(withReuseIdentifier: ThreadFilmstripCell.reuseIdentifier, for: indexPath)
            if let cell = cell as? ThreadFilmstripCell { configureThumb(cell, at: indexPath.item) }
            return cell
        }
        let cell = collectionView.dequeueReusableCell(withReuseIdentifier: StudioViewerPageCell.reuseIdentifier, for: indexPath)
        if let cell = cell as? StudioViewerPageCell { configurePage(cell, at: indexPath.item) }
        return cell
    }

    func collectionView(_ collectionView: UICollectionView, didEndDisplaying cell: UICollectionViewCell, forItemAt indexPath: IndexPath) {
        guard let page = cell as? StudioViewerPageCell else { return }
        page.detachPlayer()
        page.resetZoom()
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        guard collectionView === strip else { return }
        UISelectionFeedbackGenerator().selectionChanged()
        select(indexPath.item, from: .strip)
        strip.scrollToItem(at: indexPath, at: .centeredHorizontally, animated: true)
    }

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        if scrollView === strip { stripDriving = true }
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        // Scrubbing the strip pages the pager along with it.
        guard scrollView === strip, stripDriving, let index = centeredStripIndex(), index != selectedIndex else { return }
        UISelectionFeedbackGenerator().selectionChanged()
        select(index, from: .strip, prepareAfter: .milliseconds(100))
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { settle(scrollView) }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) { settle(scrollView) }

    func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        if scrollView === pager { settle(scrollView) }
    }

    func accessibilityScrollStatus(for scrollView: UIScrollView) -> String? {
        "\(selectedIndex + 1) of \(items.count)"
    }

    private func settle(_ scrollView: UIScrollView) {
        if scrollView === strip {
            stripDriving = false
            if let index = centeredStripIndex() { select(index, from: .strip) }
            return
        }
        guard scrollView === pager, pager.bounds.width > 0 else { return }
        let index = Int(round(pager.contentOffset.x / pager.bounds.width))
        guard items.indices.contains(index) else { return }
        select(index, from: .pager)
    }

    private func centeredStripIndex() -> Int? {
        let center = CGPoint(x: strip.bounds.midX, y: strip.bounds.midY)
        let path = strip.indexPathForItem(at: center) ?? strip.indexPathsForVisibleItems.min { a, b in
            let ax = strip.layoutAttributesForItem(at: a)?.center.x ?? 0
            let bx = strip.layoutAttributesForItem(at: b)?.center.x ?? 0
            return abs(ax - center.x) < abs(bx - center.x)
        }
        return path.map(\.item).flatMap { items.indices.contains($0) ? $0 : nil }
    }

    private enum SelectionSource { case pager, strip }

    private func select(_ index: Int, from source: SelectionSource, prepareAfter delay: Duration? = nil) {
        guard items.indices.contains(index) else { return }
        let changed = index != selectedIndex
        selectedIndex = index
        if source != .pager, changed {
            pager.scrollToItem(at: IndexPath(item: index, section: 0), at: .centeredHorizontally, animated: false)
        }
        if source != .strip {
            strip.scrollToItem(at: IndexPath(item: index, section: 0), at: .centeredHorizontally, animated: true)
        }
        refreshStripSelection()
        updateCounter()
        prepareTask?.cancel()
        guard let delay else {
            media.prepare(selected: index, in: items)
            return
        }
        // A fast scrub crosses many pages; only the one it rests on loads.
        prepareTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self, selectedIndex == index else { return }
            media.prepare(selected: index, in: items)
        }
    }

    private func refresh(_ id: String) {
        guard let index = items.firstIndex(where: { $0.id == id }) else { return }
        let path = IndexPath(item: index, section: 0)
        if let page = pager.cellForItem(at: path) as? StudioViewerPageCell { configurePage(page, at: index) }
        if let thumb = strip.cellForItem(at: path) as? ThreadFilmstripCell { configureThumb(thumb, at: index) }
        if index == selectedIndex { updateButtons() }
    }

    private func refreshStripSelection() {
        for case let cell as ThreadFilmstripCell in strip.visibleCells {
            guard let path = strip.indexPath(for: cell) else { continue }
            cell.setSelected(path.item == selectedIndex)
        }
    }

    private func aspectRatio(_ item: ThreadMediaItem) -> CGFloat {
        guard let image = media.image(for: item.id), image.size.height > 0 else { return item.isVideo ? 16 / 9 : 1 }
        return image.size.width / image.size.height
    }

    private func configurePage(_ cell: StudioViewerPageCell, at index: Int) {
        let item = items[index]
        cell.playerChromeInsets = playerChromeInsets
        cell.onTap = { [weak self] in self?.setChromeHidden(!(self?.chromeHidden ?? false)) }
        cell.onRetry = { [weak self] in self?.media.retry(item) }
        let name = item.file?.name
        let kind = item.isVideo ? "Video" : "Image"
        cell.configure(
            id: item.id,
            isVideo: item.isVideo,
            accessibilityLabel: name.map { "\(kind), \($0)" } ?? kind,
            aspectRatio: aspectRatio(item),
            image: media.image(for: item.id),
            originalState: media.state(for: item),
            player: media.player(for: item.id),
            parentViewController: self
        )
    }

    private func configureThumb(_ cell: ThreadFilmstripCell, at index: Int) {
        let item = items[index]
        cell.configure(item: item, image: media.image(for: item.id), selected: index == selectedIndex, loader: loader)
    }

    // MARK: Dismiss pan

    func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard gestureRecognizer === dismissPan else { return true }
        guard transitionController != nil, !isDismissing, presentedViewController == nil else { return false }
        if let cell = pager.cellForItem(at: IndexPath(item: selectedIndex, section: 0)) as? StudioViewerPageCell, cell.isZoomed {
            return false
        }
        let velocity = dismissPan.velocity(in: view)
        return velocity.y > 0 && velocity.y > abs(velocity.x) * 1.2
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        guard gestureRecognizer === dismissPan, let touched = touch.view else { return true }
        if touched is UIControl { return false }
        if topChrome.alpha > 0.05, touched.isDescendant(of: topChrome) { return false }
        if bottomChrome.alpha > 0.05, touched.isDescendant(of: bottomChrome) { return false }
        return true
    }

    @objc private func handleDismissPan(_ gesture: UIPanGestureRecognizer) {
        switch gesture.state {
        case .began:
            guard let transitionController else { return }
            let dismissal = StudioViewerInteractiveDismissal(transition: transitionController, viewer: self, touch: gesture.location(in: view))
            activeDismissal = dismissal
            transitionController.interactiveDismissal = dismissal
            media.pausePlayback()
            close()
        case .changed:
            activeDismissal?.update(translation: gesture.translation(in: view))
        case .ended:
            activeDismissal?.update(translation: gesture.translation(in: view))
            activeDismissal?.end(velocity: gesture.velocity(in: view))
        case .cancelled, .failed:
            activeDismissal?.update(translation: .zero)
            activeDismissal?.end(velocity: .zero)
        default:
            break
        }
    }

    // MARK: Chrome

    private func configureChrome() {
        circle(closeButton, "chevron.backward", label: "Close") { [weak self] in self?.close() }
        topChrome.addSubview(closeButton)

        circle(moreButton, "ellipsis", label: "More", action: nil)
        moreButton.showsMenuAsPrimaryAction = true
        moreButton.menu = UIMenu(children: [UIDeferredMenuElement.uncached { [weak self] completion in
            completion(self?.actions() ?? [])
        }])
        topChrome.addSubview(moreButton)

        counter.font = Fonts.ui(.sansMedium, TypeScale.size(15))
        counter.textColor = .white
        counter.layer.shadowColor = UIColor.black.cgColor
        counter.layer.shadowOpacity = 0.5
        counter.layer.shadowRadius = 6
        counter.layer.shadowOffset = .zero
        counter.accessibilityTraits = .updatesFrequently
        topChrome.addSubview(counter)

        circle(shareButton, "square.and.arrow.up", label: "Share") { [weak self] in self?.share() }
        circle(saveButton, "square.and.arrow.down", label: "Save to Photos") { [weak self] in self?.save() }
        bottomChrome.addSubview(shareButton)
        bottomChrome.addSubview(saveButton)
        updateButtons()
    }

    private func circle(_ button: UIButton, _ symbol: String, label: String, action: (() -> Void)?) {
        var configuration = UIButton.Configuration.glass()
        configuration.image = UIImage(systemName: symbol, withConfiguration: UIImage.SymbolConfiguration(pointSize: 17, weight: .medium))
        configuration.baseForegroundColor = .white
        configuration.cornerStyle = .capsule
        button.configuration = configuration
        button.accessibilityLabel = label
        if let action { button.addAction(UIAction { _ in action() }, for: .touchUpInside) }
    }

    private func updateCounter() {
        counter.text = items.count > 1 ? "\(selectedIndex + 1) of \(items.count)" : nil
        counter.isHidden = items.count < 2
        view.setNeedsLayout()
    }

    /// Sharing or saving needs the picture (or a video's file) in hand.
    private func updateButtons() {
        let ready = selected.isVideo ? selected.file != nil : media.image(for: selected.id) != nil || selected.file != nil
        for button in [shareButton, saveButton] { button.isEnabled = ready && !busy }
    }

    private func setChromeHidden(_ hidden: Bool) {
        guard chromeHidden != hidden else { return }
        chromeHidden = hidden
        let apply = {
            self.updateChromeAlpha()
            self.setNeedsStatusBarAppearanceUpdate()
        }
        if UIAccessibility.isReduceMotionEnabled {
            apply()
        } else {
            UIView.animate(withDuration: 0.22, delay: 0, options: [.beginFromCurrentState, .allowUserInteraction], animations: apply)
        }
        setNeedsUpdateOfHomeIndicatorAutoHidden()
    }

    private func updateChromeAlpha() {
        let alpha = chromeHidden ? 0 : transitionChrome
        topChrome.alpha = alpha
        bottomChrome.alpha = alpha
        let live = alpha > 0.05 && !isDismissing
        for chrome in [topChrome, bottomChrome] {
            chrome.isUserInteractionEnabled = live
            chrome.accessibilityElementsHidden = !live
        }
    }

    private func close() {
        guard !isDismissing else { return }
        isDismissing = true
        updateChromeAlpha()
        dismiss(animated: true)
    }

    // MARK: Actions

    private func actions() -> [UIMenuElement] {
        var actions: [UIMenuElement] = [
            UIAction(title: "Save to Photos", image: UIImage(systemName: "square.and.arrow.down")) { [weak self] _ in self?.save() },
            UIAction(title: "Share…", image: UIImage(systemName: "square.and.arrow.up")) { [weak self] _ in self?.share() },
        ]
        if !selected.isVideo, let image = media.image(for: selected.id) {
            actions.append(UIAction(title: "Copy", image: UIImage(systemName: "doc.on.doc")) { [weak self] _ in
                UIPasteboard.general.image = image
                Toast.show("Copied", in: self?.view.window, above: self?.bottomChrome)
            })
        }
        return actions
    }

    func contextMenuInteraction(_ interaction: UIContextMenuInteraction, configurationForMenuAtLocation location: CGPoint) -> UIContextMenuConfiguration? {
        // Videos keep their long press for AVKit.
        guard !selected.isVideo, !isDismissing, liftableImageView != nil else { return nil }
        return UIContextMenuConfiguration(identifier: selected.id as NSString, previewProvider: nil) { [weak self] _ in
            UIMenu(children: self?.actions() ?? [])
        }
    }

    func contextMenuInteraction(
        _ interaction: UIContextMenuInteraction,
        configuration: UIContextMenuConfiguration,
        highlightPreviewForItemWithIdentifier identifier: any NSCopying
    ) -> UITargetedPreview? {
        guard let imageView = liftableImageView, imageView.window != nil else { return nil }
        let parameters = UIPreviewParameters()
        parameters.backgroundColor = .clear
        return UITargetedPreview(view: imageView, parameters: parameters)
    }

    private var liftableImageView: UIView? {
        (pager.cellForItem(at: IndexPath(item: selectedIndex, section: 0)) as? StudioViewerPageCell)?.liftableImageView
    }

    /// The file behind the selection (shared/sent files), on disk.
    private func localFile(_ item: ThreadMediaItem) async -> URL? {
        guard let file = item.file else { return nil }
        return await loader.fileURL(file, false)
    }

    private func setBusy(_ value: Bool, _ button: UIButton? = nil) {
        busy = value
        button?.configuration?.showsActivityIndicator = value
        if !value { shareButton.configuration?.showsActivityIndicator = false; saveButton.configuration?.showsActivityIndicator = false }
        updateButtons()
    }

    private func share() {
        guard !busy else { return }
        let item = selected
        if item.file == nil, let image = media.image(for: item.id) {
            presentShare([image])
            return
        }
        setBusy(true, shareButton)
        Task { [weak self] in
            let url = await self?.localFile(item)
            guard let self else { return }
            setBusy(false)
            guard !isDismissing, view.window != nil else { return }
            guard let url else {
                presentFailure("Couldn't share")
                return
            }
            presentShare([url])
        }
    }

    private func presentShare(_ activityItems: [Any]) {
        guard presentedViewController == nil else { return }
        let sheet = UIActivityViewController(activityItems: activityItems, applicationActivities: nil)
        if let popover = sheet.popoverPresentationController {
            popover.sourceView = shareButton.window == nil ? view : shareButton
            popover.sourceRect = shareButton.window == nil ? CGRect(x: view.bounds.midX, y: view.bounds.maxY, width: 0, height: 0) : shareButton.bounds
        }
        present(sheet, animated: true)
    }

    private func save() {
        guard !busy else { return }
        let item = selected
        let image = media.image(for: item.id)
        setBusy(true, saveButton)
        Task { [weak self] in
            do {
                try await StudioArtifactActions.requestPhotoAddAccess()
                let url = item.file == nil ? nil : await self?.localFile(item)
                guard self != nil else { return }
                if item.file != nil, url == nil { throw CocoaError(.fileReadUnknown) }
                try await PHPhotoLibrary.shared().performChanges {
                    if let url {
                        PHAssetCreationRequest.forAsset().addResource(with: item.isVideo ? .video : .photo, fileURL: url, options: nil)
                    } else if let image {
                        PHAssetCreationRequest.creationRequestForAsset(from: image)
                    }
                }
                guard let self else { return }
                UINotificationFeedbackGenerator().notificationOccurred(.success)
                Toast.show(item.isVideo ? "Video saved to Photos" : "Saved to Photos", in: view.window, above: bottomChrome)
            } catch StudioArtifactActionError.photoAccessDenied {
                self?.presentFailure("Allow Photos access", message: "Zeron needs permission to add to your photo library. You can allow it in Settings.")
            } catch {
                self?.presentFailure("Couldn't save to Photos")
            }
            self?.setBusy(false)
        }
    }

    private func presentFailure(_ title: String, message: String? = nil) {
        guard view.window != nil, !isDismissing, presentedViewController == nil else { return }
        let alert = UIAlertController(title: title, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
}

/// A filmstrip thumbnail: the picture (or a video's poster, with a play
/// badge), outlined when it's the page on screen.
@MainActor
private final class ThreadFilmstripCell: UICollectionViewCell {
    static let reuseIdentifier = "ThreadFilmstripCell"

    private let imageView = UIImageView()
    private let placeholder = UIImageView()
    private let badge = UIImageView(image: UIImage(systemName: "play.fill", withConfiguration: UIImage.SymbolConfiguration(pointSize: 9, weight: .bold)))
    private var representedId: String?

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = Palette.chip
        contentView.layer.cornerRadius = 6
        contentView.layer.cornerCurve = .continuous
        contentView.clipsToBounds = true
        contentView.layer.borderColor = UIColor.white.cgColor
        for v in [imageView, placeholder] {
            v.frame = contentView.bounds
            v.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            contentView.addSubview(v)
        }
        imageView.contentMode = .scaleAspectFill
        imageView.clipsToBounds = true
        placeholder.contentMode = .center
        placeholder.tintColor = Palette.color(.textFaint).withAlphaComponent(0.5)
        badge.tintColor = .white
        badge.layer.shadowColor = UIColor.black.cgColor
        badge.layer.shadowOpacity = 0.6
        badge.layer.shadowRadius = 2
        badge.layer.shadowOffset = .zero
        contentView.addSubview(badge)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func layoutSubviews() {
        super.layoutSubviews()
        badge.sizeToFit()
        badge.frame.origin = CGPoint(x: 5, y: contentView.bounds.height - badge.bounds.height - 5)
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        representedId = nil
        imageView.image = nil
        imageView.accessibilityIdentifier = nil
    }

    func configure(item: ThreadMediaItem, image: UIImage?, selected: Bool, loader: ThreadMediaLoader) {
        let fresh = representedId != item.id
        representedId = item.id
        setSelected(selected)
        badge.isHidden = !item.isVideo
        placeholder.image = UIImage(systemName: item.isVideo ? "film" : "photo")
        if let image {
            imageView.image = image
        } else if fresh {
            imageView.image = nil
            // Inline pictures come from the transcript's own loader (cached).
            if item.file == nil { loader.thumbnail(item.id, imageView) }
        }
        placeholder.isHidden = imageView.image != nil || (item.file == nil && !item.isVideo)
    }

    func setSelected(_ selected: Bool) {
        contentView.layer.borderWidth = selected ? 2 : 0
        contentView.alpha = selected ? 1 : 0.72
    }
}
