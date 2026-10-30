import AVKit
import UIKit

@MainActor
protocol StudioViewerPagerDelegate: AnyObject {
    func viewerPager(_ pager: StudioViewerPagerController, settledOn artifactId: String)
    /// A single tap on an image page (chrome toggles).
    func viewerPagerDidTapPage(_ pager: StudioViewerPagerController)
}

/// How far along a page's original is. Pages show their preview meanwhile.
enum StudioViewerOriginalState {
    case ready
    case loading
    case failed
}

/// A deliberately small, reusable pager. Only visible cells exist, and page
/// selection is published after UIKit finishes decelerating. This keeps a
/// horizontal fling independent from media loading and the filmstrip.
@MainActor
final class StudioViewerPagerController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate,
    UIScrollViewAccessibilityDelegate
{
    weak var delegate: StudioViewerPagerDelegate?

    /// Room the viewer's chrome takes beyond the safe area, so AVKit's
    /// transport controls sit clear of the filmstrip and buttons.
    var playerChromeInsets: UIEdgeInsets = .zero {
        didSet {
            guard playerChromeInsets != oldValue, isViewLoaded else { return }
            for case let cell as StudioViewerPageCell in collectionView.visibleCells {
                cell.playerChromeInsets = playerChromeInsets
            }
        }
    }

    private let media: StudioViewerMediaStore
    private let layout = UICollectionViewFlowLayout()
    private lazy var collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
    private var artifacts: [StudioArtifactDetail]
    private var artifactIndex: [String: Int]
    private var selectedId: String
    private var pendingSelectionId: String?
    private var lastSize: CGSize = .zero

    init(
        artifacts: [StudioArtifactDetail],
        selectedId: String,
        media: StudioViewerMediaStore
    ) {
        self.artifacts = artifacts
        self.selectedId = selectedId
        self.media = media
        artifactIndex = Self.index(artifacts)
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear

        layout.scrollDirection = .horizontal
        layout.minimumLineSpacing = 0
        layout.minimumInteritemSpacing = 0

        collectionView.frame = view.bounds
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.isPagingEnabled = true
        collectionView.isDirectionalLockEnabled = true
        collectionView.showsHorizontalScrollIndicator = false
        collectionView.showsVerticalScrollIndicator = false
        collectionView.contentInsetAdjustmentBehavior = .never
        collectionView.decelerationRate = .fast
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.register(
            StudioViewerPageCell.self,
            forCellWithReuseIdentifier: StudioViewerPageCell.reuseIdentifier
        )
        view.addSubview(collectionView)
        pendingSelectionId = selectedId
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        guard view.bounds.size != lastSize else { return }
        lastSize = view.bounds.size
        // A new page size (rotation, iPad resize) moves every page's offset;
        // stay on the selected one instead of whatever the old offset hits.
        if pendingSelectionId == nil { pendingSelectionId = selectedId }
        layout.itemSize = view.bounds.size
        layout.invalidateLayout()
        collectionView.layoutIfNeeded()
        positionPendingSelection()
    }

    /// Swap in a new list (older gallery pages, a deletion), staying on
    /// `selecting` or the current page.
    func replaceArtifacts(_ artifacts: [StudioArtifactDetail], selecting: String? = nil) {
        let keepSelected = selecting ?? selectedId
        guard self.artifacts != artifacts || keepSelected != selectedId else { return }
        let anchor = selecting == nil ? StudioStripAnchor.capture(collectionView, ids: self.artifacts) : nil
        self.artifacts = artifacts
        artifactIndex = Self.index(artifacts)
        selectedId = keepSelected
        collectionView.reloadData()
        collectionView.layoutIfNeeded()
        if let anchor {
            StudioStripAnchor.restore(anchor, in: collectionView, index: artifactIndex[anchor.id])
            return
        }
        pendingSelectionId = keepSelected
        positionPendingSelection()
    }

    func select(_ artifactId: String, animated: Bool) {
        guard let index = artifactIndex[artifactId] else { return }
        selectedId = artifactId
        guard isViewLoaded, collectionView.bounds.width > 0 else {
            pendingSelectionId = artifactId
            return
        }
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: animated
        )
        if !animated { refreshVisibleCells() }
    }

    func refresh(_ artifactId: String) {
        guard isViewLoaded else { return }
        for case let cell as StudioViewerPageCell in collectionView.visibleCells
        where cell.representedId == artifactId {
            configure(cell, artifactId: artifactId)
        }
    }

    /// The selected page's media rect, or nil before its cell exists.
    func transitionImageFrame(in coordinateView: UIView) -> CGRect? {
        guard let frame = currentCell?.visibleMediaFrame(in: coordinateView),
              frame.width > 1, frame.height > 1 else { return nil }
        return frame
    }

    var isCurrentPageZoomed: Bool { currentCell?.isZoomed ?? false }

    /// The paging drag, so the viewer can order it behind its dismiss pan.
    var pagingPanGesture: UIPanGestureRecognizer {
        loadViewIfNeeded()
        return collectionView.panGestureRecognizer
    }

    /// The selected image page's image view, for the context menu's lift.
    var currentImageView: UIView? { currentCell?.liftableImageView }

    private var currentCell: StudioViewerPageCell? {
        guard isViewLoaded, let index = artifactIndex[selectedId] else { return nil }
        return collectionView.cellForItem(at: IndexPath(item: index, section: 0)) as? StudioViewerPageCell
    }

    func collectionView(
        _ collectionView: UICollectionView,
        numberOfItemsInSection section: Int
    ) -> Int {
        artifacts.count
    }

    func collectionView(
        _ collectionView: UICollectionView,
        cellForItemAt indexPath: IndexPath
    ) -> UICollectionViewCell {
        guard let cell = collectionView.dequeueReusableCell(
            withReuseIdentifier: StudioViewerPageCell.reuseIdentifier,
            for: indexPath
        ) as? StudioViewerPageCell,
        artifacts.indices.contains(indexPath.item) else {
            return UICollectionViewCell()
        }
        configure(cell, artifactId: artifacts[indexPath.item].id)
        return cell
    }

    func collectionView(
        _ collectionView: UICollectionView,
        didEndDisplaying cell: UICollectionViewCell,
        forItemAt indexPath: IndexPath
    ) {
        guard let cell = cell as? StudioViewerPageCell else { return }
        cell.detachPlayer()
        cell.resetZoom()
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { settleSelection() }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) {
        settleSelection()
    }

    func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        // VoiceOver's three-finger page scroll lands here too.
        settleSelection(notify: false)
    }

    func accessibilityScrollStatus(for scrollView: UIScrollView) -> String? {
        guard scrollView.bounds.width > 0, !artifacts.isEmpty else { return nil }
        let index = Int(round(scrollView.contentOffset.x / scrollView.bounds.width))
        guard artifacts.indices.contains(index) else { return nil }
        return "\(index + 1) of \(artifacts.count)"
    }

    private func positionPendingSelection() {
        guard collectionView.bounds.width > 0,
              let id = pendingSelectionId,
              let index = artifactIndex[id] else { return }
        pendingSelectionId = nil
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: false
        )
        refreshVisibleCells()
    }

    private func settleSelection(notify: Bool = true) {
        guard collectionView.bounds.width > 0, !artifacts.isEmpty else { return }
        let index = Int(round(collectionView.contentOffset.x / collectionView.bounds.width))
        guard artifacts.indices.contains(index) else { return }
        let id = artifacts[index].id
        let changed = id != selectedId
        selectedId = id
        if notify || changed { delegate?.viewerPager(self, settledOn: id) }
    }

    private func refreshVisibleCells() {
        for case let cell as StudioViewerPageCell in collectionView.visibleCells {
            guard let indexPath = collectionView.indexPath(for: cell),
                  artifacts.indices.contains(indexPath.item) else { continue }
            configure(cell, artifactId: artifacts[indexPath.item].id)
        }
    }

    private func configure(_ cell: StudioViewerPageCell, artifactId: String) {
        guard let index = artifactIndex[artifactId] else { return }
        let artifact = artifacts[index]
        cell.playerChromeInsets = playerChromeInsets
        cell.onTap = { [weak self] in
            guard let self else { return }
            self.delegate?.viewerPagerDidTapPage(self)
        }
        cell.onRetry = { [weak media] in media?.retryOriginal(artifactId) }
        cell.configure(
            artifact: artifact,
            image: media.displayImage(for: artifact),
            originalState: media.originalState(for: artifact),
            player: media.player(for: artifactId),
            parentViewController: self
        )
    }

    private static func index(_ artifacts: [StudioArtifactDetail]) -> [String: Int] {
        Dictionary(artifacts.indices.map { (artifacts[$0].id, $0) }, uniquingKeysWith: { first, _ in first })
    }

