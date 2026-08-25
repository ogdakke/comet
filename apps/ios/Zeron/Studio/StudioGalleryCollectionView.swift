import SwiftUI
import UIKit

@MainActor
final class StudioGalleryTransitionSource {
    weak var provider: StudioGalleryTransitionSourceProvider?

    func imageView(for artifactId: String) -> UIView? {
        provider?.transitionImageView(for: artifactId)
    }
}

@MainActor
protocol StudioGalleryTransitionSourceProvider: AnyObject {
    func transitionImageView(for artifactId: String) -> UIView?
}

struct StudioGalleryCollectionView: UIViewRepresentable {
    let items: [StudioGalleryItem]
    let browser: StudioBrowserStore
    let workspace: WorkspaceStore?
    let deviceId: String?
    let transitionSource: StudioGalleryTransitionSource
    let openItem: (StudioGalleryItem, UIImage?) -> Void
    let downloadItem: (StudioGalleryItem) -> Void
    let showThread: (String) -> Void
    let deleteItem: (StudioGalleryItem) -> Void
    let loadOlder: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeUIView(context: Context) -> UICollectionView {
        let layout = UICollectionViewFlowLayout()
        layout.minimumInteritemSpacing = 2
        layout.minimumLineSpacing = 2

        let collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
        collectionView.backgroundColor = .clear
        collectionView.alwaysBounceVertical = true
        collectionView.contentInsetAdjustmentBehavior = .automatic
        collectionView.keyboardDismissMode = .onDrag
        collectionView.delegate = context.coordinator
        collectionView.prefetchDataSource = context.coordinator
        collectionView.register(
            StudioGalleryCell.self,
            forCellWithReuseIdentifier: StudioGalleryCell.reuseIdentifier
        )
        context.coordinator.installDataSource(on: collectionView)
        transitionSource.provider = context.coordinator
        context.coordinator.apply(items: items, to: collectionView, initial: true)
        return collectionView
    }

    func updateUIView(_ collectionView: UICollectionView, context: Context) {
        context.coordinator.parent = self
        context.coordinator.apply(items: items, to: collectionView, initial: false)
    }

    @MainActor
    final class Coordinator: NSObject,
        UICollectionViewDelegate,
        UICollectionViewDelegateFlowLayout,
        UICollectionViewDataSourcePrefetching,
        StudioGalleryTransitionSourceProvider
    {
        private enum Section { case gallery }

        var parent: StudioGalleryCollectionView
        private var dataSource: UICollectionViewDiffableDataSource<Section, String>?
        private weak var collectionView: UICollectionView?
        private var items: [StudioGalleryItem] = []
        private var itemsById: [String: StudioGalleryItem] = [:]
        private var prefetchedIds: Set<String> = []
        private var requestedOldestId: String?
        private var didSetInitialPosition = false

        init(parent: StudioGalleryCollectionView) {
            self.parent = parent
        }

        func installDataSource(on collectionView: UICollectionView) {
            self.collectionView = collectionView
            dataSource = UICollectionViewDiffableDataSource<Section, String>(
                collectionView: collectionView
            ) { [weak self] collectionView, indexPath, artifactId in
                guard let self,
                      let item = self.itemsById[artifactId],
                      let cell = collectionView.dequeueReusableCell(
                          withReuseIdentifier: StudioGalleryCell.reuseIdentifier,
                          for: indexPath
                      ) as? StudioGalleryCell else {
                    return UICollectionViewCell()
                }
                self.configure(cell, with: item)
                return cell
            }
        }

        func transitionImageView(for artifactId: String) -> UIView? {
            guard let collectionView,
                  let indexPath = dataSource?.indexPath(for: artifactId) else { return nil }
            if collectionView.cellForItem(at: indexPath) == nil {
                collectionView.scrollToItem(at: indexPath, at: .centeredVertically, animated: false)
                collectionView.layoutIfNeeded()
            }
            return (collectionView.cellForItem(at: indexPath) as? StudioGalleryCell)?.imageView
        }

        func apply(items newItems: [StudioGalleryItem], to collectionView: UICollectionView, initial: Bool) {
            let oldIds = items.map(\.id)
            let newIds = newItems.map(\.id)
            guard initial || oldIds != newIds else { return }

            cancelAllPrefetching()
            let oldFirstId = oldIds.first
            let oldContentHeight = collectionView.contentSize.height
            let oldOffset = collectionView.contentOffset.y
            let wasNearBottom = oldContentHeight - collectionView.bounds.height - oldOffset < 100

            items = newItems
            itemsById = Dictionary(uniqueKeysWithValues: newItems.map { ($0.id, $0) })
            if newIds.first != oldFirstId { requestedOldestId = nil }

            var snapshot = NSDiffableDataSourceSnapshot<Section, String>()
            snapshot.appendSections([.gallery])
            snapshot.appendItems(newIds)
            dataSource?.apply(snapshot, animatingDifferences: false) { [weak self, weak collectionView] in
                guard let self, let collectionView else { return }
                collectionView.layoutIfNeeded()
                if !self.didSetInitialPosition, !newItems.isEmpty {
                    self.didSetInitialPosition = true
                    collectionView.scrollToItem(
                        at: IndexPath(item: newItems.count - 1, section: 0),
                        at: .bottom,
                        animated: false
                    )
                } else if let oldFirstId,
                          let newIndex = newIds.firstIndex(of: oldFirstId),
                          newIndex > 0 {
                    let heightDelta = collectionView.contentSize.height - oldContentHeight
                    collectionView.contentOffset.y = oldOffset + heightDelta
                } else if wasNearBottom, newIds.count > oldIds.count, !newItems.isEmpty {
                    collectionView.scrollToItem(
                        at: IndexPath(item: newItems.count - 1, section: 0),
                        at: .bottom,
                        animated: false
                    )
                }
            }
        }

        func collectionView(
            _ collectionView: UICollectionView,
            layout collectionViewLayout: UICollectionViewLayout,
            sizeForItemAt indexPath: IndexPath
        ) -> CGSize {
            let side = floor((collectionView.bounds.width - 4) / 3)
            return CGSize(width: side, height: side)
        }

        func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
            guard let item = item(at: indexPath) else { return }
            let image = (collectionView.cellForItem(at: indexPath) as? StudioGalleryCell)?.displayedImage
            parent.openItem(item, image)
        }

