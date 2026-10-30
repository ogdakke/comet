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
    private var onlineHostIds: Set<String> = []
    /// Whether each desktop has a Studio provider key (absent: not asked yet).
    private var providerHosts: [String: Bool] = [:]
    private var watchKey: String?
    private var watches: [Task<Void, Never>] = []
    /// Launch arguments for screenshots and profiling: `-studio-threads`
    /// (the Threads list), `-studio-thread` (open the newest thread),
    /// `-studio-viewer` (open the newest creation). Each runs once. With
    /// `-studio-thread`: `-studio-thread-match <text>` picks the thread by
    /// title, and the thread screen's own steps (`-studio-thread-top`,
    /// `-studio-thread-tile`, `-studio-thread-compose`) run on it.
    /// `-studio-bench` flings the gallery through the whole library and
    /// writes `Documents/studio-bench.json`.
    private var launchSteps = Set(["-studio-threads", "-studio-thread", "-studio-viewer", "-studio-viewer-oldest", "-studio-bench"]
        .filter(ProcessInfo.processInfo.arguments.contains))
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
        item.accessibilityLabel = "Desktop"
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
        // Behind the switcher: names this page in the back button's menu.
        title = "Studio"
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

    /// Picks the desktop again only when the set of hosts changes; asks a
    /// desktop about its keys again when it comes (back) online.
    private func syncHosts(force: Bool = false) {
        let hosts = service.hosts
        let ids = hosts.map(\.id)
        let online = Set(hosts.filter(\.online).map(\.id))
        let cameOnline = online.subtracting(onlineHostIds)
        onlineHostIds = online
        guard force || ids != hostIds else {
            if !cameOnline.isEmpty { refreshProviderHosts(cameOnline) }
            return
        }
        hostIds = ids
        if let current = browser.selectedDeviceId, ids.contains(current) {
            // Stays on its desktop, even offline: the page says so, and the
            // menu picks another.
        } else if browser.selectedDeviceId != nil {
            // Its desktop is gone. `selectDevice` drops the old library
            // (`resolveDevice` would keep it on screen under the new one).
            if let next = hosts.first(where: \.online) ?? hosts.first { browser.selectDevice(next.id) }
        } else {
            browser.resolveDevice(from: hosts, online: service.isOnline)
        }
        refreshProviderHosts()
    }

    /// Ask each online desktop (or just `only`) whether it has a provider key.
    private func refreshProviderHosts(_ only: Set<String>? = nil) {
        for host in service.hosts where service.isOnline(host.id) && only?.contains(host.id) != false {
            Task { [weak self, service] in
                let providers = try? await service.listStudioProviders(deviceId: host.id)
                guard let self else { return }
                // An engine without Studio (or that can't answer) can't create either.
                self.providerHosts[host.id] = providers?.contains(where: \.configured) ?? false
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
    /// the threads and gallery watches when the desktop, its connection or
    /// a reload asks: a watch ends with the connection, and nothing else
    /// would start it again once the desktop is back.
    override func updateProperties() {
        super.updateProperties()
        let deviceId = browser.selectedDeviceId
        let online = deviceId.map(service.isOnline) ?? false
        let key = "\(deviceId ?? "none")-\(online)-\(browser.reloadGeneration)"
        if key != watchKey {
            watchKey = key
            restartWatches(deviceId: online ? deviceId : nil)
        }
        machineItem.accessibilityValue = deviceId.map(deviceName) ?? "None"
        runLaunchSteps()
        switcher.selectedSegmentIndex = mode.rawValue

        if service.hosts.isEmpty {
            showUnavailable("Studio needs a desktop", "Open Zeron on a desktop device, then try again.")
        } else if let deviceId, !online {
            let others = service.hosts.contains { $0.id != deviceId && service.isOnline($0.id) }
            showUnavailable("Desktop is offline", "Reconnect \(deviceName(deviceId)) to browse its Studio library\(others ? ", or pick another desktop from the menu" : "").")
        } else {
            contentUnavailableConfiguration = nil
            show(mode == .gallery ? gallery as UIViewController : threads)
        }
    }

    private func runLaunchSteps() {
        guard !launchSteps.isEmpty else { return }
        if launchSteps.remove("-studio-threads") != nil { mode = .threads }
        // `-studio-thread-match <text>`: the first thread whose title has it
        // (archived ones too), instead of the newest.
        let arguments = ProcessInfo.processInfo.arguments
        let match = arguments.firstIndex(of: "-studio-thread-match")
            .flatMap { arguments.indices.contains($0 + 1) ? arguments[$0 + 1] : nil }
        if launchSteps.contains("-studio-thread"),
           let thread = match.map({ text in browser.threads.first { $0.title.localizedCaseInsensitiveContains(text) } })
            ?? browser.threads.first(where: { !$0.archived }) {
            launchSteps.remove("-studio-thread")
            DispatchQueue.main.async { self.pushThread(thread.id) }
        }
        if launchSteps.contains("-studio-bench"), !browser.gallery.isEmpty, view.window != nil {
            launchSteps.remove("-studio-bench")
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { self.gallery.runBench() }
        }
        // `-studio-viewer-oldest`: on the oldest loaded creation, where the
        // viewer should page older history in.
        for step in ["-studio-viewer", "-studio-viewer-oldest"] where launchSteps.contains(step) {
            guard let item = step == "-studio-viewer" ? browser.gallery.last : browser.gallery.first,
                  view.window != nil else { continue }
            launchSteps.remove(step)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                self.openViewer(StudioViewerSession(
                    artifacts: self.browser.gallery.map(StudioArtifactDetail.init(item:)),
                    selectedId: item.id,
                    openedFromGallery: true,
                    openingPreview: self.browser.cachedPreview(artifactId: item.id)
                ))
            }
        }
    }

    private func restartWatches(deviceId: String?) {
        watches.forEach { $0.cancel() }
        watches = []
        guard let deviceId else { return }
        // Loading from now, not from when the tasks first run: the pages
        // would show "No creations yet" for a frame in between.
        browser.threadsLoading = true
        browser.galleryLoading = true
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

    private func showUnavailable(_ title: String, _ message: String, retry: Bool = true) {
        show(nil)
        contentUnavailableConfiguration = StudioUnavailable.configuration(
            title: title, message: message, symbol: "desktopcomputer.trianglebadge.exclamationmark",
            retry: retry ? { [weak self] in
                self?.syncHosts(force: true)
                self?.browser.reload()
            } : nil
        )
    }

    /// The desktops (a checkmark on the one shown, offline ones marked),
    /// then the shown desktop's providers.
    private func machineActions() -> [UIMenuElement] {
        let hosts: [UIMenuElement] = service.hosts.map { device in
            let online = service.isOnline(device.id)
            let action = UIAction(
                title: device.name,
                image: UIImage(systemName: online ? "desktopcomputer" : "desktopcomputer.trianglebadge.exclamationmark")
            ) { [weak self] _ in
                self?.browser.selectDevice(device.id)
            }
            action.state = browser.selectedDeviceId == device.id ? .on : .off
            if !online {
                action.subtitle = "Offline"
            } else if providerHosts[device.id] == false {
                action.subtitle = "No provider key"
            }
            return action
        }
        let desktops = UIMenu(title: "Desktop", options: .displayInline, children: hosts)
        guard let deviceId = browser.selectedDeviceId, service.isOnline(deviceId) else { return [desktops] }
        let providers = UIAction(title: "Providers…", image: UIImage(systemName: "key")) { [weak self] _ in
            guard let self else { return }
            self.presentStudioProviders(service: self.service, deviceId: deviceId) { [weak self] in
                self?.refreshProviderHosts([deviceId])
            }
        }
        providers.accessibilityIdentifier = "studio-providers-open"
        return [desktops, providers]
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
        // The viewer's requests name it: the root's own reference may already
        // be gone, and a viewer that asks must always be dismissed.
        weak var presented: StudioGalleryViewerController?
        let controller = StudioGalleryViewerController(
            session: session,
            browser: browser,
            workspace: service,
            deviceId: browser.selectedDeviceId,
            requestDismissal: { [weak self] in self?.dismissViewer(presented) },
            requestThread: { [weak self] threadId in
                self?.dismissViewer(presented) { self?.showThread(threadId) }
            },
            selectedArtifactChanged: { [weak transitionSource] artifactId in
                transitionSource?.prepareForDismissal(to: artifactId)
            }
        )
        presented = controller
        StudioViewerNativeTransition.configure(controller, session: session, source: transitionSource)
        viewer = controller
        presenter.present(controller, animated: true) { [weak self, weak controller] in
            controller?.presentationController?.delegate = self
        }
    }

    private func dismissViewer(_ target: StudioGalleryViewerController? = nil, completion: (() -> Void)? = nil) {
        guard let viewer = target ?? viewer else {
            completion?()
            return
        }
        viewer.session.presentationActive = false
        viewer.dismiss(animated: true) { [weak self, weak viewer] in
            // UIKit runs this for a dragged close that sprang back too, with
            // the viewer still up: forgetting it then left every later close
            // a no-op, and the viewer stuck with its controls switched off.
            if let viewer, viewer.presentingViewController != nil {
                viewer.session.presentationActive = true
                return
            }
            if self?.viewer == nil || self?.viewer === viewer { self?.viewer = nil }
            completion?()
        }
    }

    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
        viewer = nil
    }
}

/// Studio's empty and unavailable states, in the app's type; `retry` adds
/// a Try again button.
enum StudioUnavailable {
    static func configuration(title: String, message: String, symbol: String, retry: (() -> Void)? = nil) -> UIContentUnavailableConfiguration {
        var config = UIContentUnavailableConfiguration.empty()
        config.image = UIImage(systemName: symbol)
        config.imageProperties.tintColor = Palette.secondary
        config.text = title
        config.textProperties.font = Fonts.ui(.sansSemibold, 20)
        config.textProperties.color = Palette.text
        config.secondaryText = message
        config.secondaryTextProperties.font = Fonts.ui(.sans, 15)
        config.secondaryTextProperties.color = Palette.secondary
        if let retry {
            var button = UIButton.Configuration.glass()
            button.title = "Try again"
            button.baseForegroundColor = Palette.text
            config.button = button
            config.buttonProperties.primaryAction = UIAction { _ in retry() }
        }
        return config
    }
}
