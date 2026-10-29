import UIKit

/// The Studio gallery: every creation on the selected desktop as a grid of
/// squares (three columns on a phone, more as the width allows), oldest at
/// the top and opened at the newest (the bottom). Scrolling toward the top
/// pages in older items well before it gets there; previews and thumbhashes
/// prefetch ahead of the scroll. Answers the viewer's zoom transition for
/// its cells.
///
/// Built for libraries in the thousands: the grid is plain arithmetic (no
/// per-item layout pass), a page of older items is a batch insert above
/// what's on screen (no diff of the whole library), and the offset moves by
/// exactly the rows inserted, in the same pass.
final class StudioGalleryViewController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate,
    UICollectionViewDataSourcePrefetching,
    StudioGalleryTransitionSourceProvider
{
    private let browser: StudioBrowserStore
    private let service: StudioService
    private let transitionSource: StudioGalleryTransitionSource
    private let openViewer: (StudioViewerSession) -> Void
    private let showThread: (String) -> Void

    private var collectionView: StudioGalleryCollectionView!
    private let layout = StudioGalleryGridLayout()
    private let loadingView = StudioLoadingView("Loading gallery…")
    private var items: [StudioGalleryItem] = []
    private var prefetchedIds: Set<String> = []
    private var requestedOldestId: String?
    private var didSetInitialPosition = false
    private var positioningInitialItem = false

    init(
        browser: StudioBrowserStore,
        service: StudioService,
        transitionSource: StudioGalleryTransitionSource,
        openViewer: @escaping (StudioViewerSession) -> Void,
        showThread: @escaping (String) -> Void
    ) {
        self.browser = browser
        self.service = service
        self.transitionSource = transitionSource
        self.openViewer = openViewer
        self.showThread = showThread
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear

        collectionView = StudioGalleryCollectionView(frame: view.bounds, collectionViewLayout: layout)
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.alwaysBounceVertical = true
        collectionView.contentInsetAdjustmentBehavior = .automatic
        collectionView.keyboardDismissMode = .onDrag
        collectionView.topEdgeEffect.style = .soft
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.prefetchDataSource = self
        collectionView.register(StudioGalleryCell.self, forCellWithReuseIdentifier: StudioGalleryCell.reuseIdentifier)
        collectionView.layoutChanged = { [weak self] in self?.positionInitialItemIfPossible() }
        view.addSubview(collectionView)

        loadingView.frame = view.bounds
        loadingView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(loadingView)
        transitionSource.provider = self
    }

    /// Back from a thread: the viewer closes into the grid again.
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        transitionSource.provider = self
    }

    /// Tracks the store: reads here re-run this when they change.
    override func updateProperties() {
        super.updateProperties()
        let gallery = browser.gallery
        let loading = browser.galleryLoading && gallery.isEmpty
        let error = gallery.isEmpty ? browser.galleryError : nil
        loadingView.isHidden = !loading
        collectionView.isHidden = gallery.isEmpty
        if loading {
            contentUnavailableConfiguration = nil
        } else if let error {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "Gallery unavailable", message: error, symbol: "exclamationmark.triangle",
                retry: { [weak self] in self?.browser.reload() }
            )
        } else if gallery.isEmpty {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "No creations yet",
                message: "Images and videos from your Studio threads will appear here.",
                symbol: "photo.stack"
            )
        } else {
            contentUnavailableConfiguration = nil
        }
        StudioGalleryPerf.time({ apply(gallery) }) { ms in
            StudioGalleryPerf.applies += 1
            StudioGalleryPerf.maxApplyMs = max(StudioGalleryPerf.maxApplyMs, ms)
            StudioGalleryPerf.totalApplyMs += ms
        }
    }

    /// `-studio-bench`: scripted flings through the whole library.
    func runBench() {
        guard let collectionView else { return }
        StudioGalleryBench(collectionView: collectionView, hasMore: { [weak self] in
            self?.browser.galleryHasMore ?? false
        }, tileState: { cell in
            (cell as? StudioGalleryCell).map { ($0.displayedImage != nil, $0.showsPreviewImage) }
        }).run()
    }

    // MARK: Items

    private func apply(_ incoming: [StudioGalleryItem]) {
        // The store hands the same array back when nothing changed.
        if incoming == items { return }
        // A repeated id would show one artifact twice (a page boundary
        // shifting under a merge): keep its first place.
        var seen = Set<String>(minimumCapacity: incoming.count)
        var newItems = incoming
        if Set(incoming.map(\.id)).count != incoming.count {
            newItems = incoming.filter { seen.insert($0.id).inserted }
        }
        let old = items

        guard old.count != newItems.count || !old.indices.allSatisfy({ old[$0].id == newItems[$0].id }) else {
            // Same squares; an item's details may have landed since (its
            // thumbhash, a video's duration): only squares on screen care.
            items = newItems
            for indexPath in collectionView.indexPathsForVisibleItems {
                guard let cell = collectionView.cellForItem(at: indexPath) as? StudioGalleryCell,
                      old[indexPath.item] != newItems[indexPath.item] else { continue }
                configure(cell, with: newItems[indexPath.item])
            }
            return
        }

        if newItems.isEmpty || old.isEmpty || collectionView.window == nil {
            // Emptied (another machine, a reload): open at the newest again.
            if newItems.isEmpty { didSetInitialPosition = false }
            requestedOldestId = nil
            items = newItems
            layout.resetLeadingBlanks()
            collectionView.reloadData()
            if didSetInitialPosition { collectionView.layoutIfNeeded() } else { positionInitialItemIfPossible() }
            return
        }

        let start = CACurrentMediaTime()
        let wasNearBottom = collectionView.contentSize.height - collectionView.bounds.height
            + collectionView.adjustedContentInset.bottom - collectionView.contentOffset.y < 100
        // What's on screen stays where it is: the topmost visible square's
        // distance from the top of the viewport, before and after.
        let anchor = topVisibleIndex().flatMap { index -> (id: String, gap: CGFloat)? in
            guard old.indices.contains(index) else { return nil }
            return (old[index].id, layout.frame(forItem: index).minY - collectionView.contentOffset.y)
        }
        if newItems.first?.id != old.first?.id { requestedOldestId = nil }

        let prepended = newItems.count > old.count
            && zip(newItems[(newItems.count - old.count)...], old).allSatisfy { $0.id == $1.id }
        let appended = !prepended && newItems.count > old.count
            && zip(newItems, old).allSatisfy { $0.id == $1.id }
        UIView.performWithoutAnimation {
            if prepended {
                // Older items above: the columns of what's here stay put
                // (blank leading slots absorb a page that isn't a whole
                // number of rows), so nothing on screen reflows.
                let inserted = newItems.count - old.count
                layout.prependItems(inserted)
                collectionView.performBatchUpdates {
                    items = newItems
                    collectionView.insertItems(at: (0..<inserted).map { IndexPath(item: $0, section: 0) })
                }
            } else if appended {
                collectionView.performBatchUpdates {
                    items = newItems
                    collectionView.insertItems(at: (old.count..<newItems.count).map { IndexPath(item: $0, section: 0) })
                }
            } else {
                items = newItems
                collectionView.reloadData()
            }
            collectionView.layoutIfNeeded()
        }
        StudioGalleryPerf.maxSnapshotMs = max(StudioGalleryPerf.maxSnapshotMs, (CACurrentMediaTime() - start) * 1000)

        let relayoutStart = CACurrentMediaTime()
        if !didSetInitialPosition {
            positionInitialItemIfPossible()
        } else if appended, wasNearBottom {
            collectionView.scrollToItem(at: IndexPath(item: newItems.count - 1, section: 0), at: .bottom, animated: false)
        } else if let anchor, let index = newItems.firstIndex(where: { $0.id == anchor.id }) {
            let target = layout.frame(forItem: index).minY - anchor.gap
            if abs(target - collectionView.contentOffset.y) > 0.25 {
                // Set mid-fling too: the scroll view decelerates on from here.
                collectionView.contentOffset.y = target
            }
        }
        StudioGalleryPerf.maxRelayoutMs = max(StudioGalleryPerf.maxRelayoutMs, (CACurrentMediaTime() - relayoutStart) * 1000)
        if let anchor, let index = newItems.firstIndex(where: { $0.id == anchor.id }),
           let cell = collectionView.cellForItem(at: IndexPath(item: index, section: 0)) {
            // What the user would see move: the anchor square's on-screen spot.
            let drift = abs(cell.frame.minY - collectionView.contentOffset.y - anchor.gap)
            StudioGalleryPerf.maxAnchorDriftPt = max(StudioGalleryPerf.maxAnchorDriftPt, drift)
        }
        if prepended, let requested = StudioGalleryPerf.pageRequestedAt {
            StudioGalleryPerf.pageLatenciesMs.append((CACurrentMediaTime() - requested) * 1000)
            StudioGalleryPerf.pageRequestedAt = nil
        }
        loadOlderIfNear()
    }

    private func item(at indexPath: IndexPath) -> StudioGalleryItem? {
        items.indices.contains(indexPath.item) ? items[indexPath.item] : nil
    }

    private func indexPath(for artifactId: String) -> IndexPath? {
        items.firstIndex { $0.id == artifactId }.map { IndexPath(item: $0, section: 0) }
    }

    private func configure(_ cell: StudioGalleryCell, with item: StudioGalleryItem) {
        StudioGalleryPerf.time({
            cell.configure(item: item, browser: browser, service: service, deviceId: browser.selectedDeviceId)
        }) { ms in
            StudioGalleryPerf.configures += 1
            StudioGalleryPerf.maxConfigureMs = max(StudioGalleryPerf.maxConfigureMs, ms)
        }
    }

    /// The newest item at the bottom, once the grid is on screen with a size
    /// to place it in (off screen it has no safe area yet, and the newest
    /// row would land under the home indicator).
    private func positionInitialItemIfPossible() {
        guard !didSetInitialPosition, !positioningInitialItem, !items.isEmpty,
              let collectionView, collectionView.window != nil,
              collectionView.bounds.width > 0, collectionView.bounds.height > 0,
              collectionView.numberOfItems(inSection: 0) == items.count else { return }
        positioningInitialItem = true
        collectionView.layoutIfNeeded()
        didSetInitialPosition = true
        collectionView.scrollToItem(at: IndexPath(item: items.count - 1, section: 0), at: .bottom, animated: false)
        collectionView.layoutIfNeeded()
        positioningInitialItem = false
        // A gallery short enough to show its top still pages in what's older.
        loadOlderIfNear()
    }

    /// The topmost square at least partly on screen (arithmetic, not a
    /// layout query: this runs every scrolled frame).
    private func topVisibleIndex() -> Int? {
        guard let collectionView, !items.isEmpty else { return nil }
        let index = layout.firstItem(atOrBelow: collectionView.contentOffset.y + collectionView.adjustedContentInset.top)
        return min(max(index, 0), items.count - 1)
    }

    /// Older items page in while about three screens of rows remain above
    /// (a hard fling covers that in about half a second, a page's round
    /// trip with time to spare).
    private func loadOlderIfNear() {
        guard didSetInitialPosition, !positioningInitialItem, let collectionView,
              browser.galleryHasMore, let top = topVisibleIndex() else { return }
        let rowsPerScreen = max(1, Int(collectionView.bounds.height / max(layout.rowHeight, 1)))
        if top < layout.columns * rowsPerScreen * 3 { loadOlderOnce() }
    }

    /// Once per oldest item: a failed page clears it, so scrolling toward
    /// the top tries again.
    private func loadOlderOnce() {
        let oldestId = items.first?.id
        guard requestedOldestId != oldestId, !browser.galleryLoadingMore,
              let deviceId = browser.selectedDeviceId else { return }
        requestedOldestId = oldestId
        if StudioGalleryPerf.pageRequestedAt == nil { StudioGalleryPerf.pageRequestedAt = CACurrentMediaTime() }
        Task { [weak self, browser, service] in
            await browser.loadMoreGallery(workspace: service, deviceId: deviceId)
            guard let self, self.items.first?.id == oldestId, self.requestedOldestId == oldestId else { return }
            // Nothing older arrived (a failed page): free to ask again.
            self.requestedOldestId = nil
        }
    }

    // MARK: Transition source

    func transitionImageView(for artifactId: String) -> UIView? {
        guard let collectionView, let indexPath = indexPath(for: artifactId) else { return nil }
        // Never move or synchronously lay out a thousand-item gallery from
        // inside UIKit's transition callback. If paging moved away from a
        // visible source, returning nil gives UIKit a clean fade fallback.
        return (collectionView.cellForItem(at: indexPath) as? StudioGalleryCell)?.imageView
    }

    func prepareTransitionImageView(for artifactId: String) {
        guard let collectionView, let indexPath = indexPath(for: artifactId),
              collectionView.cellForItem(at: indexPath) == nil else { return }
        collectionView.scrollToItem(at: indexPath, at: .centeredVertically, animated: false)
        collectionView.layoutIfNeeded()
    }

    // MARK: Collection view

    func collectionView(_ collectionView: UICollectionView, numberOfItemsInSection section: Int) -> Int {
        items.count
    }

    func collectionView(_ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath) -> UICollectionViewCell {
        let cell = collectionView.dequeueReusableCell(withReuseIdentifier: StudioGalleryCell.reuseIdentifier, for: indexPath)
        if let cell = cell as? StudioGalleryCell, let item = item(at: indexPath) { configure(cell, with: item) }
        return cell
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        loadOlderIfNear()
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        guard let item = item(at: indexPath) else { return }
        let image = (collectionView.cellForItem(at: indexPath) as? StudioGalleryCell)?.displayedImage
        openViewer(StudioViewerSession(
            artifacts: browser.gallery.map(StudioArtifactDetail.init(item:)),
            selectedId: item.id,
            openedFromGallery: true,
            openingPreview: image ?? browser.cachedPreview(artifactId: item.id)
        ))
    }

    /// The full image starts on touch-down, ahead of the viewer.
    func collectionView(_ collectionView: UICollectionView, didHighlightItemAt indexPath: IndexPath) {
        guard let item = item(at: indexPath), let deviceId = browser.selectedDeviceId else { return }
        browser.preheatDisplayImage(artifact: StudioArtifactDetail(item: item), deviceId: deviceId, workspace: service)
    }

    func collectionView(_ collectionView: UICollectionView, willDisplay cell: UICollectionViewCell, forItemAt indexPath: IndexPath) {
        guard let item = item(at: indexPath) else { return }
        // A cell kept by cell prefetching comes back without a dequeue: its
        // read was let go when it left, so it asks again (a no-op otherwise).
        if let cell = cell as? StudioGalleryCell { configure(cell, with: item) }
        // After the square's own (visible) hold, so the read isn't dropped.
        endPrefetch(for: item.id)
    }

    func collectionView(_ collectionView: UICollectionView, didEndDisplaying cell: UICollectionViewCell, forItemAt indexPath: IndexPath) {
        (cell as? StudioGalleryCell)?.cancelPreviewRequest()
    }

    func collectionView(_ collectionView: UICollectionView, prefetchItemsAt indexPaths: [IndexPath]) {
        let upcoming = indexPaths.compactMap(item(at:))
        browser.warmThumbhashes(upcoming)
        guard let deviceId = browser.selectedDeviceId else { return }
        for item in upcoming where prefetchedIds.insert(item.id).inserted {
            _ = browser.beginPreviewRequest(artifactId: item.id, deviceId: deviceId, workspace: service, priority: .prefetch)
        }
    }

    func collectionView(_ collectionView: UICollectionView, cancelPrefetchingForItemsAt indexPaths: [IndexPath]) {
        for indexPath in indexPaths {
            if let item = item(at: indexPath) { endPrefetch(for: item.id) }
        }
    }

    private func endPrefetch(for artifactId: String) {
        guard prefetchedIds.remove(artifactId) != nil else { return }
        browser.endPreviewRequest(artifactId: artifactId)
    }

    func collectionView(_ collectionView: UICollectionView, contextMenuConfigurationForItemAt indexPath: IndexPath, point: CGPoint) -> UIContextMenuConfiguration? {
        guard let item = item(at: indexPath) else { return nil }
        return UIContextMenuConfiguration(identifier: item.id as NSString, previewProvider: nil) { [weak self] _ in
            UIMenu(children: [
                UIAction(title: "Save to Photos", image: UIImage(systemName: "square.and.arrow.down")) { _ in
                    self?.download(StudioArtifactDetail(item: item))
                },
                UIAction(title: "Show in Thread", image: UIImage(systemName: "rectangle.stack")) { _ in
                    self?.showThread(item.conversationId)
                },
                UIAction(title: "Delete", image: UIImage(systemName: "trash"), attributes: .destructive) { _ in
                    self?.confirmDelete(StudioArtifactDetail(item: item))
                },
            ])
        }
    }

    // MARK: Actions

    /// Into Photos, saying so: an original (a video especially) can take a
    /// while to come over the relay.
    private func download(_ artifact: StudioArtifactDetail) {
        guard let deviceId = browser.selectedDeviceId else { return }
        Toast.show(artifact.mediaKind == .video ? "Saving video to Photos…" : "Saving to Photos…", in: view.window)
        Task {
            do {
                try await StudioArtifactActions.download(artifact, workspace: service, deviceId: deviceId)
                Toast.show("Saved to Photos", in: view.window)
            } catch is CancellationError {
            } catch {
                showActionError(error.localizedDescription)
            }
        }
    }

    private func confirmDelete(_ artifact: StudioArtifactDetail) {
        let desktop = browser.selectedDeviceId.flatMap { id in service.hosts.first { $0.id == id }?.name }
        let sheet = UIAlertController(
            title: artifact.mediaKind == .video ? "Delete this video?" : "Delete this image?",
            message: "This removes it from Studio on \(desktop ?? "the desktop"). It can't be undone.",
            preferredStyle: .actionSheet
        )
        sheet.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak self] _ in self?.delete(artifact) })
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        if let popover = sheet.popoverPresentationController {
            if let path = indexPath(for: artifact.id), let cell = collectionView.cellForItem(at: path) {
                popover.sourceView = cell
                popover.sourceRect = cell.bounds
            } else {
                popover.sourceView = view
                popover.sourceRect = CGRect(x: view.bounds.midX, y: view.bounds.midY, width: 0, height: 0)
            }
        }
        present(sheet, animated: true)
    }

    private func delete(_ artifact: StudioArtifactDetail) {
        guard let deviceId = browser.selectedDeviceId else { return }
        Task {
            do {
                try await service.deleteStudioArtifact(deviceId: deviceId, artifactId: artifact.id)
                browser.removeArtifact(artifact.id)
            } catch {
                showActionError(error.localizedDescription)
            }
        }
    }

    private func showActionError(_ message: String) {
        // Something already over the gallery: an alert would be dropped.
        guard view.window != nil, presentedViewController == nil else {
            return Toast.show(message, in: view.window)
        }
        let alert = UIAlertController(title: "Studio action failed", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
}

/// Reports size changes and arriving on screen, so the gallery can open at
/// its newest item once it's been laid out where it'll be seen.
private final class StudioGalleryCollectionView: UICollectionView {
    var layoutChanged: (() -> Void)?
    private var lastLayoutSize: CGSize = .zero

    override func layoutSubviews() {
        super.layoutSubviews()
        guard bounds.size != lastLayoutSize else { return }
        lastLayoutSize = bounds.size
        layoutChanged?()
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil { layoutChanged?() }
    }

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        layoutChanged?()
    }
}


