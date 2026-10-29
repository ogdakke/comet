import UIKit

/// Studio's library on one desktop: the gallery or the threads, switched
/// by the glass control in the bar's center, the desktop picked from the
/// menu on the right. Threads push on this stack; creations open in the
/// full-screen viewer, zooming out of their thumbnail.
final class StudioRootViewController: UIViewController, UIAdaptivePresentationControllerDelegate {
    private enum Mode: Int { case gallery, threads }

    private let app: AppModel
    private let service: StudioService
    private let browser = StudioBrowserStore()
    private let transitionSource = StudioGalleryTransitionSource()
    private var mode = Mode.gallery
    private var appToken: AnyObject?
    private var hostIds: [String]?
    /// Whether each desktop has a Studio provider key (absent: not asked yet).
    private var providerHosts: [String: Bool] = [:]
    private var watchKey: String?
    private var watches: [Task<Void, Never>] = []
    private weak var viewer: StudioGalleryViewerController?

    private lazy var gallery = StudioGalleryViewController(
        browser: browser,
        service: service,
        transitionSource: transitionSource,
        openViewer: { [weak self] in self?.openViewer($0) },
        showThread: { [weak self] in self?.showThread($0) }
    )
    private lazy var threads = StudioThreadsListController(browser: browser, service: service) { [weak self] id in
        self?.pushThread(id)
    }

    private lazy var switcher: UISegmentedControl = {
        let control = UISegmentedControl(items: ["Gallery", "Threads"])
        control.selectedSegmentIndex = mode.rawValue
        control.setTitleTextAttributes([.font: Fonts.ui(.sansMedium, 14), .foregroundColor: Palette.secondary], for: .normal)
        control.setTitleTextAttributes([.font: Fonts.ui(.sansMedium, 14), .foregroundColor: Palette.text], for: .selected)
        control.addAction(UIAction { [weak self, weak control] _ in
            guard let self, let control, let mode = Mode(rawValue: control.selectedSegmentIndex) else { return }
            self.mode = mode
            self.setNeedsUpdateProperties()
        }, for: .valueChanged)
        control.accessibilityLabel = "Studio view"
        control.accessibilityIdentifier = "studio-switcher"
        control.frame = CGRect(x: 0, y: 0, width: 188, height: 44)
        return control
    }()

    private lazy var machineItem: UIBarButtonItem = {
        let item = UIBarButtonItem(image: UIImage(systemName: "desktopcomputer"), menu: UIMenu(children: [
            // Built as it opens: hosts come and go.
            UIDeferredMenuElement.uncached { [weak self] done in done(self?.machineActions() ?? []) },
        ]))
        item.accessibilityLabel = "Studio machine"
        item.accessibilityIdentifier = "studio-machine"
        return item
    }()

