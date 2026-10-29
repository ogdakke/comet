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
        // As the Sessions list: rows on the page, a rounded fill when pressed.
        config.showsSeparators = false
        config.trailingSwipeActionsConfigurationProvider = { [weak self] indexPath in
            self?.swipeActions(at: indexPath)
        }
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
            c.textProperties.font = Fonts.ui(.sansMedium, 13)
            c.textProperties.color = Palette.secondary
            cell.contentConfiguration = c
            cell.accessories = [.outlineDisclosure(options: .init(style: .cell, tintColor: Palette.secondary)) {
                self?.toggleArchived()
            }]
            cell.configurationUpdateHandler = { cell, state in
                cell.backgroundConfiguration = StudioThreadCell.background(for: state)
            }
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

    /// "3 minutes ago" went stale while a thread was on top.
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        reconfigureAll()
    }

    private func reconfigureAll() {
        guard let dataSource else { return }
        var snapshot = dataSource.snapshot()
        guard !snapshot.itemIdentifiers.isEmpty else { return }
        snapshot.reconfigureItems(snapshot.itemIdentifiers)
        dataSource.apply(snapshot, animatingDifferences: false)
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
                title: "Threads unavailable", message: error, symbol: "exclamationmark.triangle",
                retry: { [weak self] in self?.browser.reload() }
            )
        } else if threads.isEmpty {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "No threads yet", message: "Threads you start in Studio on this desktop will appear here.", symbol: "rectangle.stack"
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

    // MARK: Thread actions

    private func swipeActions(at indexPath: IndexPath) -> UISwipeActionsConfiguration? {
        guard case let .thread(id)? = dataSource.itemIdentifier(for: indexPath),
              let thread = rows[id]?.thread else { return nil }
        let delete = UIContextualAction(style: .destructive, title: "Delete") { [weak self] _, _, done in
            guard let self else { return done(false) }
            StudioThreadActions.confirmDelete(thread, from: self, browser: self.browser, service: self.service) { done($0) }
        }
        delete.image = UIImage(systemName: "trash")
        let archive = UIContextualAction(style: .normal, title: thread.archived ? "Unarchive" : "Archive") { [weak self] _, _, done in
            guard let self else { return done(false) }
            StudioThreadActions.setArchived(!thread.archived, thread, from: self, browser: self.browser, service: self.service)
            done(true)
        }
        archive.image = UIImage(systemName: thread.archived ? "tray.and.arrow.up" : "archivebox")
        archive.backgroundColor = Palette.secondary
        return UISwipeActionsConfiguration(actions: [delete, archive])
    }

    func collectionView(_ collectionView: UICollectionView, contextMenuConfigurationForItemsAt indexPaths: [IndexPath], point: CGPoint) -> UIContextMenuConfiguration? {
        guard indexPaths.count == 1, case let .thread(id)? = dataSource.itemIdentifier(for: indexPaths[0]),
              let thread = rows[id]?.thread else { return nil }
        return UIContextMenuConfiguration(actionProvider: { [weak self] _ in
            guard let self else { return nil }
            return UIMenu(children: StudioThreadActions.menuElements(for: thread, from: self, browser: self.browser, service: self.service))
        })
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
    private let status = UIStackView()
    private let previewSlot = UIView()

    /// The Sessions list's row fill: clear, rounded controlFill when pressed.
    static func background(for state: UICellConfigurationState) -> UIBackgroundConfiguration {
        var bg = UIBackgroundConfiguration.listCell().updated(for: state)
        bg.backgroundColor = state.isHighlighted || state.isSelected ? Palette.controlFill : .clear
        bg.cornerRadius = 16
        bg.backgroundInsets = NSDirectionalEdgeInsets(top: 1, leading: 8, bottom: 1, trailing: 8)
        return bg
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundConfiguration = Self.background(for: configurationState)

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

        titleLabel.font = UIFontMetrics(forTextStyle: .body).scaledFont(for: Fonts.ui(.sansMedium, 14))
        metaLabel.font = UIFontMetrics(forTextStyle: .footnote).scaledFont(for: Fonts.ui(.sans, 11))
        metaLabel.textColor = Palette.color(.textFaint)
        let text = UIStackView(arrangedSubviews: [titleLabel, metaLabel])
        text.axis = .vertical
        text.spacing = 4
        // The text column takes the row's width; the status hugs the
        // trailing edge (the labels inside both stacks decide this, not
        // the stacks' own priorities).
        for label in [titleLabel, metaLabel] {
            label.setContentHuggingPriority(.defaultLow, for: .horizontal)
            label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            label.adjustsFontForContentSizeCategory = true
        }
        statusLabel.setContentHuggingPriority(.required, for: .horizontal)
        statusLabel.setContentCompressionResistancePriority(.required, for: .horizontal)

        spinner.transform = CGAffineTransform(scaleX: 0.7, y: 0.7)
        spinner.color = Palette.secondary
        for v in [spinner, statusLabel] { status.addArrangedSubview(v) }
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

        // One element for VoiceOver: the title, then turns, time and state.
        isAccessibilityElement = true
        accessibilityTraits = .button
    }

    required init?(coder: NSCoder) { fatalError() }

    override func updateConfiguration(using state: UICellConfigurationState) {
        backgroundConfiguration = Self.background(for: state)
    }

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

        let title = thread.title.trimmingCharacters(in: .whitespacesAndNewlines)
        titleLabel.text = title.isEmpty ? "Untitled thread" : title
        titleLabel.textColor = thread.archived || title.isEmpty ? Palette.secondary : Palette.text
        let turns = "\(thread.turnCount) \(thread.turnCount == 1 ? "turn" : "turns")"
        let updated = thread.updatedDate == .distantPast ? nil : thread.updatedDate.formatted(.relative(presentation: .named))
        metaLabel.text = [turns, updated].compactMap { $0 }.joined(separator: " · ")

        if thread.creating {
            spinner.startAnimating()
            spinner.isHidden = false
            statusLabel.text = "Creating"
            statusLabel.font = Fonts.ui(.sansMedium, 11)
            statusLabel.textColor = Palette.secondary
        } else {
            spinner.stopAnimating()
            spinner.isHidden = true
            statusLabel.text = "Done"
            statusLabel.font = Fonts.ui(.sansSemibold, 11)
            statusLabel.textColor = Palette.success
        }
        // Hidden whole when there's nothing to say, so the title takes the
        // width (the stack's spacing after it goes with it).
        status.isHidden = !thread.creating && !thread.done

        accessibilityLabel = titleLabel.text
        accessibilityValue = [metaLabel.text, status.isHidden ? nil : statusLabel.text, thread.archived ? "Archived" : nil]
            .compactMap { $0 }.joined(separator: ", ")
        accessibilityIdentifier = "studio-thread-\(thread.id)"
    }
}