/// The gallery's grid as arithmetic: squares of about 130pt, three across a
/// phone and more in landscape and on iPad (where three would stretch the
/// 448px previews past sharp), 2pt apart. Every frame is computed from its
/// index, so a library of thousands costs the same as a screenful, and
/// inserting a page above moves the offset by whole rows.
final class StudioGalleryGridLayout: UICollectionViewLayout {
    static let spacing: CGFloat = 2

    private(set) var columns = 3
    private(set) var side: CGFloat = 0
    private var count = 0
    private var width: CGFloat = 0
    /// Empty slots before the oldest item: a page inserted above that isn't
    /// a whole number of rows leaves the squares below in their columns.
    private(set) var leadingBlanks = 0
    /// Rows a pending insert above adds: applied as the update's offset.
    private var pendingRowShift = 0

    var rowHeight: CGFloat { side + Self.spacing }

    override func prepare() {
        super.prepare()
        guard let collectionView else { return }
        let width = collectionView.bounds.width - collectionView.adjustedContentInset.left - collectionView.adjustedContentInset.right
        if width != self.width {
            self.width = width
            let columns = max(3, Int((width + Self.spacing) / 132))
            if columns != self.columns { leadingBlanks = 0 }
            self.columns = columns
            side = max(1, floor((width - Self.spacing * CGFloat(columns - 1)) / CGFloat(columns)))
        }
        count = collectionView.numberOfSections > 0 ? collectionView.numberOfItems(inSection: 0) : 0
    }

