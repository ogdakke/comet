import UIKit

/// One desktop's Studio providers: whether each has a key and whether the
/// provider accepted it. A key typed here goes to that desktop's credential
/// store (its keychain, or an owner-only file on a headless machine), never
/// this phone's — which is how a desktop without a screen gets one.
final class StudioProvidersViewController: UIViewController, UICollectionViewDelegate {
    private let service: StudioService
    private let deviceId: String
    private let deviceName: String
    /// After a key is added or removed (the composer reloads its models).
    private let changed: () -> Void
    private var collectionView: UICollectionView!
    private var dataSource: UICollectionViewDiffableDataSource<Int, StudioProviderConnection>!
    private var providers: [StudioProviderConnection] = []
    private var busy: String?

    init(service: StudioService, deviceId: String, deviceName: String, changed: @escaping () -> Void) {
        self.service = service
        self.deviceId = deviceId
        self.deviceName = deviceName
        self.changed = changed
        super.init(nibName: nil, bundle: nil)
        title = "Providers"
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.rightBarButtonItem = UIBarButtonItem(systemItem: .done, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        var config = UICollectionLayoutListConfiguration(appearance: .insetGrouped)
        config.backgroundColor = .clear
        config.headerMode = .supplementary
        config.footerMode = .supplementary
        config.trailingSwipeActionsConfigurationProvider = { [weak self] indexPath in
            guard let self, let provider = self.dataSource.itemIdentifier(for: indexPath), provider.configured else { return nil }
            let remove = UIContextualAction(style: .destructive, title: "Remove Key") { [weak self] _, _, done in
                self?.remove(provider)
                done(true)
            }
            return UISwipeActionsConfiguration(actions: [remove])
        }
        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: UICollectionViewCompositionalLayout.list(using: config))
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.delegate = self
        collectionView.accessibilityIdentifier = "studio-providers"
        view.addSubview(collectionView)

        let cell = UICollectionView.CellRegistration<UICollectionViewListCell, StudioProviderConnection> { [weak self] cell, _, provider in
            var c = UIListContentConfiguration.subtitleCell()
            c.text = provider.displayLabel
            c.textProperties.font = Fonts.ui(.sansMedium, 16)
            c.textProperties.color = Palette.text
            let (status, tint) = Self.status(provider)
            c.secondaryText = status
            c.secondaryTextProperties.font = Fonts.ui(.sans, 13)
            c.secondaryTextProperties.color = tint
            c.image = UIImage(systemName: provider.configured ? "key.fill" : "key")
            c.imageProperties.tintColor = provider.configured ? Palette.text : Palette.secondary
            cell.contentConfiguration = c
            if self?.busy == provider.providerId {
                let spinner = UIActivityIndicatorView(style: .medium)
                spinner.startAnimating()
                cell.accessories = [.customView(configuration: .init(customView: spinner, placement: .trailing()))]
            } else {
                cell.accessories = [.disclosureIndicator()]
            }
        }
        let header = UICollectionView.SupplementaryRegistration<UICollectionViewListCell>(elementKind: UICollectionView.elementKindSectionHeader) { [weak self] view, _, _ in
            var c = UIListContentConfiguration.groupedHeader()
            c.text = "On \(self?.deviceName ?? "this desktop")"
            view.contentConfiguration = c
        }
        let footer = UICollectionView.SupplementaryRegistration<UICollectionViewListCell>(elementKind: UICollectionView.elementKindSectionFooter) { [weak self] view, _, _ in
            var c = UIListContentConfiguration.groupedFooter()
            c.text = "Keys are stored on \(self?.deviceName ?? "the desktop"), not on this phone, and are sent there over your Zeron connection (encrypted in transit). On a machine you set up by hand, ZERON_STUDIO_<PROVIDER>_KEY in the engine's environment works too."
            view.contentConfiguration = c
        }
        dataSource = UICollectionViewDiffableDataSource(collectionView: collectionView) { collectionView, indexPath, provider in
            collectionView.dequeueConfiguredReusableCell(using: cell, for: indexPath, item: provider)
        }
        dataSource.supplementaryViewProvider = { collectionView, kind, indexPath in
            collectionView.dequeueConfiguredReusableSupplementary(
                using: kind == UICollectionView.elementKindSectionHeader ? header : footer, for: indexPath
            )
        }
        contentUnavailableConfiguration = UIContentUnavailableConfiguration.loading()
        Task { await load() }
    }