/// Rename, archive and delete for one thread: the list's swipes and menus,
/// and the thread screen's ⋯ menu.
@MainActor
enum StudioThreadActions {
    static func menuElements(
        for thread: StudioThreadSummary,
        from host: UIViewController,
        browser: StudioBrowserStore,
        service: StudioService,
        deleted: (() -> Void)? = nil
    ) -> [UIMenuElement] {
        [
            UIAction(title: "Rename", image: UIImage(systemName: "pencil")) { [weak host] _ in
                guard let host else { return }
                rename(thread, from: host, browser: browser, service: service)
            },
            UIAction(
                title: thread.archived ? "Unarchive" : "Archive",
                image: UIImage(systemName: thread.archived ? "tray.and.arrow.up" : "archivebox")
            ) { [weak host] _ in
                guard let host else { return }
                setArchived(!thread.archived, thread, from: host, browser: browser, service: service)
            },
            UIMenu(options: .displayInline, children: [
                UIAction(title: "Delete Thread", image: UIImage(systemName: "trash"), attributes: .destructive) { [weak host] _ in
                    guard let host else { return }
                    confirmDelete(thread, from: host, browser: browser, service: service) { if $0 { deleted?() } }
                },
            ]),
        ]
    }

    static func rename(_ thread: StudioThreadSummary, from host: UIViewController, browser: StudioBrowserStore, service: StudioService) {
        guard let deviceId = browser.selectedDeviceId else { return }
        let alert = UIAlertController(title: "Rename Thread", message: nil, preferredStyle: .alert)
        alert.addTextField { field in
            field.text = thread.title
            field.placeholder = "Title"
            field.clearButtonMode = .whileEditing
            field.autocapitalizationType = .sentences
            field.returnKeyType = .done
        }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        let save = UIAlertAction(title: "Rename", style: .default) { [weak host, weak alert] _ in
            let title = alert?.textFields?.first?.text?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            guard !title.isEmpty, title != thread.title else { return }
            Task {
                do {
                    try await service.renameStudioThread(deviceId: deviceId, threadId: thread.id, title: String(title.prefix(200)))
                } catch {
                    showFailure(error, from: host)
                }
            }
        }
        alert.addAction(save)
        alert.preferredAction = save
        present(alert, from: host)
    }

    static func setArchived(_ archived: Bool, _ thread: StudioThreadSummary, from host: UIViewController, browser: StudioBrowserStore, service: StudioService) {
        guard let deviceId = browser.selectedDeviceId else { return }
        Task { [weak host] in
            do {
                try await service.archiveStudioThread(deviceId: deviceId, threadId: thread.id, archived: archived)
            } catch {
                showFailure(error, from: host)
            }
        }
    }

    /// `completion(true)` once it's gone from the desktop.
    static func confirmDelete(
        _ thread: StudioThreadSummary,
        from host: UIViewController,
        browser: StudioBrowserStore,
        service: StudioService,
        completion: @escaping (Bool) -> Void
    ) {
        guard let deviceId = browser.selectedDeviceId else { return completion(false) }
        let desktop = service.hosts.first { $0.id == deviceId }?.name ?? "the desktop"
        let title = thread.title.trimmingCharacters(in: .whitespacesAndNewlines)
        let sheet = UIAlertController(
            title: title.isEmpty ? "Delete this thread?" : "Delete “\(title)”?",
            message: "Its prompts and everything it made are removed from Studio on \(desktop). This can't be undone.",
            preferredStyle: .alert
        )
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completion(false) })
        sheet.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak host] _ in
            Task {
                do {
                    try await service.deleteStudioThread(deviceId: deviceId, threadId: thread.id)
                    completion(true)
                } catch {
                    completion(false)
                    showFailure(error, from: host)
                }
            }
        })
        present(sheet, from: host)
    }

    private static func showFailure(_ error: Error, from host: UIViewController?) {
        guard let host else { return }
        let alert = UIAlertController(title: "Studio action failed", message: error.localizedDescription, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, from: host)
    }

    /// Over whatever is already up from `host`.
    private static func present(_ controller: UIViewController, from host: UIViewController) {
        guard host.viewIfLoaded?.window != nil else { return }
        var top = host
        while let presented = top.presentedViewController { top = presented }
        top.present(controller, animated: true)
    }
}