    /// `inserted` items are about to go in above the oldest.
    func prependItems(_ inserted: Int) {
        let blanks = ((leadingBlanks - inserted) % columns + columns) % columns
        pendingRowShift = (inserted + blanks - leadingBlanks) / columns
        leadingBlanks = blanks
    }

    /// Back to a plain grid (a reload).
    func resetLeadingBlanks() {
        leadingBlanks = 0
        pendingRowShift = 0
    }

    override var collectionViewContentSize: CGSize {
        let rows = count == 0 ? 0 : (count + leadingBlanks + columns - 1) / columns
        return CGSize(width: width, height: max(0, CGFloat(rows) * rowHeight - Self.spacing))
    }

    func frame(forItem index: Int) -> CGRect {
        let slot = index + leadingBlanks
        return CGRect(
            x: CGFloat(slot % columns) * (side + Self.spacing),
            y: CGFloat(slot / columns) * rowHeight,
            width: side, height: side
        )
    }

    /// The first item whose row reaches `y` or below it.
    func firstItem(atOrBelow y: CGFloat) -> Int {
        let row = max(0, Int(floor((y + Self.spacing) / max(rowHeight, 1))))
        return row * columns - leadingBlanks
    }

    override func layoutAttributesForElements(in rect: CGRect) -> [UICollectionViewLayoutAttributes]? {
        guard count > 0, rowHeight > 1 else { return [] }
        let firstRow = max(0, Int(floor(rect.minY / rowHeight)))
        let lastRow = Int(floor(rect.maxY / rowHeight))
        let first = max(0, firstRow * columns - leadingBlanks)
        let last = min(count - 1, (lastRow + 1) * columns - 1 - leadingBlanks)
        guard first <= last else { return [] }
        return (first...last).map { attributes(forItem: $0) }
    }