#if DEBUG
    var residentCellCountForTesting: Int { collectionView.visibleCells.count }
#endif
}

@MainActor
final class StudioViewerPageCell: UICollectionViewCell, UIScrollViewDelegate {
    static let reuseIdentifier = "StudioViewerPageCell"

    var onTap: (() -> Void)?
    var onRetry: (() -> Void)?
    var playerChromeInsets: UIEdgeInsets = .zero {
        didSet { playerController?.additionalSafeAreaInsets = playerChromeInsets }
    }

    private let imageScrollView = UIScrollView()
    private let imageView = UIImageView()
    private let placeholderView = UIImageView()
    private let loadingBadge = Glass.surface()
    private let spinner = UIActivityIndicatorView(style: .medium)
    private let retryButton = UIButton(type: .system)
    private var playerController: AVPlayerViewController?
    private var mediaAspectRatio: CGFloat = 1
    private var lastScrollSize: CGSize = .zero
    private var isVideo = false
    private var showsLoading = false
    private(set) var representedId: String?

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = .clear

        imageScrollView.frame = contentView.bounds
        imageScrollView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        imageScrollView.minimumZoomScale = 1
        imageScrollView.maximumZoomScale = 6
        imageScrollView.bouncesZoom = true
        imageScrollView.showsHorizontalScrollIndicator = false
        imageScrollView.showsVerticalScrollIndicator = false
        // Insets here center the image; the device's safe area isn't ours to add.
        imageScrollView.contentInsetAdjustmentBehavior = .never
        imageScrollView.decelerationRate = .fast
        imageScrollView.delegate = self
        imageScrollView.panGestureRecognizer.isEnabled = false
        contentView.addSubview(imageScrollView)

