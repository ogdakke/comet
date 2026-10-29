import UIKit

/// The Studio gallery: every creation on the selected desktop as a grid of
/// squares (three columns on a phone, more as the width allows), oldest at
/// the top and opened at the newest (the bottom). Scrolling near the top
/// pages in older items; previews prefetch ahead of the scroll. Answers the
/// viewer's zoom transition for its cells.
final class StudioGalleryViewController: UIViewController,
    UICollectionViewDelegate,
    UICollectionViewDataSourcePrefetching,
    StudioGalleryTransitionSourceProvider
{
    private enum Section { case gallery }
    private static let spacing: CGFloat = 2

    private let browser: StudioBrowserStore
    private let service: StudioService
    private let transitionSource: StudioGalleryTransitionSource
    private let openViewer: (StudioViewerSession) -> Void
    private let showThread: (String) -> Void

    private var collectionView: StudioGalleryCollectionView!
    private var dataSource: UICollectionViewDiffableDataSource<Section, String>!
    private let loadingView = StudioLoadingView("Loading gallery…")
    private var items: [StudioGalleryItem] = []
    private var itemsById: [String: StudioGalleryItem] = [:]
    private var prefetchedIds: Set<String> = []
    private var requestedOldestId: String?
    private var didSetInitialPosition = false
    private var positioningInitialItem = false
    /// Columns in the current layout (the grid's width decides).
    private var columns = 3

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

        // Squares of about 130pt: three across a phone, more in landscape and
        // on iPad, where three would stretch the 448px previews past sharp.
        let layout = UICollectionViewCompositionalLayout { [weak self] _, environment in
            let width = environment.container.effectiveContentSize.width
            let columns = max(3, Int((width + Self.spacing) / 132))
            self?.columns = columns
            let side = floor((width - Self.spacing * CGFloat(columns - 1)) / CGFloat(columns))
            let item = NSCollectionLayoutItem(layoutSize: .init(widthDimension: .fractionalWidth(1), heightDimension: .fractionalHeight(1)))
            let group = NSCollectionLayoutGroup.horizontal(
                layoutSize: .init(widthDimension: .fractionalWidth(1), heightDimension: .absolute(side)),
                repeatingSubitem: item, count: columns
            )
            group.interItemSpacing = .fixed(Self.spacing)
            let section = NSCollectionLayoutSection(group: group)
            section.interGroupSpacing = Self.spacing
            return section
        }
        collectionView = StudioGalleryCollectionView(frame: view.bounds, collectionViewLayout: layout)
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.alwaysBounceVertical = true
        collectionView.contentInsetAdjustmentBehavior = .automatic
        collectionView.keyboardDismissMode = .onDrag
        collectionView.topEdgeEffect.style = .soft
        collectionView.delegate = self
        collectionView.prefetchDataSource = self
        collectionView.register(StudioGalleryCell.self, forCellWithReuseIdentifier: StudioGalleryCell.reuseIdentifier)
        collectionView.layoutChanged = { [weak self] in self?.positionInitialItemIfPossible() }
        view.addSubview(collectionView)

        dataSource = UICollectionViewDiffableDataSource(collectionView: collectionView) { [weak self] cv, path, id in
            guard let self, let item = self.itemsById[id],
                  let cell = cv.dequeueReusableCell(withReuseIdentifier: StudioGalleryCell.reuseIdentifier, for: path) as? StudioGalleryCell
            else { return UICollectionViewCell() }
            self.configure(cell, with: item)
            return cell
        }

        loadingView.frame = view.bounds
        loadingView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(loadingView)
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
        apply(gallery)
    }

    // MARK: Items

    private func apply(_ incoming: [StudioGalleryItem]) {
        // A repeated id would crash the snapshot (a page boundary shifting
        // under a merge): keep its first place.
        var seen = Set<String>()
        let newItems = incoming.filter { seen.insert($0.id).inserted }
        let oldIds = items.map(\.id)
        let newIds = newItems.map(\.id)
        guard oldIds != newIds else {
            // Same squares; an item's details may have landed since (its
            // thumbhash, a video's duration).
            let changed = newItems.filter { itemsById[$0.id] != $0 }.map(\.id)
            guard !changed.isEmpty else { return }
            items = newItems
            itemsById = Dictionary(uniqueKeysWithValues: newItems.map { ($0.id, $0) })
            var snapshot = dataSource.snapshot()
            snapshot.reconfigureItems(changed)
            dataSource.apply(snapshot, animatingDifferences: false)
            return
        }

        cancelAllPrefetching()
        let oldFirstId = oldIds.first
        let oldContentHeight = collectionView.contentSize.height
        let oldOffset = collectionView.contentOffset.y
        let wasNearBottom = oldContentHeight - collectionView.bounds.height - oldOffset < 100

        items = newItems
        itemsById = Dictionary(uniqueKeysWithValues: newItems.map { ($0.id, $0) })
        if newIds.first != oldFirstId { requestedOldestId = nil }
        // Emptied (another machine, a reload): open at the newest again.
        if newItems.isEmpty { didSetInitialPosition = false }

        var snapshot = NSDiffableDataSourceSnapshot<Section, String>()
        snapshot.appendSections([.gallery])
        snapshot.appendItems(newIds)
        dataSource.apply(snapshot, animatingDifferences: false) { [weak self] in
            guard let self, let collectionView = self.collectionView else { return }
            collectionView.layoutIfNeeded()
            if !self.didSetInitialPosition {
                self.positionInitialItemIfPossible()
            } else if let oldFirstId, let newIndex = newIds.firstIndex(of: oldFirstId), newIndex > 0 {
                // Older items landed above: hold what's on screen still.
                collectionView.contentOffset.y = oldOffset + collectionView.contentSize.height - oldContentHeight
            } else if wasNearBottom, newIds.count > oldIds.count, !newItems.isEmpty {
                collectionView.scrollToItem(at: IndexPath(item: newItems.count - 1, section: 0), at: .bottom, animated: false)
            }
        }
    }

    private func item(at indexPath: IndexPath) -> StudioGalleryItem? {
        items.indices.contains(indexPath.item) ? items[indexPath.item] : nil
    }

    private func configure(_ cell: StudioGalleryCell, with item: StudioGalleryItem) {
        cell.configure(item: item, browser: browser, service: service, deviceId: browser.selectedDeviceId)
    }

    /// The newest item at the bottom, once the grid is on screen with a size
    /// to place it in (off screen it has no safe area yet, and the newest
    /// row would land under the home indicator).
    private func positionInitialItemIfPossible() {
        guard !didSetInitialPosition, !positioningInitialItem, !items.isEmpty,
              let collectionView, collectionView.window != nil,
              collectionView.bounds.width > 0, collectionView.bounds.height > 0,
              dataSource.snapshot().numberOfItems == items.count else { return }
        positioningInitialItem = true
        collectionView.layoutIfNeeded()
        didSetInitialPosition = true
        collectionView.scrollToItem(at: IndexPath(item: items.count - 1, section: 0), at: .bottom, animated: false)
        collectionView.layoutIfNeeded()
        positioningInitialItem = false
        // Rows laid out at the top before the jump don't count; a gallery
        // short enough to show its top still pages in what's older.
        if collectionView.indexPathsForVisibleItems.contains(where: { $0.item < olderThreshold }) {
            loadOlderOnce()
        }
    }

    /// The first rows (about four) page in older items as they appear.
    private var olderThreshold: Int { columns * 4 }

    /// Once per oldest item: a failed page clears it, so scrolling back to
    /// the top tries again.
    private func loadOlderOnce() {
        let oldestId = items.first?.id
        guard requestedOldestId != oldestId, let deviceId = browser.selectedDeviceId else { return }
        requestedOldestId = oldestId
        Task { [weak self, browser, service] in
            await browser.loadMoreGallery(workspace: service, deviceId: deviceId)
            guard let self, self.items.first?.id == oldestId, self.requestedOldestId == oldestId else { return }
            self.requestedOldestId = nil
        }
    }

    // MARK: Transition source

    func transitionImageView(for artifactId: String) -> UIView? {
        guard let collectionView, let indexPath = dataSource?.indexPath(for: artifactId) else { return nil }
        // Never move or synchronously lay out a thousand-item gallery from
        // inside UIKit's transition callback. If paging moved away from a
        // visible source, returning nil gives UIKit a clean fade fallback.
        return (collectionView.cellForItem(at: indexPath) as? StudioGalleryCell)?.imageView
    }

    func prepareTransitionImageView(for artifactId: String) {
        guard let collectionView, let indexPath = dataSource?.indexPath(for: artifactId),
              collectionView.cellForItem(at: indexPath) == nil else { return }
        collectionView.scrollToItem(at: indexPath, at: .centeredVertically, animated: false)
        collectionView.layoutIfNeeded()
    }

    // MARK: Collection view

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
        endPrefetch(for: item.id)
        if let cell = cell as? StudioGalleryCell { configure(cell, with: item) }
        // Not while the first layout passes the top on its way to the newest.
        if didSetInitialPosition, !positioningInitialItem, indexPath.item < olderThreshold { loadOlderOnce() }
    }

    func collectionView(_ collectionView: UICollectionView, didEndDisplaying cell: UICollectionViewCell, forItemAt indexPath: IndexPath) {
        (cell as? StudioGalleryCell)?.cancelPreviewRequest()
    }

    func collectionView(_ collectionView: UICollectionView, prefetchItemsAt indexPaths: [IndexPath]) {
        guard let deviceId = browser.selectedDeviceId else { return }
        for indexPath in indexPaths {
            guard let item = item(at: indexPath), prefetchedIds.insert(item.id).inserted else { continue }
            _ = browser.beginPreviewRequest(artifactId: item.id, deviceId: deviceId, workspace: service)
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

    private func cancelAllPrefetching() {
        for id in prefetchedIds { browser.endPreviewRequest(artifactId: id) }
        prefetchedIds.removeAll()
    }

    func collectionView(_ collectionView: UICollectionView, contextMenuConfigurationForItemAt indexPath: IndexPath, point: CGPoint) -> UIContextMenuConfiguration? {
        guard let item = item(at: indexPath) else { return nil }
        return UIContextMenuConfiguration(identifier: item.id as NSString, previewProvider: nil) { [weak self] _ in
            UIMenu(children: [
                UIAction(title: "Download", image: UIImage(systemName: "square.and.arrow.down")) { _ in
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
            if let path = dataSource.indexPath(for: artifact.id), let cell = collectionView.cellForItem(at: path) {
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

/// One gallery square: the cached preview or thumbhash at once, the relayed
/// preview when it lands, a duration badge on videos.
final class StudioGalleryCell: UICollectionViewCell {
    static let reuseIdentifier = "StudioGalleryCell"

    let imageView = UIImageView()
    private let placeholderView = UIImageView()
    private let highlight = UIView()
    private let videoBadge = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterialDark))
    private let videoLabel = UILabel()
    private var representedId: String?
    /// The relayed preview is on screen (not the thumbhash standing in).
    private var showsPreview = false
    private var browser: StudioBrowserStore?
    private var requestActive = false
    private var loadTask: Task<Void, Never>?

    var displayedImage: UIImage? { imageView.image }

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
        let badgeSize = videoLabel.sizeThatFits(CGSize(width: 90, height: 22))
        let width = max(34, ceil(badgeSize.width) + 14)
        videoBadge.frame = CGRect(x: contentView.bounds.width - width - 6, y: contentView.bounds.height - 26, width: width, height: 20)
        videoLabel.frame = videoBadge.bounds
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        cancelPreviewRequest()
        representedId = nil
        showsPreview = false
        imageView.image = nil
        placeholderView.image = nil
        placeholderView.isHidden = false
        videoBadge.isHidden = true
        accessibilityLabel = nil
    }

    func configure(item: StudioGalleryItem, browser: StudioBrowserStore, service: StudioService, deviceId: String?) {
        if representedId != item.id {
            cancelPreviewRequest()
            showsPreview = false
        }
        representedId = item.id
        self.browser = browser
        let video = item.mediaKind == .video
        let duration = video ? Self.duration(item.durationSeconds) : nil
        accessibilityLabel = [video ? "Video" : "Image", duration, item.prompt, item.modelDisplayName]
            .compactMap { $0?.isEmpty == false ? $0 : nil }.joined(separator: ", ")
        placeholderView.image = UIImage(systemName: video ? "film" : "photo")
        videoBadge.isHidden = !video
        if videoLabel.text != duration {
            videoLabel.text = duration
            // The badge fits its text ("Video" or "1:02:05").
            setNeedsLayout()
        }

        // Configured again (willDisplay, a reconfigure): keep what's shown.
        if showsPreview { return }
        if let cached = browser.cachedPreview(artifactId: item.id) {
            imageView.image = cached
            placeholderView.isHidden = true
            showsPreview = true
            return
        }
        let thumbhash = browser.thumbhashImage(item.thumbhash, aspectRatio: item.aspectRatio)
        imageView.image = thumbhash
        placeholderView.isHidden = thumbhash != nil
        guard !requestActive, let deviceId else { return }
        requestActive = true
        let artifactId = item.id
        let request = browser.beginPreviewRequest(artifactId: artifactId, deviceId: deviceId, workspace: service)
        loadTask = Task { [weak self] in
            let image = await request.value
            guard !Task.isCancelled, let self, self.representedId == artifactId, let image else { return }
            self.imageView.image = image
            self.placeholderView.isHidden = true
            self.showsPreview = true
        }
    }

    func cancelPreviewRequest() {
        loadTask?.cancel()
        loadTask = nil
        if requestActive, let representedId { browser?.endPreviewRequest(artifactId: representedId) }
        requestActive = false
    }

    private static func duration(_ seconds: Double?) -> String {
        guard let seconds, seconds.isFinite else { return "Video" }
        let total = max(0, Int(seconds.rounded()))
        let (hours, minutes) = (total / 3600, total / 60 % 60)
        return hours > 0
            ? String(format: "%d:%02d:%02d", hours, minutes, total % 60)
            : String(format: "%d:%02d", minutes, total % 60)
    }
}