    override func layoutAttributesForItem(at indexPath: IndexPath) -> UICollectionViewLayoutAttributes? {
        guard indexPath.section == 0, indexPath.item >= 0, indexPath.item < count else { return nil }
        return attributes(forItem: indexPath.item)
    }

    private func attributes(forItem index: Int) -> UICollectionViewLayoutAttributes {
        let attributes = UICollectionViewLayoutAttributes(forCellWith: IndexPath(item: index, section: 0))
        attributes.frame = frame(forItem: index)
        return attributes
    }

    override func shouldInvalidateLayout(forBoundsChange newBounds: CGRect) -> Bool {
        guard let collectionView else { return false }
        return newBounds.width != collectionView.bounds.width
    }

    /// After an insert above, the offset moves down by the rows inserted, in
    /// the update itself: what's on screen never moves, and the cells for
    /// the inserted rows are never made.
    override func targetContentOffset(forProposedContentOffset proposedContentOffset: CGPoint) -> CGPoint {
        guard pendingRowShift != 0 else { return super.targetContentOffset(forProposedContentOffset: proposedContentOffset) }
        let shift = CGFloat(pendingRowShift) * rowHeight
        pendingRowShift = 0
        return CGPoint(x: proposedContentOffset.x, y: proposedContentOffset.y + shift)
    }