        // Sized to the image itself (not the page), so a zoomed image pans
        // to its own edges rather than into letterbox bars.
        imageView.contentMode = .scaleAspectFit
        imageView.clipsToBounds = true
        imageView.isAccessibilityElement = true
        imageView.accessibilityTraits = .image
        imageScrollView.addSubview(imageView)

        placeholderView.frame = contentView.bounds
        placeholderView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        placeholderView.contentMode = .center
        placeholderView.preferredSymbolConfiguration = UIImage.SymbolConfiguration(pointSize: 28, weight: .light)
        placeholderView.tintColor = Palette.color(.textFaint).withAlphaComponent(0.55)
        placeholderView.isUserInteractionEnabled = false
        contentView.addSubview(placeholderView)

        spinner.color = .white
        spinner.translatesAutoresizingMaskIntoConstraints = false
        loadingBadge.contentView.addSubview(spinner)
        loadingBadge.isUserInteractionEnabled = false
        loadingBadge.isHidden = true
        loadingBadge.accessibilityLabel = "Loading full resolution"
        loadingBadge.isAccessibilityElement = true
        contentView.addSubview(loadingBadge)
        NSLayoutConstraint.activate([
            spinner.centerXAnchor.constraint(equalTo: loadingBadge.contentView.centerXAnchor),
            spinner.centerYAnchor.constraint(equalTo: loadingBadge.contentView.centerYAnchor),
        ])

        var retry = UIButton.Configuration.glass()
        retry.image = UIImage(systemName: "arrow.clockwise", withConfiguration: UIImage.SymbolConfiguration(pointSize: 14, weight: .semibold))
        retry.imagePadding = 8
        retry.baseForegroundColor = .white
        retry.cornerStyle = .capsule
        retry.contentInsets = NSDirectionalEdgeInsets(top: 12, leading: 18, bottom: 12, trailing: 18)
        retry.attributedTitle = AttributedString(
            "Couldn't load · Retry",
            attributes: AttributeContainer([.font: Fonts.ui(.sansMedium, 15)])
        )
        retryButton.configuration = retry
        retryButton.isHidden = true
        retryButton.accessibilityLabel = "Couldn't load the full resolution. Retry"
        retryButton.addAction(UIAction { [weak self] _ in self?.onRetry?() }, for: .touchUpInside)
        contentView.addSubview(retryButton)

