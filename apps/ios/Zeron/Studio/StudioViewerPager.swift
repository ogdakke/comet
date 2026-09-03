import AVKit
import UIKit

@MainActor
protocol StudioViewerPagerDelegate: AnyObject {
    func viewerPager(_ pager: StudioViewerPagerController, settledOn artifactId: String)
}

/// A deliberately small, reusable pager. Only visible cells exist, and page
/// selection is published after UIKit finishes decelerating. This keeps a
/// horizontal fling independent from media loading and the filmstrip.
@MainActor
final class StudioViewerPagerController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate
{
    weak var delegate: StudioViewerPagerDelegate?

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
        artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
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
        layout.itemSize = view.bounds.size
        layout.invalidateLayout()
        collectionView.layoutIfNeeded()
        positionPendingSelection()
    }

    func replaceArtifacts(_ artifacts: [StudioArtifactDetail]) {
        guard self.artifacts != artifacts else { return }
        let keepSelected = selectedId
        self.artifacts = artifacts
        artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
        collectionView.reloadData()
        pendingSelectionId = keepSelected
        collectionView.layoutIfNeeded()
        positionPendingSelection()
    }

    func select(_ artifactId: String, animated: Bool) {
        guard artifactIndex[artifactId] != nil else { return }
        selectedId = artifactId
        guard isViewLoaded, collectionView.bounds.width > 0 else {
            pendingSelectionId = artifactId
            return
        }
        guard let index = artifactIndex[artifactId] else { return }
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

    func transitionImageFrame(in coordinateView: UIView) -> CGRect {
        guard let index = artifactIndex[selectedId],
              let cell = collectionView.cellForItem(
                at: IndexPath(item: index, section: 0)
              ) as? StudioViewerPageCell else {
            return coordinateView.bounds
        }
        return cell.visibleMediaFrame(in: coordinateView)
    }

    var isCurrentPageZoomed: Bool {
        guard let index = artifactIndex[selectedId],
              let cell = collectionView.cellForItem(
                at: IndexPath(item: index, section: 0)
              ) as? StudioViewerPageCell else { return false }
        return cell.isZoomed
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
        (cell as? StudioViewerPageCell)?.detachPlayer()
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { settleSelection() }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) {
        settleSelection()
    }

    func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        settleSelection(notify: false)
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
        cell.configure(
            artifact: artifact,
            image: media.displayImage(for: artifact),
            player: media.player(for: artifactId),
            parentViewController: self
        )
    }

#if DEBUG
    var residentCellCountForTesting: Int { collectionView.visibleCells.count }
#endif
}

@MainActor
final class StudioViewerPageCell: UICollectionViewCell, UIScrollViewDelegate {
    static let reuseIdentifier = "StudioViewerPageCell"

    private let imageScrollView = UIScrollView()
    private let imageView = UIImageView()
    private let placeholderView = UIImageView()
    private var playerController: AVPlayerViewController?
    private var artifactAspectRatio: CGFloat = 1
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
        imageScrollView.delegate = self
        imageScrollView.panGestureRecognizer.isEnabled = false
        contentView.addSubview(imageScrollView)

        imageView.frame = imageScrollView.bounds
        imageView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        imageView.contentMode = .scaleAspectFit
        imageView.clipsToBounds = true
        imageScrollView.addSubview(imageView)

        placeholderView.frame = contentView.bounds
        placeholderView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        placeholderView.contentMode = .center
        placeholderView.tintColor = UIColor(Theme.textFaint.opacity(0.55))
        contentView.addSubview(placeholderView)

        let doubleTap = UITapGestureRecognizer(target: self, action: #selector(handleDoubleTap(_:)))
        doubleTap.numberOfTapsRequired = 2
        imageScrollView.addGestureRecognizer(doubleTap)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func prepareForReuse() {
        super.prepareForReuse()
        detachPlayer()
        representedId = nil
        imageView.image = nil
        placeholderView.image = nil
        placeholderView.isHidden = false
        imageScrollView.setZoomScale(1, animated: false)
    }

    func configure(
        artifact: StudioArtifactDetail,
        image: UIImage?,
        player: AVPlayer?,
        parentViewController: UIViewController
    ) {
        let identityChanged = representedId != artifact.id
        if identityChanged {
            detachPlayer()
            representedId = artifact.id
            artifactAspectRatio = max(0.01, artifact.aspectRatio)
            imageScrollView.setZoomScale(1, animated: false)
        }
        placeholderView.image = UIImage(systemName: artifact.mediaKind == .video ? "film" : "photo")

        if artifact.mediaKind == .video, let player {
            imageScrollView.isHidden = true
            placeholderView.isHidden = true
            if playerController?.player !== player {
                detachPlayer()
                let controller = AVPlayerViewController()
                controller.player = player
                controller.showsPlaybackControls = true
                controller.videoGravity = .resizeAspect
                controller.view.backgroundColor = .clear
                parentViewController.addChild(controller)
                controller.view.frame = contentView.bounds
                controller.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
                contentView.addSubview(controller.view)
                controller.didMove(toParent: parentViewController)
                playerController = controller
            }
            return
        }

        detachPlayer()
        imageScrollView.isHidden = false
        // Replacing a decoded preview with the display image in-place avoids a
        // second composited image during a page or presentation transition.
        imageView.image = image
        placeholderView.isHidden = image != nil
    }

    func detachPlayer() {
        guard let playerController else { return }
        playerController.willMove(toParent: nil)
        playerController.view.removeFromSuperview()
        playerController.removeFromParent()
        self.playerController = nil
    }

    func visibleMediaFrame(in coordinateView: UIView) -> CGRect {
        let bounds = imageView.bounds
        let containerAspect = bounds.width / max(bounds.height, 1)
        let frame: CGRect
        if artifactAspectRatio > containerAspect {
            let height = bounds.width / artifactAspectRatio
            frame = CGRect(x: 0, y: (bounds.height - height) / 2, width: bounds.width, height: height)
        } else {
            let width = bounds.height * artifactAspectRatio
            frame = CGRect(x: (bounds.width - width) / 2, y: 0, width: width, height: bounds.height)
        }
        return imageView.convert(frame, to: coordinateView)
    }

    func viewForZooming(in scrollView: UIScrollView) -> UIView? { imageView }

    var isZoomed: Bool { imageScrollView.zoomScale > 1.001 }

    func scrollViewDidZoom(_ scrollView: UIScrollView) {
        scrollView.panGestureRecognizer.isEnabled = scrollView.zoomScale > 1.001
    }

    @objc private func handleDoubleTap(_ gesture: UITapGestureRecognizer) {
        if imageScrollView.zoomScale > 1.001 {
            imageScrollView.setZoomScale(1, animated: true)
            return
        }
        imageScrollView.panGestureRecognizer.isEnabled = true
        let point = gesture.location(in: imageView)
        let size = CGSize(
            width: imageScrollView.bounds.width / 3,
            height: imageScrollView.bounds.height / 3
        )
        imageScrollView.zoom(to: CGRect(
            x: point.x - size.width / 2,
            y: point.y - size.height / 2,
            width: size.width,
            height: size.height
        ), animated: true)
    }
}