    override func finalizeCollectionViewUpdates() {
        super.finalizeCollectionViewUpdates()
        pendingRowShift = 0
    }
}

/// One gallery square: the cached preview, else the thumbhash (decoded
/// ahead, so it's there on the first frame), the relayed preview when it
/// lands, a duration badge on videos.
final class StudioGalleryCell: UICollectionViewCell {
    static let reuseIdentifier = "StudioGalleryCell"
    private static let photoGlyph = UIImage(systemName: "photo")
    private static let videoGlyph = UIImage(systemName: "film")

    let imageView = UIImageView()
    private let placeholderView = UIImageView()
    private let highlight = UIView()
    private let videoBadge = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterialDark))
    private let videoLabel = UILabel()
    private var item: StudioGalleryItem?
    /// The relayed preview is on screen (not the thumbhash standing in).
    private var showsPreview = false
    private weak var browser: StudioBrowserStore?
    private var requestActive = false
    private var loadTask: Task<Void, Never>?

    var displayedImage: UIImage? { imageView.image }
    var showsPreviewImage: Bool { showsPreview }

    override var isHighlighted: Bool {
        didSet { highlight.isHidden = !isHighlighted }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = Palette.chip
        contentView.clipsToBounds = true

        imageView.frame = contentView.bounds
        imageView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        imageView.contentMode = .scaleAspectFill
        imageView.clipsToBounds = true
        contentView.addSubview(imageView)

        placeholderView.frame = contentView.bounds
        placeholderView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        placeholderView.contentMode = .center
        placeholderView.tintColor = Palette.color(.textFaint).withAlphaComponent(0.45)
        placeholderView.isHidden = true
        contentView.addSubview(placeholderView)

        // A press dims the square (the image view itself is the zoom's source).
        highlight.frame = contentView.bounds
        highlight.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        highlight.backgroundColor = UIColor.black.withAlphaComponent(0.25)
        highlight.isHidden = true
        contentView.addSubview(highlight)

        videoBadge.layer.cornerRadius = 10
        videoBadge.clipsToBounds = true
        videoBadge.contentView.addSubview(videoLabel)
        videoLabel.font = Fonts.ui(.sansSemibold, 10)
        videoLabel.textColor = .white
        videoLabel.textAlignment = .center
        videoBadge.isHidden = true
        contentView.addSubview(videoBadge)

        isAccessibilityElement = true
        accessibilityTraits = [.button, .image]
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layoutSubviews() {
        super.layoutSubviews()
        guard !videoBadge.isHidden else { return }
        let badgeSize = videoLabel.sizeThatFits(CGSize(width: 90, height: 22))
        let width = max(34, ceil(badgeSize.width) + 14)
        videoBadge.frame = CGRect(x: contentView.bounds.width - width - 6, y: contentView.bounds.height - 26, width: width, height: 20)
        videoLabel.frame = videoBadge.bounds
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        cancelPreviewRequest()
        // The image stays until `configure` replaces it in the same pass:
        // clearing it here would only cost a second image swap.
        item = nil
        showsPreview = false
    }

    func configure(item: StudioGalleryItem, browser: StudioBrowserStore, service: StudioService, deviceId: String?) {
        let same = self.item?.id == item.id
        if !same {
            cancelPreviewRequest()
            showsPreview = false
        }
        self.browser = browser
        if self.item != item {
            self.item = item
            let video = item.mediaKind == .video
            let duration = video ? Self.duration(item.durationSeconds) : nil
            accessibilityLabel = [video ? "Video" : "Image", duration, item.prompt, item.modelDisplayName]
                .compactMap { $0?.isEmpty == false ? $0 : nil }.joined(separator: ", ")
            placeholderView.image = video ? Self.videoGlyph : Self.photoGlyph
            videoBadge.isHidden = !video
            if videoLabel.text != duration {
                videoLabel.text = duration
                // The badge fits its text ("Video" or "1:02:05").
                setNeedsLayout()
            }
        }

        // Configured again (willDisplay, a detail change): keep what's shown.
        if showsPreview { return }
        if let cached = browser.cachedPreview(artifactId: item.id) {
            show(cached, preview: true)
            return
        }
        if !same || imageView.image == nil {
            // Never blank: the thumbhash (or, lacking one, the glyph) now.
            show(browser.thumbhashImage(item.thumbhash, aspectRatio: item.aspectRatio), preview: false)
        }
        guard !requestActive, let deviceId else { return }
        requestActive = true
        let artifactId = item.id
        let request = browser.beginPreviewRequest(artifactId: artifactId, deviceId: deviceId, workspace: service)
        loadTask = Task { [weak self] in
            let image = await request.value
            guard !Task.isCancelled, let self, self.item?.id == artifactId else { return }
            // Landed (or failed): this square's hold ends with its read.
            self.cancelPreviewRequest()
            guard let image else { return }
            self.show(image, preview: true)
        }
    }

    private func show(_ image: UIImage?, preview: Bool) {
        if imageView.image !== image { imageView.image = image }
        placeholderView.isHidden = image != nil
        showsPreview = preview && image != nil
    }

    func cancelPreviewRequest() {
        loadTask?.cancel()
        loadTask = nil
        if requestActive, let item { browser?.endPreviewRequest(artifactId: item.id) }
        requestActive = false
    }

    /// "0:05", "1:02:05"; "Video" when the length isn't known yet.
    static func duration(_ seconds: Double?) -> String {
        guard let seconds, seconds.isFinite else { return "Video" }
        let total = max(0, Int(seconds.rounded()))
        let (hours, minutes) = (total / 3600, total / 60 % 60)
        return hours > 0
            ? String(format: "%d:%02d:%02d", hours, minutes, total % 60)
            : String(format: "%d:%02d", minutes, total % 60)
    }
}