    init(app: AppModel, service: StudioService) {
        self.app = app
        self.service = service
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    deinit {
        watches.forEach { $0.cancel() }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.largeTitleDisplayMode = .never
        navigationItem.backButtonDisplayMode = .minimal
        navigationItem.titleView = switcher
        navigationItem.rightBarButtonItem = machineItem
        // Hosts going on/offline, or appearing (not tracked by the store).
        appToken = app.observe { [weak self] in
            self?.syncHosts()
            self?.setNeedsUpdateProperties()
        }
        syncHosts()
    }

    /// Picks the desktop again only when the set of hosts changes.
    private func syncHosts(force: Bool = false) {
        let ids = service.hosts.map(\.id)
        guard force || ids != hostIds else { return }
        hostIds = ids
        browser.resolveDevice(from: service.hosts, online: service.isOnline)
        refreshProviderHosts()
    }

    /// Ask each online desktop whether it has a provider key.
    private func refreshProviderHosts() {
        guard !app.isDemo else { return }
        for host in service.hosts where service.isOnline(host.id) {
            Task { [weak self, service] in
                guard let providers = try? await service.listStudioProviders(deviceId: host.id), let self else { return }
                self.providerHosts[host.id] = providers.contains(where: \.configured)
                self.preferProviderHost()
            }
        }
    }

    /// Until a desktop is picked by hand, open on one that can create.
    private func preferProviderHost() {
        guard UserDefaults.standard.string(forKey: "studioDeviceId") == nil,
              let current = browser.selectedDeviceId, providerHosts[current] == false,
              let other = service.hosts.first(where: { providerHosts[$0.id] == true && service.isOnline($0.id) })
        else { return }
        browser.selectDevice(other.id)
    }

    /// Tracks the store: reads here re-run this when they change. Restarts
    /// the threads and gallery watches when the desktop or a reload asks.
    override func updateProperties() {
        super.updateProperties()
        let deviceId = browser.selectedDeviceId
        let key = "\(deviceId ?? "none")-\(browser.reloadGeneration)"
        if key != watchKey {
            watchKey = key
            restartWatches(deviceId: deviceId)
        }
        machineItem.accessibilityValue = deviceId.map(deviceName) ?? "None"
        switcher.selectedSegmentIndex = mode.rawValue

        if app.isDemo {
            showUnavailable("Studio is not in the demo", "Connect to a desktop device to browse your Studio library.")
        } else if service.hosts.isEmpty {
            showUnavailable("Studio needs a desktop", "Open Zeron on a desktop device, then try again.")
        } else if let deviceId, !service.isOnline(deviceId) {
            showUnavailable("Desktop is offline", "Reconnect \(deviceName(deviceId)) to browse its Studio library.")
        } else {
            contentUnavailableConfiguration = nil
            show(mode == .gallery ? gallery as UIViewController : threads)
        }
    }

    private func restartWatches(deviceId: String?) {
        watches.forEach { $0.cancel() }
        watches = []
        guard !app.isDemo, let deviceId else { return }
        watches = [
            Task { [browser, service] in await browser.watchThreads(workspace: service, deviceId: deviceId) },
            Task { [browser, service] in await browser.watchGallery(workspace: service, deviceId: deviceId) },
        ]
    }

    private func deviceName(_ id: String) -> String {
        service.hosts.first { $0.id == id }?.name ?? "Unknown device"
    }

    // MARK: Content

    /// One child on screen (or none, behind an unavailable state).
    private func show(_ child: UIViewController?) {
        for other in [gallery, threads] where other !== child && other.parent === self {
            other.willMove(toParent: nil)
            other.view.removeFromSuperview()
            other.removeFromParent()
        }
        guard let child, child.parent !== self else { return }
        addChild(child)
        child.view.frame = view.bounds
        child.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(child.view)
        child.didMove(toParent: self)
    }

    private func showUnavailable(_ title: String, _ message: String) {
        show(nil)
        var config = StudioUnavailable.configuration(
            title: title, message: message, symbol: "desktopcomputer.trianglebadge.exclamationmark"
        )
        var button = UIButton.Configuration.glass()
        button.title = "Try again"
        button.baseForegroundColor = Palette.text
        config.button = button
        config.buttonProperties.primaryAction = UIAction { [weak self] _ in
            self?.syncHosts(force: true)
            self?.browser.reload()
        }
        contentUnavailableConfiguration = config
    }

    private func machineActions() -> [UIMenuElement] {
        let hosts: [UIMenuElement] = service.hosts.map { device in
            let selected = browser.selectedDeviceId == device.id
            let symbol = selected ? "checkmark"
                : service.isOnline(device.id) ? "desktopcomputer" : "desktopcomputer.trianglebadge.exclamationmark"
            let action = UIAction(title: device.name, image: UIImage(systemName: symbol)) { [weak self] _ in
                self?.browser.selectDevice(device.id)
            }
            if providerHosts[device.id] == false { action.subtitle = "No provider key" }
            return action
        }
        guard let deviceId = browser.selectedDeviceId, service.isOnline(deviceId) else { return hosts }
        let providers = UIAction(title: "Providers…", image: UIImage(systemName: "key")) { [weak self] _ in
            guard let self else { return }
            self.presentStudioProviders(service: self.service, deviceId: deviceId) { [weak self] in
                self?.refreshProviderHosts()
            }
        }
        providers.accessibilityIdentifier = "studio-providers-open"
        return [UIMenu(options: .displayInline, children: hosts), providers]
    }

    // MARK: Threads

    private func pushThread(_ threadId: String) {
        let thread = StudioThreadViewController(
            threadId: threadId,
            browser: browser,
            service: service,
            transitionSource: transitionSource,
            openViewer: { [weak self] in self?.openViewer($0) }
        )
        navigationController?.pushViewController(thread, animated: true)
    }

    /// "Show in Thread" (gallery, viewer): the Threads list with that
    /// thread open over it.
    func showThread(_ threadId: String) {
        mode = .threads
        setNeedsUpdateProperties()
        if let nav = navigationController, nav.topViewController !== self {
            nav.popToViewController(self, animated: false)
        }
        pushThread(threadId)
    }

    // MARK: Viewer

    /// The full-screen viewer, zooming out of the thumbnail it opened from.
    func openViewer(_ session: StudioViewerSession) {
        guard viewer == nil, let presenter = StudioViewerPresentationHost.resolve(from: self) else { return }
        let controller = StudioGalleryViewerController(
            session: session,
            browser: browser,
            workspace: service,
            deviceId: browser.selectedDeviceId,
            requestDismissal: { [weak self] in self?.dismissViewer() },
            requestThread: { [weak self] threadId in
                self?.dismissViewer { self?.showThread(threadId) }
            },
            selectedArtifactChanged: { [weak transitionSource] artifactId in
                transitionSource?.prepareForDismissal(to: artifactId)
            }
        )
        StudioViewerNativeTransition.configure(controller, session: session, source: transitionSource)
        controller.modalPresentationStyle = .fullScreen
        viewer = controller
        presenter.present(controller, animated: true) { [weak self, weak controller] in
            controller?.presentationController?.delegate = self
        }
    }

    private func dismissViewer(completion: (() -> Void)? = nil) {
        guard let viewer else {
            completion?()
            return
        }
        viewer.session.presentationActive = false
        viewer.dismiss(animated: true) { [weak self] in
            self?.viewer = nil
            completion?()
        }
    }

    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
        viewer = nil
    }
}

/// Studio's empty and unavailable states, in the app's type.
enum StudioUnavailable {
    static func configuration(title: String, message: String, symbol: String) -> UIContentUnavailableConfiguration {
        var config = UIContentUnavailableConfiguration.empty()
        config.image = UIImage(systemName: symbol)
        config.imageProperties.tintColor = Palette.secondary
        config.text = title
        config.textProperties.font = Fonts.ui(.sansSemibold, 20)
        config.textProperties.color = Palette.text
        config.secondaryText = message
        config.secondaryTextProperties.font = Fonts.ui(.sans, 15)
        config.secondaryTextProperties.color = Palette.secondary
        return config
    }
}
