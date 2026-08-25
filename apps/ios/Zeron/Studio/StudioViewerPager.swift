import AVKit
import SwiftUI
import UIKit

struct StudioViewerPager: UIViewControllerRepresentable {
    let artifacts: [StudioArtifactDetail]
    let artifactRevision: Int
    let selectedId: String
    let previews: [String: UIImage]
    let images: [String: UIImage]
    let players: [String: AVPlayer]
    let openingPreview: UIImage?
    let openingPreviewArtifactId: String
    let browser: StudioBrowserStore
    let selectionChanged: (String) -> Void
    let selectionSettled: (String) -> Void

    func makeUIViewController(context: Context) -> StudioViewerPagerController {
        let controller = StudioViewerPagerController()
        controller.update(with: self)
        return controller
    }

    func updateUIViewController(_ controller: StudioViewerPagerController, context: Context) {
        controller.update(with: self)
    }
}

@MainActor
final class StudioViewerPagerController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate,
    UICollectionViewDataSourcePrefetching
{
    private let layout = UICollectionViewFlowLayout()
    private lazy var collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
    private var configuration: StudioViewerPager?
    private var artifacts: [StudioArtifactDetail] = []
    private var artifactIndex: [String: Int] = [:]
    private var artifactRevision = -1
    private var pendingSelectionId: String?
    private var userInteracting = false
    private var lastSize: CGSize = .zero

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
        collectionView.showsHorizontalScrollIndicator = false
        collectionView.showsVerticalScrollIndicator = false
        collectionView.contentInsetAdjustmentBehavior = .never
        collectionView.decelerationRate = .fast
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.prefetchDataSource = self
        collectionView.register(
            StudioViewerPageCell.self,
            forCellWithReuseIdentifier: StudioViewerPageCell.reuseIdentifier
        )
        view.addSubview(collectionView)
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        guard view.bounds.size != lastSize else { return }
        lastSize = view.bounds.size
        layout.itemSize = view.bounds.size
        layout.invalidateLayout()
        positionPendingSelection(animated: false)
    }

    func update(with configuration: StudioViewerPager) {
        loadViewIfNeeded()
        self.configuration = configuration
        if artifactRevision != configuration.artifactRevision {
            artifactRevision = configuration.artifactRevision
            artifacts = configuration.artifacts
            artifactIndex = Dictionary(uniqueKeysWithValues: artifacts.indices.map { (artifacts[$0].id, $0) })
            collectionView.reloadData()
            pendingSelectionId = configuration.selectedId
            positionPendingSelection(animated: false)
        } else if !userInteracting, currentArtifactId() != configuration.selectedId {
            pendingSelectionId = configuration.selectedId
            positionPendingSelection(animated: false)
        }
        updateVisibleCells()
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
        configure(cell, artifact: artifacts[indexPath.item])
        return cell
    }

    func collectionView(
        _ collectionView: UICollectionView,
        didEndDisplaying cell: UICollectionViewCell,
        forItemAt indexPath: IndexPath
    ) {
        (cell as? StudioViewerPageCell)?.detachPlayer()
    }

    func collectionView(
        _ collectionView: UICollectionView,
        willDisplay cell: UICollectionViewCell,
        forItemAt indexPath: IndexPath
    ) {
        guard let cell = cell as? StudioViewerPageCell,
              artifacts.indices.contains(indexPath.item) else { return }
        configure(cell, artifact: artifacts[indexPath.item])
    }

    func collectionView(_ collectionView: UICollectionView, prefetchItemsAt indexPaths: [IndexPath]) {
        updateVisibleCells()
    }

    func collectionView(
        _ collectionView: UICollectionView,
        cancelPrefetchingForItemsAt indexPaths: [IndexPath]
    ) {}

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        userInteracting = true
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        guard userInteracting, let id = currentArtifactId(), id != configuration?.selectedId else { return }
        configuration?.selectionChanged(id)
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { settleSelection() }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) {
        settleSelection()
    }

    private func settleSelection() {
        userInteracting = false
        guard let id = currentArtifactId() else { return }
        configuration?.selectionSettled(id)
    }

    private func positionPendingSelection(animated: Bool) {
        guard collectionView.bounds.width > 0,
              let id = pendingSelectionId ?? configuration?.selectedId,
              let index = artifactIndex[id] else { return }
        pendingSelectionId = nil
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: animated
        )
    }

    private func currentArtifactId() -> String? {
        guard collectionView.bounds.width > 0, !artifacts.isEmpty else { return nil }
        let index = Int(round(collectionView.contentOffset.x / collectionView.bounds.width))
        guard artifacts.indices.contains(index) else { return nil }
        return artifacts[index].id
    }

    private func updateVisibleCells() {
        for case let cell as StudioViewerPageCell in collectionView.visibleCells {
            guard let indexPath = collectionView.indexPath(for: cell),
                  artifacts.indices.contains(indexPath.item) else { continue }
            configure(cell, artifact: artifacts[indexPath.item])
        }
    }

    private func configure(_ cell: StudioViewerPageCell, artifact: StudioArtifactDetail) {
        guard let configuration else { return }
        let original = configuration.images[artifact.id]
        let preview = configuration.previews[artifact.id]
        let opening = artifact.id == configuration.openingPreviewArtifactId
            ? configuration.openingPreview
            : nil
        let image = original
            ?? preview
            ?? opening
            ?? configuration.browser.cachedPreview(artifactId: artifact.id)
        cell.configure(
            artifact: artifact,
            image: image,
            player: configuration.players[artifact.id],
            parentViewController: self
        )
    }
}

@MainActor
private final class StudioViewerPageCell: UICollectionViewCell, UIScrollViewDelegate {
    static let reuseIdentifier = "StudioViewerPageCell"

    private let imageScrollView = UIScrollView()
    private let imageView = UIImageView()
    private let placeholderView = UIImageView()
    private var representedId: String?
    private var playerController: AVPlayerViewController?

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
        if imageView.image !== image { imageView.image = image }
        placeholderView.isHidden = image != nil
    }

    func detachPlayer() {
        guard let playerController else { return }
        playerController.willMove(toParent: nil)
        playerController.view.removeFromSuperview()
        playerController.removeFromParent()
        self.playerController = nil
    }

    func viewForZooming(in scrollView: UIScrollView) -> UIView? {
        imageView
    }

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
