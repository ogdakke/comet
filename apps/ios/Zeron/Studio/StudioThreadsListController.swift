import UIKit

/// The Studio threads on the selected desktop, newest first, with the
/// archived ones folded under a disclosure row at the end.
final class StudioThreadsListController: UIViewController, UICollectionViewDelegate {
    private enum Item: Hashable {
        case thread(String)
        case archived
    }

    /// What a row shows; a change reconfigures it in place.
    private struct Row: Equatable {
        let thread: StudioThreadSummary
        let preview: StudioGalleryItem?
    }

    private let browser: StudioBrowserStore
    private let service: StudioService
    private let openThread: (String) -> Void

    private var collectionView: UICollectionView!
    private var dataSource: UICollectionViewDiffableDataSource<Int, Item>!
    private let loadingView = StudioLoadingView("Loading threads…")
    private var rows: [String: Row] = [:]
    private var archivedCount = 0
    private var archivedOpen = false

    init(browser: StudioBrowserStore, service: StudioService, openThread: @escaping (String) -> Void) {
        self.browser = browser
        self.service = service
        self.openThread = openThread
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        var config = UICollectionLayoutListConfiguration(appearance: .plain)
        config.backgroundColor = .clear
        config.separatorConfiguration.color = Palette.hairline
        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: UICollectionViewCompositionalLayout.list(using: config))
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.topEdgeEffect.style = .soft
        collectionView.delegate = self
        view.addSubview(collectionView)

        let threadReg = UICollectionView.CellRegistration<StudioThreadCell, String> { [weak self] cell, _, id in
            guard let self, let row = self.rows[id] else { return }
            cell.configure(row.thread, preview: row.preview, browser: self.browser, service: self.service)
        }
        let archivedReg = UICollectionView.CellRegistration<UICollectionViewListCell, Item> { [weak self] cell, _, _ in
            var c = UIListContentConfiguration.cell()
            c.text = "Archived (\(self?.archivedCount ?? 0))"
            c.textProperties.font = Fonts.ui(.sansMedium, 12)
            c.textProperties.color = Palette.secondary
            cell.contentConfiguration = c
            cell.accessories = [.outlineDisclosure(options: .init(style: .cell, tintColor: Palette.secondary)) {
                self?.toggleArchived()
            }]
            cell.backgroundConfiguration = .clear()
            cell.accessibilityIdentifier = "studio-archived"
        }
        dataSource = UICollectionViewDiffableDataSource(collectionView: collectionView) { cv, path, item in
            switch item {
            case let .thread(id): cv.dequeueConfiguredReusableCell(using: threadReg, for: path, item: id)
            case .archived: cv.dequeueConfiguredReusableCell(using: archivedReg, for: path, item: item)
            }
        }

        loadingView.frame = view.bounds
        loadingView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(loadingView)
    }

    /// Tracks the store: reads here re-run this when they change.
    override func updateProperties() {
        super.updateProperties()
        let threads = browser.threads
        let loading = browser.threadsLoading && threads.isEmpty
        loadingView.isHidden = !loading
        collectionView.isHidden = threads.isEmpty
        if loading {
            contentUnavailableConfiguration = nil
        } else if let error = browser.threadsError, threads.isEmpty {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "Threads unavailable", message: error, symbol: "exclamationmark.triangle"
            )
        } else if threads.isEmpty {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "No threads yet", message: "New Studio threads will appear here.", symbol: "rectangle.stack"
            )
        } else {
            contentUnavailableConfiguration = nil
        }
        apply(threads, gallery: browser.gallery)
    }

    private func apply(_ threads: [StudioThreadSummary], gallery: [StudioGalleryItem]) {
        // A thread's preview is its newest creation.
        var latest: [String: StudioGalleryItem] = [:]
        for item in gallery { latest[item.conversationId] = item }
        let next = Dictionary(threads.map { ($0.id, Row(thread: $0, preview: latest[$0.id])) }, uniquingKeysWith: { a, _ in a })
        let changed = next.compactMap { id, row in rows[id].map { $0 == row ? nil : Item.thread(id) } ?? nil }
        let active = threads.filter { !$0.archived }
        let archived = threads.filter(\.archived)
        let countChanged = archivedCount != archived.count
        rows = next
        archivedCount = archived.count

        var section = NSDiffableDataSourceSectionSnapshot<Item>()
        section.append(active.map { .thread($0.id) })
        if !archived.isEmpty {
            section.append([.archived])
            section.append(archived.map { .thread($0.id) }, to: .archived)
            if archivedOpen { section.expand([.archived]) }
        }
        let animate = collectionView.window != nil
        dataSource.apply(section, to: 0, animatingDifferences: animate)
        var snapshot = dataSource.snapshot()
        var reconfigure = changed.filter { snapshot.indexOfItem($0) != nil }
        if countChanged, snapshot.indexOfItem(.archived) != nil { reconfigure.append(.archived) }
        guard !reconfigure.isEmpty else { return }
        snapshot.reconfigureItems(reconfigure)
        dataSource.apply(snapshot, animatingDifferences: false)
    }

    /// Folds or unfolds the archived threads (the row or its chevron).
    private func toggleArchived() {
        archivedOpen.toggle()
        var section = dataSource.snapshot(for: 0)
        if archivedOpen { section.expand([.archived]) } else { section.collapse([.archived]) }
        dataSource.apply(section, to: 0, animatingDifferences: true)
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: true)
        switch dataSource.itemIdentifier(for: indexPath) {
        case let .thread(id): openThread(id)
        case .archived: toggleArchived()
        case nil: break
        }
    }
}

