import UIKit

/// The Studio gallery: every creation on the selected desktop as a square
/// three-column grid, oldest at the top and opened at the newest (the
/// bottom). Scrolling near the top pages in older items; previews prefetch
/// ahead of the scroll. Answers the viewer's zoom transition for its cells.
final class StudioGalleryViewController: UIViewController,
    UICollectionViewDelegateFlowLayout,
    UICollectionViewDataSourcePrefetching,
    StudioGalleryTransitionSourceProvider
{
    private enum Section { case gallery }

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

        let layout = UICollectionViewFlowLayout()
        layout.minimumInteritemSpacing = 2
        layout.minimumLineSpacing = 2
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
                title: "Gallery unavailable", message: error, symbol: "exclamationmark.triangle"
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

    private func apply(_ newItems: [StudioGalleryItem]) {
        let oldIds = items.map(\.id)
        let newIds = newItems.map(\.id)
        guard oldIds != newIds else { return }

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

    /// The newest item at the bottom, once the grid has a size to place it in.
    private func positionInitialItemIfPossible() {
        guard !didSetInitialPosition, !positioningInitialItem, !items.isEmpty,
              let collectionView, collectionView.bounds.width > 0, collectionView.bounds.height > 0,
              dataSource.snapshot().numberOfItems == items.count else { return }
        positioningInitialItem = true
        collectionView.layoutIfNeeded()
        didSetInitialPosition = true
        collectionView.scrollToItem(at: IndexPath(item: items.count - 1, section: 0), at: .bottom, animated: false)
        positioningInitialItem = false
    }

    private func loadOlder() {
        guard let deviceId = browser.selectedDeviceId else { return }
        Task { await browser.loadMoreGallery(workspace: service, deviceId: deviceId) }
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

    func collectionView(_ collectionView: UICollectionView, layout collectionViewLayout: UICollectionViewLayout, sizeForItemAt indexPath: IndexPath) -> CGSize {
        let side = floor((collectionView.bounds.width - 4) / 3)
        return CGSize(width: side, height: side)
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
        endPrefetch(for: item.id)
        if let cell = cell as? StudioGalleryCell { configure(cell, with: item) }
        if indexPath.item < 12, requestedOldestId != items.first?.id {
            requestedOldestId = items.first?.id
            loadOlder()
        }
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

    private func download(_ artifact: StudioArtifactDetail) {
        guard let deviceId = browser.selectedDeviceId else { return }
        Task {
            do {
                try await StudioArtifactActions.download(artifact, workspace: service, deviceId: deviceId)
            } catch {
                showActionError(error.localizedDescription)
            }
        }
    }

    private func confirmDelete(_ artifact: StudioArtifactDetail) {
        let sheet = UIAlertController(
            title: "Delete this creation?",
            message: "This removes it from Studio on the selected machine.",
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
        let alert = UIAlertController(title: "Studio action failed", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
}

/// Reports size changes, so the gallery can open at its newest item once
/// it's been laid out.
private final class StudioGalleryCollectionView: UICollectionView {
    var layoutChanged: (() -> Void)?
    private var lastLayoutSize: CGSize = .zero

    override func layoutSubviews() {
        super.layoutSubviews()
        guard bounds.size != lastLayoutSize else { return }
        lastLayoutSize = bounds.size
        layoutChanged?()
    }
}

/// One gallery square: the cached preview or thumbhash at once, the relayed
/// preview when it lands, a duration badge on videos.
final class StudioGalleryCell: UICollectionViewCell {
    static let reuseIdentifier = "StudioGalleryCell"

    let imageView = UIImageView()
    private let placeholderView = UIImageView()
    private let videoBadge = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterialDark))
    private let videoLabel = UILabel()
    private var representedId: String?
    private var browser: StudioBrowserStore?
    private var requestActive = false
    private var loadTask: Task<Void, Never>?

    var displayedImage: UIImage? { imageView.image }

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

        videoBadge.layer.cornerRadius = 10
        videoBadge.clipsToBounds = true
        videoBadge.contentView.addSubview(videoLabel)
        videoLabel.font = .systemFont(ofSize: 10, weight: .semibold)
        videoLabel.textColor = .white
        videoLabel.textAlignment = .center
        contentView.addSubview(videoBadge)

        isAccessibilityElement = true
        accessibilityTraits = [.button, .image]
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layoutSubviews() {
        super.layoutSubviews()
        let badgeSize = videoLabel.sizeThatFits(CGSize(width: 90, height: 22))
        let width = max(34, badgeSize.width + 14)
        videoBadge.frame = CGRect(x: contentView.bounds.width - width - 6, y: contentView.bounds.height - 26, width: width, height: 20)
        videoLabel.frame = videoBadge.bounds
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        cancelPreviewRequest()
        representedId = nil
        imageView.image = nil
        placeholderView.image = nil
        placeholderView.isHidden = false
        videoBadge.isHidden = true
        accessibilityLabel = nil
    }

    func configure(item: StudioGalleryItem, browser: StudioBrowserStore, service: StudioService, deviceId: String?) {
        if representedId != item.id { cancelPreviewRequest() }
        representedId = item.id
        self.browser = browser
        accessibilityLabel = "\(item.modelDisplayName), \(item.prompt)"
        placeholderView.image = UIImage(systemName: item.mediaKind == .video ? "film" : "photo")
        videoBadge.isHidden = item.mediaKind != .video
        videoLabel.text = Self.duration(item.durationSeconds)

        if let cached = browser.cachedPreview(artifactId: item.id) {
            imageView.image = cached
            placeholderView.isHidden = true
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
            guard !Task.isCancelled, let self, self.representedId == artifactId else { return }
            if let image { self.imageView.image = image }
            self.placeholderView.isHidden = image != nil || thumbhash != nil
        }
    }

    func cancelPreviewRequest() {
        loadTask?.cancel()
        loadTask = nil
        if requestActive, let representedId { browser?.endPreviewRequest(artifactId: representedId) }
        requestActive = false
    }

    private static func duration(_ seconds: Double?) -> String {
        guard let seconds else { return "Video" }
        let total = max(0, Int(seconds.rounded()))
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}