    private static func status(_ provider: StudioProviderConnection) -> (String, UIColor) {
        guard provider.configured else { return ("No key — tap to add one", Palette.secondary) }
        switch provider.validationState {
        case .valid: return ("Connected", Palette.success)
        case .invalid: return (provider.validationMessage ?? "The provider rejected this key", Palette.danger)
        case .unavailable: return (provider.validationMessage ?? "Couldn't reach the provider", Palette.warning)
        case .notValidated, nil: return ("Key saved, not checked yet", Palette.secondary)
        }
    }

    private func load() async {
        do {
            providers = try await service.listStudioProviders(deviceId: deviceId)
            contentUnavailableConfiguration = providers.isEmpty
                ? StudioUnavailable.configuration(title: "No providers", message: "This desktop's Zeron doesn't offer any Studio providers.", symbol: "key")
                : nil
            render()
        } catch {
            contentUnavailableConfiguration = StudioUnavailable.configuration(
                title: "Providers unavailable", message: error.localizedDescription, symbol: "exclamationmark.triangle"
            )
        }
    }

    private func render() {
        var snapshot = NSDiffableDataSourceSnapshot<Int, StudioProviderConnection>()
        snapshot.appendSections([0])
        snapshot.appendItems(providers)
        dataSource.apply(snapshot, animatingDifferences: view.window != nil)
    }

    private func replace(_ connection: StudioProviderConnection) {
        if let index = providers.firstIndex(where: { $0.providerId == connection.providerId }) {
            providers[index] = connection
        }
        render()
    }

    private func setBusy(_ providerId: String?) {
        let previous = busy
        busy = providerId
        var snapshot = dataSource.snapshot()
        let ids = Set([previous, providerId].compactMap { $0 })
        snapshot.reconfigureItems(snapshot.itemIdentifiers.filter { ids.contains($0.providerId) })
        dataSource.apply(snapshot, animatingDifferences: false)
    }

    // MARK: Actions

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: true)
        guard busy == nil, let provider = dataSource.itemIdentifier(for: indexPath) else { return }
        askForKey(provider)
    }

    private func askForKey(_ provider: StudioProviderConnection) {
        let alert = UIAlertController(
            title: provider.configured ? "Replace \(provider.displayLabel) Key" : "Add \(provider.displayLabel) Key",
            message: "Paste the API key. It's saved on \(deviceName) and checked with \(provider.displayLabel).",
            preferredStyle: .alert
        )
        alert.addTextField { field in
            field.placeholder = "API key"
            field.isSecureTextEntry = true
            field.autocorrectionType = .no
            field.autocapitalizationType = .none
            field.textContentType = .password
        }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        if provider.configured {
            alert.addAction(UIAlertAction(title: "Remove Key", style: .destructive) { [weak self] _ in self?.remove(provider) })
        }
        alert.addAction(UIAlertAction(title: "Save", style: .default) { [weak self, weak alert] _ in
            let key = alert?.textFields?.first?.text?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            guard !key.isEmpty else { return }
            self?.save(provider, key: key)
        })
        present(alert, animated: true)
    }

    private func save(_ provider: StudioProviderConnection, key: String) {
        setBusy(provider.providerId)
        Task {
            defer { setBusy(nil) }
            do {
                let connection = try await service.setStudioProviderCredential(
                    deviceId: deviceId, providerId: provider.providerId,
                    displayLabel: provider.displayLabel, secret: key
                )
                replace(connection)
                changed()
            } catch {
                fail(error)
            }
        }
    }

    private func remove(_ provider: StudioProviderConnection) {
        setBusy(provider.providerId)
        Task {
            defer { setBusy(nil) }
            do {
                try await service.removeStudioProviderCredential(deviceId: deviceId, providerId: provider.providerId)
                await load()
                changed()
            } catch {
                fail(error)
            }
        }
    }

    private func fail(_ error: Error) {
        let alert = UIAlertController(title: "Couldn't update the provider", message: error.localizedDescription, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
}

extension UIViewController {
    /// Studio's providers for `deviceId`, as a sheet.
    func presentStudioProviders(service: StudioService, deviceId: String, changed: @escaping () -> Void) {
        let name = service.hosts.first { $0.id == deviceId }?.name ?? "the desktop"
        let providers = StudioProvidersViewController(service: service, deviceId: deviceId, deviceName: name, changed: changed)
        let nav = UINavigationController(rootViewController: providers)
        nav.modalPresentationStyle = .pageSheet
        nav.sheetPresentationController?.detents = [.medium(), .large()]
        nav.sheetPresentationController?.prefersGrabberVisible = true
        present(nav, animated: true)
    }
}