/// A thread row: its newest creation (52pt), title, turns and when it last
/// changed, and Creating / Done on the right.
private final class StudioThreadCell: UICollectionViewListCell {
    private var preview: StudioMediaPreviewView?
    private let emptyPreview = UIView()
    private let emptyGlyph = UIImageView(image: UIImage(systemName: "photo"))
    private let titleLabel = UILabel()
    private let metaLabel = UILabel()
    private let spinner = UIActivityIndicatorView(style: .medium)
    private let statusLabel = UILabel()
    private let previewSlot = UIView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundConfiguration = .clear()

        previewSlot.layer.cornerRadius = 9
        previewSlot.layer.cornerCurve = .continuous
        previewSlot.clipsToBounds = true
        emptyPreview.backgroundColor = Palette.chip
        emptyGlyph.tintColor = Palette.color(.textFaint)
        emptyGlyph.contentMode = .center
        for v in [emptyPreview, emptyGlyph] {
            v.frame = previewSlot.bounds
            v.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        }
        emptyPreview.addSubview(emptyGlyph)
        previewSlot.addSubview(emptyPreview)

        titleLabel.font = Fonts.ui(.sansMedium, 14)
        metaLabel.font = Fonts.ui(.sans, 11)
        metaLabel.textColor = Palette.color(.textFaint)
        let text = UIStackView(arrangedSubviews: [titleLabel, metaLabel])
        text.axis = .vertical
        text.spacing = 4

        spinner.transform = CGAffineTransform(scaleX: 0.7, y: 0.7)
        spinner.color = Palette.secondary
        let status = UIStackView(arrangedSubviews: [spinner, statusLabel])
        status.spacing = 6
        status.alignment = .center
        status.setContentHuggingPriority(.required, for: .horizontal)
        status.setContentCompressionResistancePriority(.required, for: .horizontal)

        let row = UIStackView(arrangedSubviews: [previewSlot, text, status])
        row.spacing = 12
        row.alignment = .center
        row.setCustomSpacing(8, after: text)
        row.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(row)
        NSLayoutConstraint.activate([
            previewSlot.widthAnchor.constraint(equalToConstant: 52),
            previewSlot.heightAnchor.constraint(equalToConstant: 52),
            row.leadingAnchor.constraint(equalTo: contentView.layoutMarginsGuide.leadingAnchor),
            row.trailingAnchor.constraint(equalTo: contentView.layoutMarginsGuide.trailingAnchor),
            row.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 10),
            row.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -10),
        ])
        // Separators start under the title, as list rows do.
        separatorLayoutGuide.leadingAnchor.constraint(equalTo: text.leadingAnchor).isActive = true
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(_ thread: StudioThreadSummary, preview item: StudioGalleryItem?, browser: StudioBrowserStore, service: StudioService) {
        if let item {
            let view = preview ?? StudioMediaPreviewView(browser: browser, service: service)
            if preview == nil {
                view.frame = previewSlot.bounds
                view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
                previewSlot.addSubview(view)
                preview = view
            }
            view.isHidden = false
            view.configure(artifactId: item.id, mediaKind: item.mediaKind, thumbhash: item.thumbhash, aspectRatio: item.aspectRatio)
        } else {
            preview?.reset()
            preview?.isHidden = true
        }

        titleLabel.text = thread.title
        titleLabel.textColor = thread.archived ? Palette.secondary : Palette.text
        let turns = "\(thread.turnCount) \(thread.turnCount == 1 ? "turn" : "turns")"
        metaLabel.text = "\(turns) · \(thread.updatedDate.formatted(.relative(presentation: .named)))"

        if thread.creating {
            spinner.startAnimating()
            spinner.isHidden = false
            statusLabel.isHidden = false
            statusLabel.text = "Creating"
            statusLabel.font = Fonts.ui(.sansMedium, 11)
            statusLabel.textColor = Palette.secondary
        } else {
            spinner.stopAnimating()
            spinner.isHidden = true
            statusLabel.isHidden = !thread.done
            statusLabel.text = "Done"
            statusLabel.font = Fonts.ui(.sansSemibold, 11)
            statusLabel.textColor = Palette.success
        }
        accessibilityIdentifier = "studio-thread-\(thread.id)"
    }
}