        func collectionView(
            _ collectionView: UICollectionView,
            willDisplay cell: UICollectionViewCell,
            forItemAt indexPath: IndexPath
        ) {
            guard let item = item(at: indexPath) else { return }
            endPrefetch(for: item.id)
            if let cell = cell as? StudioGalleryCell { configure(cell, with: item) }
            if indexPath.item < 12, requestedOldestId != items.first?.id {
                requestedOldestId = items.first?.id
                parent.loadOlder()
            }
        }

        func collectionView(
            _ collectionView: UICollectionView,
            didEndDisplaying cell: UICollectionViewCell,
            forItemAt indexPath: IndexPath
        ) {
            (cell as? StudioGalleryCell)?.cancelPreviewRequest()
        }

        func collectionView(
            _ collectionView: UICollectionView,
            prefetchItemsAt indexPaths: [IndexPath]
        ) {
            guard let workspace = parent.workspace, let deviceId = parent.deviceId else { return }
            for indexPath in indexPaths {
                guard let item = item(at: indexPath), prefetchedIds.insert(item.id).inserted else { continue }
                _ = parent.browser.beginPreviewRequest(
                    artifactId: item.id,
                    deviceId: deviceId,
                    workspace: workspace
                )
            }
        }

        func collectionView(
            _ collectionView: UICollectionView,
            cancelPrefetchingForItemsAt indexPaths: [IndexPath]
        ) {
            for indexPath in indexPaths {
                guard let item = item(at: indexPath) else { continue }
                endPrefetch(for: item.id)
            }
        }

        func collectionView(
            _ collectionView: UICollectionView,
            contextMenuConfigurationForItemAt indexPath: IndexPath,
            point: CGPoint
        ) -> UIContextMenuConfiguration? {
            guard let item = item(at: indexPath) else { return nil }
            return UIContextMenuConfiguration(identifier: item.id as NSString, previewProvider: nil) { [weak self] _ in
                guard let self else { return nil }
                return UIMenu(children: [
                    UIAction(title: "Download", image: UIImage(systemName: "square.and.arrow.down")) { _ in
                        self.parent.downloadItem(item)
                    },
                    UIAction(title: "Show in Thread", image: UIImage(systemName: "rectangle.stack")) { _ in
                        self.parent.showThread(item.conversationId)
                    },
                    UIAction(title: "Delete", image: UIImage(systemName: "trash"), attributes: .destructive) { _ in
                        self.parent.deleteItem(item)
                    },
                ])
            }
        }

        private func configure(_ cell: StudioGalleryCell, with item: StudioGalleryItem) {
            cell.configure(
                item: item,
                browser: parent.browser,
                workspace: parent.workspace,
                deviceId: parent.deviceId
            )
        }

        private func item(at indexPath: IndexPath) -> StudioGalleryItem? {
            guard items.indices.contains(indexPath.item) else { return nil }
            return items[indexPath.item]
        }

        private func endPrefetch(for artifactId: String) {
            guard prefetchedIds.remove(artifactId) != nil else { return }
            parent.browser.endPreviewRequest(artifactId: artifactId)
        }

        private func cancelAllPrefetching() {
            for id in prefetchedIds { parent.browser.endPreviewRequest(artifactId: id) }
            prefetchedIds.removeAll()
        }
    }
}

@MainActor
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
        contentView.backgroundColor = UIColor(Theme.elementHover)
        contentView.clipsToBounds = true

        imageView.frame = contentView.bounds
        imageView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        imageView.contentMode = .scaleAspectFill
        imageView.clipsToBounds = true
        contentView.addSubview(imageView)

        placeholderView.frame = contentView.bounds
        placeholderView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        placeholderView.contentMode = .center
        placeholderView.tintColor = UIColor(Theme.textFaint.opacity(0.45))
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

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func layoutSubviews() {
        super.layoutSubviews()
        let badgeSize = videoLabel.sizeThatFits(CGSize(width: 90, height: 22))
        let width = max(34, badgeSize.width + 14)
        videoBadge.frame = CGRect(
            x: contentView.bounds.width - width - 6,
            y: contentView.bounds.height - 26,
            width: width,
            height: 20
        )
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

    func configure(
        item: StudioGalleryItem,
        browser: StudioBrowserStore,
        workspace: WorkspaceStore?,
        deviceId: String?
    ) {
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
        guard !requestActive, let workspace, let deviceId else { return }
        imageView.image = nil
        placeholderView.isHidden = false
        requestActive = true
        let artifactId = item.id
        let request = browser.beginPreviewRequest(
            artifactId: artifactId,
            deviceId: deviceId,
            workspace: workspace
        )
        loadTask = Task { @MainActor [weak self] in
            let image = await request.value
            guard !Task.isCancelled, self?.representedId == artifactId else { return }
            self?.imageView.image = image
            self?.placeholderView.isHidden = image != nil
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

    private static func duration(_ seconds: Double?) -> String {
        guard let seconds else { return "Video" }
        let total = max(0, Int(seconds.rounded()))
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}