        let doubleTap = UITapGestureRecognizer(target: self, action: #selector(handleDoubleTap(_:)))
        doubleTap.numberOfTapsRequired = 2
        imageScrollView.addGestureRecognizer(doubleTap)
        let singleTap = UITapGestureRecognizer(target: self, action: #selector(handleSingleTap))
        singleTap.require(toFail: doubleTap)
        imageScrollView.addGestureRecognizer(singleTap)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func prepareForReuse() {
        super.prepareForReuse()
        detachPlayer()
        representedId = nil
        onTap = nil
        onRetry = nil
        imageView.image = nil
        placeholderView.image = nil
        placeholderView.isHidden = false
        setOriginalState(.ready)
        resetZoom()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if imageScrollView.bounds.size != lastScrollSize {
            lastScrollSize = imageScrollView.bounds.size
            imageScrollView.setZoomScale(1, animated: false)
            layoutImage()
        }
        let badge: CGFloat = 44
        loadingBadge.frame = CGRect(
            x: (contentView.bounds.width - badge) / 2,
            y: (contentView.bounds.height - badge) / 2,
            width: badge,
            height: badge
        )
        let retrySize = retryButton.sizeThatFits(contentView.bounds.size)
        retryButton.frame = CGRect(
            x: (contentView.bounds.width - retrySize.width) / 2,
            y: (contentView.bounds.height - retrySize.height) / 2,
            width: retrySize.width,
            height: max(44, retrySize.height)
        )
    }

    func configure(
        artifact: StudioArtifactDetail,
        image: UIImage?,
        originalState: StudioViewerOriginalState,
        player: AVPlayer?,
        parentViewController: UIViewController
    ) {
        if representedId != artifact.id {
            detachPlayer()
            representedId = artifact.id
            resetZoom()
        }
        isVideo = artifact.mediaKind == .video
        placeholderView.image = UIImage(systemName: isVideo ? "film" : "photo")
        let kind = isVideo ? "Video" : "Image"
        imageView.accessibilityLabel = artifact.prompt.isEmpty ? kind : "\(kind), \(artifact.prompt)"
        // A video's preview is a poster, not something to zoom into.
        imageScrollView.maximumZoomScale = isVideo ? 1 : 6

        // Metadata dimensions are what the thumbnail zooms from; the decoded
        // image stands in when a provider didn't report them.
        let ratio: CGFloat
        if artifact.width != nil, artifact.height != nil {
            ratio = artifact.aspectRatio
        } else if let image, image.size.height > 0 {
            ratio = image.size.width / image.size.height
        } else {
            // The thumbhash's recorded shape, so nothing reshapes from square.
            ratio = artifact.aspectRatio
        }
        if abs(max(0.01, ratio) - mediaAspectRatio) > 0.001 {
            mediaAspectRatio = max(0.01, ratio)
            resetZoom()
            layoutImage()
        }

        // Replacing a decoded preview with the display image in-place avoids a
        // second composited image during a page or presentation transition.
        imageView.image = image
        placeholderView.isHidden = image != nil

        if isVideo, let player {
            // The preview stays underneath until the first frame draws.
            setOriginalState(.ready)
            imageScrollView.isUserInteractionEnabled = false
            attachPlayer(player, parent: parentViewController)
            return
        }
        detachPlayer()
        imageScrollView.isUserInteractionEnabled = true
        setOriginalState(isVideo ? .ready : originalState)
    }

    func detachPlayer() {
        guard let playerController else { return }
        playerController.willMove(toParent: nil)
        playerController.view.removeFromSuperview()
        playerController.removeFromParent()
        self.playerController = nil
    }

    func resetZoom() {
        guard isZoomed else { return }
        imageScrollView.setZoomScale(1, animated: false)
        layoutImage()
    }

    func visibleMediaFrame(in coordinateView: UIView) -> CGRect {
        if imageView.bounds.isEmpty { layoutImage() }
        return imageView.convert(imageView.bounds, to: coordinateView)
    }

    var liftableImageView: UIView? {
        isVideo || imageView.image == nil || isZoomed ? nil : imageView
    }

    var isZoomed: Bool { imageScrollView.zoomScale > 1.001 }

    func viewForZooming(in scrollView: UIScrollView) -> UIView? { imageView }

    func scrollViewDidZoom(_ scrollView: UIScrollView) {
        scrollView.panGestureRecognizer.isEnabled = scrollView.zoomScale > 1.001
        centerImage()
    }

    private func attachPlayer(_ player: AVPlayer, parent: UIViewController) {
        guard playerController?.player !== player else { return }
        detachPlayer()
        let controller = AVPlayerViewController()
        controller.player = player
        controller.showsPlaybackControls = true
        controller.videoGravity = .resizeAspect
        // Streams end with the viewer; picture in picture would outlive it.
        controller.allowsPictureInPicturePlayback = false
        controller.view.backgroundColor = .clear
        controller.additionalSafeAreaInsets = playerChromeInsets
        parent.addChild(controller)
        controller.view.frame = contentView.bounds
        controller.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        contentView.insertSubview(controller.view, aboveSubview: placeholderView)
        controller.didMove(toParent: parent)
        playerController = controller
    }

    private func setOriginalState(_ state: StudioViewerOriginalState) {
        retryButton.isHidden = state != .failed
        let loading = state == .loading
        guard loading != showsLoading else { return }
        showsLoading = loading
        loadingBadge.layer.removeAllAnimations()
        if loading {
            spinner.startAnimating()
            loadingBadge.isHidden = false
            loadingBadge.alpha = 0
            // Cached and quick originals land before this shows, so they
            // never flash a spinner.
            UIView.animate(withDuration: 0.25, delay: 0.6, options: [.allowUserInteraction]) {
                self.loadingBadge.alpha = 1
            }
        } else {
            spinner.stopAnimating()
            loadingBadge.isHidden = true
            loadingBadge.alpha = 0
        }
    }

    /// Fit the image view to the media's aspect ratio inside the page.
    private func layoutImage() {
        let bounds = imageScrollView.bounds.size
        guard bounds.width > 0, bounds.height > 0, !isZoomed else { return }
        let fitted = AVMakeRect(
            aspectRatio: CGSize(width: mediaAspectRatio, height: 1),
            insideRect: CGRect(origin: .zero, size: bounds)
        ).size
        let size = CGSize(width: fitted.width.rounded(), height: fitted.height.rounded())
        imageView.frame = CGRect(origin: .zero, size: size)
        imageScrollView.contentSize = size
        centerImage()
        imageScrollView.contentOffset = CGPoint(
            x: -imageScrollView.contentInset.left,
            y: -imageScrollView.contentInset.top
        )
    }

    private func centerImage() {
        let bounds = imageScrollView.bounds.size
        let content = imageScrollView.contentSize
        let x = max(0, (bounds.width - content.width) / 2)
        let y = max(0, (bounds.height - content.height) / 2)
        imageScrollView.contentInset = UIEdgeInsets(top: y, left: x, bottom: y, right: x)
    }

    @objc private func handleSingleTap() {
        onTap?()
    }

    @objc private func handleDoubleTap(_ gesture: UITapGestureRecognizer) {
        if isZoomed {
            imageScrollView.setZoomScale(1, animated: true)
            return
        }
        let bounds = imageScrollView.bounds.size
        let fitted = imageView.bounds.size
        guard fitted.width > 0, fitted.height > 0 else { return }
        // Fill the screen when that's a real zoom (a wide image on a tall
        // phone), otherwise a fixed step in.
        let fill = max(bounds.width / fitted.width, bounds.height / fitted.height)
        let scale = min(imageScrollView.maximumZoomScale, max(2.5, fill))
        imageScrollView.panGestureRecognizer.isEnabled = true
        let point = gesture.location(in: imageView)
        let size = CGSize(width: bounds.width / scale, height: bounds.height / scale)
        imageScrollView.zoom(to: CGRect(
            x: point.x - size.width / 2,
            y: point.y - size.height / 2,
            width: size.width,
            height: size.height
        ), animated: true)
    }
}
