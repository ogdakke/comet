import AVKit
import UIKit

enum StudioViewerSnapResolver {
    static func targetOffset(
        current: CGFloat,
        proposed: CGFloat,
        velocity: CGFloat,
        peek: CGFloat
    ) -> CGFloat {
        let proposed = max(0, proposed)
        if current < peek * 0.55 {
            return velocity > 0.28 || proposed > peek * 0.45 ? peek : 0
        }
        if current <= peek + 50 {
            if velocity < -0.28 { return 0 }
            if velocity > 0.62, proposed > peek + 60 { return proposed }
            return peek
        }
        return proposed < peek + 45 ? peek : proposed
    }
}

/// The viewer's media: previews for the pages around the selection, the
/// selected page's original (or its video stream), and nothing further out.
@MainActor
final class StudioViewerMediaStore {
    var changed: ((String) -> Void)?

    private struct OriginalRequest {
        let token: UUID
        let task: Task<Void, Never>
    }

    private let browser: StudioBrowserStore
    private var previews: [String: UIImage] = [:]
    private var images: [String: UIImage] = [:]
    private var players: [String: AVPlayer] = [:]
    private var videoStreams: [String: StudioVideoStream] = [:]
    private var previewTasks: [String: Task<Void, Never>] = [:]
    private var originalRequests: [String: OriginalRequest] = [:]
    private var failedOriginals: Set<String> = []
    private var retainedIds: Set<String> = []
    private var selectedId: String?
    private var artifacts: [String: StudioArtifactDetail] = [:]
    private var workspace: StudioService?
    private var deviceId: String?

    init(
        browser: StudioBrowserStore,
        openingPreview: UIImage?,
        openingPreviewArtifactId: String
    ) {
        self.browser = browser
        if let openingPreview { previews[openingPreviewArtifactId] = openingPreview }
    }

    func displayImage(for artifact: StudioArtifactDetail) -> UIImage? {
        images[artifact.id]
            ?? browser.cachedDisplayImage(artifactId: artifact.id)
            ?? previews[artifact.id]
            ?? browser.cachedPreview(artifactId: artifact.id)
            ?? browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio)
    }

    func originalState(for artifact: StudioArtifactDetail) -> StudioViewerOriginalState {
        guard artifact.mediaKind == .image else { return .ready }
        if images[artifact.id] != nil || browser.cachedDisplayImage(artifactId: artifact.id) != nil { return .ready }
        if failedOriginals.contains(artifact.id) { return .failed }
        return originalRequests[artifact.id] != nil ? .loading : .ready
    }

    func player(for artifactId: String) -> AVPlayer? { players[artifactId] }

    func prepare(
        selectedId: String,
        artifacts: [StudioArtifactDetail],
        workspace: StudioService,
        deviceId: String
    ) {
        guard let selectedIndex = artifacts.firstIndex(where: { $0.id == selectedId }) else { return }
        self.selectedId = selectedId
        self.workspace = workspace
        self.deviceId = deviceId

        let neighborhood = [0, -1, 1, -2, 2].compactMap { offset -> StudioArtifactDetail? in
            let index = selectedIndex + offset
            return artifacts.indices.contains(index) ? artifacts[index] : nil
        }
        retainedIds = Set(neighborhood.map(\.id))
        self.artifacts = Dictionary(neighborhood.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        trimOutsideNeighborhood()

        for (id, player) in players where id != selectedId { player.pause() }
        if let player = players[selectedId] {
            StudioViewerAudioSession.activate()
            player.play()
        }

        for artifact in neighborhood {
            loadPreview(artifact, workspace: workspace, deviceId: deviceId)
        }
        if let selected = neighborhood.first {
            loadOriginal(selected, workspace: workspace, deviceId: deviceId)
        }
    }

    /// Try a failed original again (the page's Retry).
    func retryOriginal(_ artifactId: String) {
        guard failedOriginals.remove(artifactId) != nil,
              let artifact = artifacts[artifactId],
              let workspace,
              let deviceId else { return }
        loadOriginal(artifact, workspace: workspace, deviceId: deviceId)
        changed?(artifactId)
    }

    func pausePlayback() {
        for player in players.values { player.pause() }
    }

    func resumeSelected() {
        guard let selectedId, let player = players[selectedId] else { return }
        player.play()
    }

    func reset() {
        previewTasks.values.forEach { $0.cancel() }
        previewTasks.removeAll()
        for id in Array(originalRequests.keys) { endOriginal(id) }
        for player in players.values { player.pause() }
        videoStreams.values.forEach { $0.cancel() }
        videoStreams.removeAll()
        previews.removeAll(keepingCapacity: false)
        images.removeAll(keepingCapacity: false)
        players.removeAll(keepingCapacity: false)
        failedOriginals.removeAll()
        retainedIds.removeAll(keepingCapacity: false)
        artifacts.removeAll()
        StudioViewerAudioSession.deactivate()
    }

    private func loadPreview(
        _ artifact: StudioArtifactDetail,
        workspace: StudioService,
        deviceId: String
    ) {
        if previews[artifact.id] == nil,
           let immediate = browser.cachedPreview(artifactId: artifact.id)
                ?? browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio) {
            previews[artifact.id] = immediate
            changed?(artifact.id)
        }
        guard previews[artifact.id] == nil, previewTasks[artifact.id] == nil else { return }
        let artifactId = artifact.id
        previewTasks[artifactId] = Task { [weak self, browser] in
            let image = await browser.preview(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            )
            guard let self, !Task.isCancelled else { return }
            previewTasks.removeValue(forKey: artifactId)
            guard retainedIds.contains(artifactId), let image else { return }
            previews[artifactId] = image
            changed?(artifactId)
        }
    }

    private func loadOriginal(
        _ artifact: StudioArtifactDetail,
        workspace: StudioService,
        deviceId: String
    ) {
        if artifact.mediaKind == .video {
            loadVideo(artifact, workspace: workspace, deviceId: deviceId)
            return
        }
        let artifactId = artifact.id
        guard images[artifactId] == nil,
              originalRequests[artifactId] == nil,
              !failedOriginals.contains(artifactId) else { return }
        if let cached = browser.cachedDisplayImage(artifactId: artifactId) {
            images[artifactId] = cached
            changed?(artifactId)
            return
        }

        let request = browser.beginDisplayImageRequest(
            artifact: artifact,
            deviceId: deviceId,
            workspace: workspace
        )
        let token = UUID()
        let task = Task { [weak self, browser] in
            let image = await request.value
            // Only the request still on file may finish it; a trimmed or reset
            // one was already ended by whoever removed it.
            guard let self, originalRequests[artifactId]?.token == token else { return }
            originalRequests.removeValue(forKey: artifactId)
            browser.endDisplayImageRequest(artifactId: artifactId)
            guard !Task.isCancelled, retainedIds.contains(artifactId) else { return }
            if let image {
                images[artifactId] = image
            } else {
                failedOriginals.insert(artifactId)
            }
            changed?(artifactId)
        }
        originalRequests[artifactId] = OriginalRequest(token: token, task: task)
        changed?(artifactId)
    }

    private func endOriginal(_ artifactId: String) {
        guard let request = originalRequests.removeValue(forKey: artifactId) else { return }
        request.task.cancel()
        browser.endDisplayImageRequest(artifactId: artifactId)
    }

    private func loadVideo(
        _ artifact: StudioArtifactDetail,
        workspace: StudioService,
        deviceId: String
    ) {
        StudioViewerAudioSession.activate()
        guard videoStreams[artifact.id] == nil else {
            players[artifact.id]?.play()
            return
        }
        let artifactId = artifact.id
        let stream = StudioVideoStream(
            artifactId: artifactId,
            mimeType: artifact.mimeType,
            declaredSize: artifact.sizeBytes,
            readChunk: { offset in
                try await workspace.readStudioArtifactChunk(
                    deviceId: deviceId,
                    artifactId: artifactId,
                    offset: offset
                )
            }
        )
        videoStreams[artifactId] = stream
        players[artifactId] = stream.player
        stream.player.play()
        changed?(artifactId)
    }

    private func trimOutsideNeighborhood() {
        for id in previewTasks.keys.filter({ !retainedIds.contains($0) }) {
            previewTasks.removeValue(forKey: id)?.cancel()
        }
        for id in originalRequests.keys.filter({ !retainedIds.contains($0) }) { endOriginal(id) }
        for id in previews.keys.filter({ !retainedIds.contains($0) }) { previews.removeValue(forKey: id) }
        for id in images.keys.filter({ !retainedIds.contains($0) }) { images.removeValue(forKey: id) }
        failedOriginals.formIntersection(retainedIds)
        for id in players.keys.filter({ $0 != selectedId }) {
            players[id]?.pause()
            players.removeValue(forKey: id)
            videoStreams.removeValue(forKey: id)?.cancel()
        }
    }
}

/// A view that only takes touches meant for its subviews, so the chrome's
/// empty stretches don't swallow taps, double-taps and pans on the image.
private final class StudioViewerPassthroughView: UIView {
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        let hit = super.hitTest(point, with: event)
        return hit === self ? nil : hit
    }
}

/// The full-screen viewer. Which gesture owns what:
///
/// - **Dismiss pan** (`dismissPan`, on the view): a mostly-downward drag
///   that starts with the details card closed and the page at minimum zoom.
///   It drives `StudioViewerInteractiveDismissal` 1:1. The details scroll
///   view's pan and the pager's paging pan both wait for it to fail, so
///   only one gesture ever moves the media.
/// - **Details scroll view** (vertical): everything else vertical — a swipe
///   up raises the details card from under the photo (the photo rises and
///   shrinks to stay above it), a swipe down lowers it again. It never goes
///   below zero, so pulling down on the card can't turn into a dismissal.
/// - **Pager** (horizontal): paging; the page's own scroll view owns pinch,
///   double-tap and panning while zoomed.
/// - No edge-swipe dismissal: it would fight the pager.
@MainActor
final class StudioGalleryViewerController: UIViewController,
    UIScrollViewDelegate,
    UIGestureRecognizerDelegate,
    StudioViewerPagerDelegate,
    StudioViewerFilmstripDelegate,
    UIContextMenuInteractionDelegate
{
    let session: StudioViewerSession
    /// Owns the custom zoom transition (see `StudioViewerNativeTransition`).
    var transitionController: StudioViewerTransitionController?

    private let browser: StudioBrowserStore
    private let workspace: StudioService?
    private let deviceId: String?
    private let requestDismissal: () -> Void
    /// The thread to show, and the creation to bring into view there.
    private let requestThread: (_ threadId: String, _ artifactId: String) -> Void
    private let selectedArtifactChanged: (String) -> Void
    private let media: StudioViewerMediaStore
    private let pager: StudioViewerPagerController
    private let filmstrip: StudioViewerFilmstripController

    private let backdrop = UIView()
    private let scrollView = UIScrollView()
    private let detailsCard = UIView()
    private let grabber = UIView()
    private let promptView = UITextView()
    private let detailsStack = UIStackView()
    private let cardActions = UIStackView()
    private let topChrome = StudioViewerPassthroughView()
    private let bottomChrome = StudioViewerPassthroughView()
    private let closeButton = UIButton(type: .system)
    private let moreButton = UIButton(type: .system)
    private let shareButton = UIButton(type: .system)
    private let infoButton = UIButton(type: .system)
    private let saveButton = UIButton(type: .system)
    private let deleteButton = UIButton(type: .system)
    private lazy var dismissPan = UIPanGestureRecognizer(target: self, action: #selector(handleDismissPan(_:)))
    private let cardHaptics = UIImpactFeedbackGenerator(style: .light)

    private enum BusyAction { case save, share, delete }

    private var artifacts: [StudioArtifactDetail]
    private var selectedId: String
    private var cardHeight: CGFloat = 320
    private var isDismissing = false
    private var isLoadingOlder = false
    private var busyAction: BusyAction?
    private var chromeHidden = false
    /// Chrome opacity a transition imposes on top of the viewer's own.
    private var transitionChrome: CGFloat = 1
    private var cardWasOpenAtDragStart = false
    private var activeDismissal: StudioViewerInteractiveDismissal?
    private var mediaPrepareTask: Task<Void, Never>?
    private var shareTask: Task<Void, Never>?
    private var didLayOutInitialSelection = false

    private static let buttonSide: CGFloat = 44
    private static let topChromeHeight: CGFloat = 60
    private static let filmstripHeight: CGFloat = 58
    private static let bottomChromeHeight: CGFloat = 58 + 8 + 44 + 8

    init(
        session: StudioViewerSession,
        browser: StudioBrowserStore,
        workspace: StudioService?,
        deviceId: String?,
        requestDismissal: @escaping () -> Void,
        requestThread: @escaping (_ threadId: String, _ artifactId: String) -> Void,
        selectedArtifactChanged: @escaping (String) -> Void = { _ in }
    ) {
        self.session = session
        self.browser = browser
        self.workspace = workspace
        self.deviceId = deviceId
        self.requestDismissal = requestDismissal
        self.requestThread = requestThread
        self.selectedArtifactChanged = selectedArtifactChanged
        artifacts = session.artifacts
        selectedId = session.selectedId

        let media = StudioViewerMediaStore(
            browser: browser,
            openingPreview: session.openingPreview,
            openingPreviewArtifactId: session.openingPreviewArtifactId
        )
        self.media = media
        pager = StudioViewerPagerController(
            artifacts: session.artifacts,
            selectedId: session.selectedId,
            media: media
        )
        filmstrip = StudioViewerFilmstripController(
            artifacts: session.artifacts,
            selectedId: session.selectedId,
            browser: browser,
            workspace: workspace,
            deviceId: deviceId
        )
        super.init(nibName: nil, bundle: nil)
        modalPresentationCapturesStatusBarAppearance = true
        // Media sits on black whatever the app's appearance, so the chrome,
        // details card and the sheets presented from here are dark too.
        overrideUserInterfaceStyle = .dark
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    /// Always the custom presentation that keeps the grid in the window (a
    /// `.fullScreen` set by the presenter would remove it from behind the
    /// dragged photo).
    override var modalPresentationStyle: UIModalPresentationStyle {
        get { transitionController == nil ? super.modalPresentationStyle : .custom }
        set { super.modalPresentationStyle = newValue }
    }

    override var prefersStatusBarHidden: Bool { chromeHidden }
    override var preferredStatusBarStyle: UIStatusBarStyle { .lightContent }
    override var preferredStatusBarUpdateAnimation: UIStatusBarAnimation { .fade }
    override var prefersHomeIndicatorAutoHidden: Bool { chromeHidden }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear

        backdrop.backgroundColor = .black
        view.addSubview(backdrop)

        scrollView.backgroundColor = .clear
        scrollView.alwaysBounceVertical = true
        scrollView.isDirectionalLockEnabled = true
        scrollView.showsVerticalScrollIndicator = false
        scrollView.contentInsetAdjustmentBehavior = .never
        scrollView.decelerationRate = .normal
        scrollView.delegate = self
        view.addSubview(scrollView)

        addChild(pager)
        scrollView.addSubview(pager.view)
        pager.didMove(toParent: self)
        pager.delegate = self
        pager.view.addInteraction(UIContextMenuInteraction(delegate: self))

        configureDetailsCard()
        scrollView.addSubview(detailsCard)

        addChild(filmstrip)
        bottomChrome.addSubview(filmstrip.view)
        filmstrip.didMove(toParent: self)
        filmstrip.delegate = self

        configureChrome()
        view.addSubview(topChrome)
        view.addSubview(bottomChrome)

        dismissPan.delegate = self
        dismissPan.maximumNumberOfTouches = 1
        view.addGestureRecognizer(dismissPan)
        // One owner per drag: these wait until the dismiss pan has declined.
        scrollView.panGestureRecognizer.require(toFail: dismissPan)
        pager.pagingPanGesture.require(toFail: dismissPan)

        media.changed = { [weak self] artifactId in
            self?.pager.refresh(artifactId)
            self?.filmstrip.refresh(artifactId)
        }
        prepareSelectedMedia()
        rebuildDetails()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        let bounds = view.bounds
        let safe = view.safeAreaInsets
        backdrop.frame = bounds
        scrollView.frame = bounds
        pager.view.bounds = CGRect(origin: .zero, size: bounds.size)
        pager.view.center = CGPoint(x: bounds.midX, y: bounds.midY)
        pager.playerChromeInsets = UIEdgeInsets(
            top: Self.topChromeHeight,
            left: 0,
            bottom: Self.bottomChromeHeight,
            right: 0
        )

        cardHeight = layoutDetailsCard(width: bounds.width)
        // The card's surface runs on past its content so a bounce at the
        // bottom never shows a gap under it.
        detailsCard.frame = CGRect(x: 0, y: bounds.height, width: bounds.width, height: cardHeight + bounds.height)
        scrollView.contentSize = CGSize(width: bounds.width, height: bounds.height + cardHeight)

        let side = Self.buttonSide
        let leading = safe.left + 16
        let trailing = bounds.width - safe.right - 16 - side
        topChrome.frame = CGRect(x: 0, y: 0, width: bounds.width, height: safe.top + Self.topChromeHeight)
        closeButton.frame = CGRect(x: leading, y: safe.top + 8, width: side, height: side)
        moreButton.frame = CGRect(x: trailing, y: safe.top + 8, width: side, height: side)

        let bottomHeight = safe.bottom + Self.bottomChromeHeight
        bottomChrome.frame = CGRect(
            x: 0,
            y: bounds.height - bottomHeight,
            width: bounds.width,
            height: bottomHeight
        )
        filmstrip.view.frame = CGRect(x: 0, y: 0, width: bounds.width, height: Self.filmstripHeight)
        let buttonY = Self.filmstripHeight + 8
        shareButton.frame = CGRect(x: leading, y: buttonY, width: side, height: side)
        infoButton.frame = CGRect(x: bounds.midX - side - 8, y: buttonY, width: side, height: side)
        saveButton.frame = CGRect(x: bounds.midX + 8, y: buttonY, width: side, height: side)
        deleteButton.frame = CGRect(x: trailing, y: buttonY, width: side, height: side)

        if !didLayOutInitialSelection {
            didLayOutInitialSelection = true
            pager.select(selectedId, animated: false)
            filmstrip.select(selectedId, animated: false)
        }
        updateMediaForCard()
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        session.presentationActive = true
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isBeingDismissed || presentingViewController == nil {
            session.presentationActive = false
            mediaPrepareTask?.cancel()
            shareTask?.cancel()
            media.reset()
        }
    }

    override func viewWillTransition(to size: CGSize, with coordinator: any UIViewControllerTransitionCoordinator) {
        super.viewWillTransition(to: size, with: coordinator)
        // The card's open offset was measured for the old height.
        let cardOpen = scrollView.contentOffset.y > 1
        coordinator.animate(alongsideTransition: { [weak self] _ in
            guard let self else { return }
            scrollView.contentOffset.y = cardOpen ? cardOpenOffset : 0
            updateMediaForCard()
        })
    }

    // MARK: Transition hooks

    /// What the transition flies: the page's current image, and the live
    /// player for a video (so it never freezes on a stale frame).
    var transitionContent: (image: UIImage?, player: AVPlayer?) {
        guard let selected else { return (nil, nil) }
        return (media.displayImage(for: selected), media.player(for: selected.id))
    }

    /// Backdrop opacity, chrome opacity, and whether the pager's own copy of
    /// the media is hidden while the transition view stands in for it.
    func setTransitionState(backdrop backdropAlpha: CGFloat, chrome: CGFloat, mediaHidden: Bool) {
        backdrop.alpha = backdropAlpha
        transitionChrome = chrome
        pager.view.alpha = mediaHidden ? 0 : 1
        detailsCard.alpha = chrome
        updateChromeAlpha()
    }

    /// A dragged close that sprang back.
    func interactiveDismissalDidCancel() {
        activeDismissal = nil
        isDismissing = false
        scrollView.isScrollEnabled = true
        media.resumeSelected()
        updateChromeAlpha()
        setNeedsStatusBarAppearanceUpdate()
    }

    func transitionImageFrame(in coordinateView: UIView) -> CGRect {
        if let frame = pager.transitionImageFrame(in: coordinateView) { return frame }
        // Before the page's cell exists, the image lands aspect-fit on screen.
        return view.convert(fittedMediaRect, to: coordinateView)
    }

    // MARK: Pager and filmstrip

    func viewerPager(_ pager: StudioViewerPagerController, settledOn artifactId: String) {
        select(artifactId, source: .pager)
    }

    func viewerPagerDidTapPage(_ pager: StudioViewerPagerController) {
        if scrollView.contentOffset.y > 1 {
            setCardOpen(false)
            return
        }
        setChromeHidden(!chromeHidden)
    }

    func viewerFilmstrip(
        _ filmstrip: StudioViewerFilmstripController,
        centeredOn artifactId: String,
        settled: Bool
    ) {
        select(artifactId, source: .filmstrip, prepareImmediately: settled)
    }

    // MARK: Dismiss pan

    func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard gestureRecognizer === dismissPan else { return true }
        guard transitionController != nil,
              !isDismissing,
              presentedViewController == nil,
              scrollView.contentOffset.y <= 0.5,
              !scrollView.isDecelerating,
              !pager.isCurrentPageZoomed else { return false }
        let velocity = dismissPan.velocity(in: view)
        // Down, and clearly more down than sideways; the rest is paging and the card.
        return velocity.y > 0 && velocity.y > abs(velocity.x) * 1.2
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        guard gestureRecognizer === dismissPan, let touched = touch.view else { return true }
        // The chrome's own controls (buttons, the filmstrip) and AVKit's.
        if touched is UIControl { return false }
        if topChrome.alpha > 0.05, touched.isDescendant(of: topChrome) { return false }
        if bottomChrome.alpha > 0.05, touched.isDescendant(of: bottomChrome) { return false }
        return true
    }

    @objc private func handleDismissPan(_ gesture: UIPanGestureRecognizer) {
        switch gesture.state {
        case .began:
            guard let transitionController else { return }
            let dismissal = StudioViewerInteractiveDismissal(
                transition: transitionController,
                viewer: self,
                touch: gesture.location(in: view)
            )
            activeDismissal = dismissal
            transitionController.interactiveDismissal = dismissal
            // A paused frame is what the player layer holds while dragged.
            media.pausePlayback()
            dismissViewer()
        case .changed:
            activeDismissal?.update(translation: gesture.translation(in: view))
        case .ended:
            activeDismissal?.update(translation: gesture.translation(in: view))
            activeDismissal?.end(velocity: gesture.velocity(in: view))
        case .cancelled, .failed:
            activeDismissal?.update(translation: .zero)
            activeDismissal?.end(velocity: .zero)
        default:
            break
        }
    }

    // MARK: Details card

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        // The card rises from underneath; there's nothing above the photo.
        if scrollView.contentOffset.y < 0 { scrollView.contentOffset.y = 0 }
        updateMediaForCard()
        updateChromeAlpha()
    }

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        cardWasOpenAtDragStart = scrollView.contentOffset.y > cardOpenOffset * 0.5
        cardHaptics.prepare()
        if chromeHidden { setChromeHidden(false) }
    }

    func scrollViewWillEndDragging(
        _ scrollView: UIScrollView,
        withVelocity velocity: CGPoint,
        targetContentOffset: UnsafeMutablePointer<CGPoint>
    ) {
        let open = cardOpenOffset
        let current = scrollView.contentOffset.y
        let proposed = targetContentOffset.pointee.y
        let target: CGFloat
        if current > open + 1, proposed > open {
            // Reading further down a tall card: ordinary scrolling.
            target = proposed
        } else if velocity.y > 0.3 {
            target = open
        } else if velocity.y < -0.3 {
            target = 0
        } else {
            target = current > open * 0.5 ? open : 0
        }
        targetContentOffset.pointee.y = target
        if (target > 0) != cardWasOpenAtDragStart {
            cardHaptics.impactOccurred()
            if target > 0 { UIAccessibility.post(notification: .layoutChanged, argument: promptView) }
        }
    }

    /// How far the card rises when open: all of it, up to most of the screen
    /// (a longer prompt scrolls on from there).
    private var cardOpenOffset: CGFloat {
        min(cardHeight, view.bounds.height * 0.62)
    }

    /// The page's image, aspect-fit on screen with no zoom.
    private var fittedMediaRect: CGRect {
        let ratio = max(0.01, selected?.aspectRatio ?? 1)
        return AVMakeRect(aspectRatio: CGSize(width: ratio, height: 1), insideRect: view.bounds)
    }

    /// Keep the photo above the rising card: it moves up with the card only
    /// as far as needed and shrinks once the space above runs out.
    private func updateMediaForCard() {
        let offset = scrollView.contentOffset.y
        let height = view.bounds.height
        guard offset > 0, height > 0 else {
            pager.view.transform = .identity
            return
        }
        let travel = min(offset, cardOpenOffset)
        let top = view.safeAreaInsets.top + 12
        let room = max(80, height - travel - 16 - top)
        let fitted = fittedMediaRect
        let scale = min(1, room / max(fitted.height, 1))
        let center = min(fitted.midY, top + room / 2)
        // In content coordinates: the pager scrolls up by `offset`; past the
        // card's open height it rides along with the card.
        let shift = center - fitted.midY + travel
        pager.view.transform = CGAffineTransform(translationX: 0, y: shift).scaledBy(x: scale, y: scale)
    }

    private func setCardOpen(_ open: Bool) {
        let target = open ? cardOpenOffset : 0
        guard abs(scrollView.contentOffset.y - target) > 0.5 else { return }
        if chromeHidden { setChromeHidden(false) }
        cardHaptics.impactOccurred()
        if UIAccessibility.isReduceMotionEnabled {
            scrollView.setContentOffset(CGPoint(x: 0, y: target), animated: false)
        } else {
            let animator = UIViewPropertyAnimator(duration: 0.5, dampingRatio: 0.86) {
                self.scrollView.contentOffset.y = target
            }
            animator.startAnimation()
        }
        if open { UIAccessibility.post(notification: .layoutChanged, argument: promptView) }
    }

    private func configureDetailsCard() {
        // Its own surface, lighter than the backdrop, so it reads as a card
        // over (not a continuation of) the black around the photo.
        detailsCard.backgroundColor = Palette.chip
        detailsCard.layer.cornerRadius = 28
        detailsCard.layer.cornerCurve = .continuous
        detailsCard.layer.maskedCorners = [.layerMinXMinYCorner, .layerMaxXMinYCorner]

        grabber.backgroundColor = Palette.tertiary.withAlphaComponent(0.6)
        grabber.layer.cornerRadius = 2.5
        grabber.isAccessibilityElement = false
        detailsCard.addSubview(grabber)

        // Selectable, so the prompt can be copied in part or whole.
        promptView.isEditable = false
        promptView.isSelectable = true
        promptView.isScrollEnabled = false
        promptView.backgroundColor = .clear
        promptView.textContainerInset = .zero
        promptView.textContainer.lineFragmentPadding = 0
        promptView.font = Fonts.ui(.sansMedium, TypeScale.size(17))
        promptView.textColor = Palette.text
        promptView.accessibilityLabel = "Prompt"
        detailsCard.addSubview(promptView)

        detailsStack.axis = .vertical
        detailsStack.spacing = 12
        detailsCard.addSubview(detailsStack)

        cardActions.axis = .horizontal
        cardActions.distribution = .fillEqually
        cardActions.spacing = 10
        var buttons = [
            cardActionButton("Save", symbol: "square.and.arrow.down") { [weak self] in self?.saveSelected() },
            cardActionButton("Share", symbol: "square.and.arrow.up") { [weak self] in self?.shareSelected() },
        ]
        // From a thread, the viewer is already over that thread.
        if session.openedFromGallery {
            buttons.append(cardActionButton("Thread", symbol: "rectangle.stack") { [weak self] in
                guard let self, let selected else { return }
                requestThread(selected.conversationId, selected.id)
            })
        }
        let delete = cardActionButton("Delete", symbol: "trash", destructive: true) { [weak self] in
            self?.confirmDelete(from: self?.cardActions.arrangedSubviews.last)
        }
        buttons.append(delete)
        let connected = workspace != nil && deviceId != nil
        for button in buttons {
            if button.accessibilityLabel != "Thread" { button.isEnabled = connected }
            cardActions.addArrangedSubview(button)
        }
        detailsCard.addSubview(cardActions)
    }

    private func cardActionButton(
        _ title: String,
        symbol: String,
        destructive: Bool = false,
        action: @escaping () -> Void
    ) -> UIButton {
        var configuration = UIButton.Configuration.gray()
        configuration.image = UIImage(
            systemName: symbol,
            withConfiguration: UIImage.SymbolConfiguration(pointSize: 17, weight: .medium)
        )
        configuration.imagePlacement = .top
        configuration.imagePadding = 6
        configuration.baseForegroundColor = destructive ? Palette.danger : Palette.text
        configuration.background.backgroundColor = Palette.controlFill
        configuration.cornerStyle = .large
        configuration.contentInsets = NSDirectionalEdgeInsets(top: 12, leading: 4, bottom: 10, trailing: 4)
        configuration.attributedTitle = AttributedString(
            title,
            attributes: AttributeContainer([.font: Fonts.ui(.sansMedium, TypeScale.size(12))])
        )
        let button = UIButton(configuration: configuration, primaryAction: UIAction { _ in action() })
        button.accessibilityLabel = title
        return button
    }

    private func rebuildDetails() {
        promptView.text = selected?.prompt
        detailsStack.arrangedSubviews.forEach {
            detailsStack.removeArrangedSubview($0)
            $0.removeFromSuperview()
        }
        guard let selected else { return }
        detailsStack.addArrangedSubview(detailRow("Model", selected.modelDisplayName))
        if let width = selected.width, let height = selected.height {
            detailsStack.addArrangedSubview(detailRow("Dimensions", "\(width) × \(height)"))
        }
        if let seconds = selected.durationSeconds {
            let total = max(0, Int(seconds.rounded()))
            detailsStack.addArrangedSubview(detailRow("Duration", String(format: "%d:%02d", total / 60, total % 60)))
        }
        detailsStack.addArrangedSubview(detailRow(
            "Size",
            ByteCountFormatter.string(fromByteCount: Int64(clamping: selected.sizeBytes), countStyle: .file)
        ))
        if selected.createdDate != .distantPast {
            detailsStack.addArrangedSubview(detailRow(
                "Created",
                selected.createdDate.formatted(date: .abbreviated, time: .shortened)
            ))
        }
        if isViewLoaded { view.setNeedsLayout() }
    }

    private func detailRow(_ title: String, _ value: String) -> UIView {
        let row = UIView()
        let titleLabel = UILabel()
        titleLabel.text = title
        titleLabel.font = Fonts.ui(.sans, TypeScale.size(14))
        titleLabel.textColor = Palette.secondary
        titleLabel.setContentHuggingPriority(.required, for: .horizontal)
        titleLabel.setContentCompressionResistancePriority(.required, for: .horizontal)
        titleLabel.translatesAutoresizingMaskIntoConstraints = false
        row.addSubview(titleLabel)

        let valueLabel = UILabel()
        valueLabel.text = value
        valueLabel.font = Fonts.ui(.sans, TypeScale.size(14))
        valueLabel.textColor = Palette.text
        valueLabel.textAlignment = .right
        valueLabel.lineBreakMode = .byTruncatingMiddle
        valueLabel.translatesAutoresizingMaskIntoConstraints = false
        row.addSubview(valueLabel)

        row.isAccessibilityElement = true
        row.accessibilityLabel = title
        row.accessibilityValue = value

        NSLayoutConstraint.activate([
            titleLabel.leadingAnchor.constraint(equalTo: row.leadingAnchor),
            titleLabel.topAnchor.constraint(equalTo: row.topAnchor),
            titleLabel.bottomAnchor.constraint(equalTo: row.bottomAnchor),
            valueLabel.leadingAnchor.constraint(greaterThanOrEqualTo: titleLabel.trailingAnchor, constant: 12),
            valueLabel.trailingAnchor.constraint(equalTo: row.trailingAnchor),
            valueLabel.topAnchor.constraint(equalTo: row.topAnchor),
            valueLabel.bottomAnchor.constraint(equalTo: row.bottomAnchor),
            row.heightAnchor.constraint(greaterThanOrEqualToConstant: 20),
        ])
        return row
    }

    /// Lays out the card's content and returns the height it needs.
    private func layoutDetailsCard(width: CGFloat) -> CGFloat {
        let safe = view.safeAreaInsets
        let x = 20 + safe.left
        let contentWidth = max(0, width - 40 - safe.left - safe.right)
        grabber.frame = CGRect(x: (width - 36) / 2, y: 8, width: 36, height: 5)

        var y: CGFloat = 32
        let hasPrompt = !(promptView.text ?? "").isEmpty
        promptView.isHidden = !hasPrompt
        if hasPrompt {
            let size = promptView.sizeThatFits(CGSize(width: contentWidth, height: .greatestFiniteMagnitude))
            promptView.frame = CGRect(x: x, y: y, width: contentWidth, height: ceil(size.height))
            y = promptView.frame.maxY + 22
        }
        let stackSize = detailsStack.systemLayoutSizeFitting(
            CGSize(width: contentWidth, height: UIView.layoutFittingCompressedSize.height),
            withHorizontalFittingPriority: .required,
            verticalFittingPriority: .fittingSizeLevel
        )
        detailsStack.frame = CGRect(x: x, y: y, width: contentWidth, height: stackSize.height)
        y = detailsStack.frame.maxY + 22
        let actionsHeight = max(64, cardActions.systemLayoutSizeFitting(UIView.layoutFittingCompressedSize).height)
        cardActions.frame = CGRect(x: x, y: y, width: contentWidth, height: actionsHeight)
        return cardActions.frame.maxY + max(safe.bottom, 16) + 8
    }

    // MARK: Selection and media

    private enum SelectionSource: Equatable { case pager, filmstrip }

    private var selected: StudioArtifactDetail? {
        artifacts.first(where: { $0.id == selectedId })
    }

    private func select(
        _ artifactId: String,
        source: SelectionSource,
        prepareImmediately: Bool = true
    ) {
        guard artifacts.contains(where: { $0.id == artifactId }) else { return }
        if artifactId == selectedId {
            if prepareImmediately {
                mediaPrepareTask?.cancel()
                rebuildDetails()
                prepareSelectedMedia()
                loadOlderIfNeeded()
            }
            return
        }
        selectedId = artifactId
        session.selectedId = artifactId
        if prepareImmediately { selectedArtifactChanged(artifactId) }
        if source != .pager { pager.select(artifactId, animated: source != .filmstrip) }
        if source != .filmstrip { filmstrip.select(artifactId, animated: true) }
        if source != .filmstrip || prepareImmediately { rebuildDetails() }
        if prepareImmediately {
            mediaPrepareTask?.cancel()
            prepareSelectedMedia()
            loadOlderIfNeeded()
        } else {
            // The cached thumbnail switches synchronously. Delay only relay
            // work so a fast fling does not download every crossed original.
            pager.refresh(artifactId)
            mediaPrepareTask?.cancel()
            mediaPrepareTask = Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(100))
                guard !Task.isCancelled, let self, self.selectedId == artifactId else { return }
                self.selectedArtifactChanged(artifactId)
                self.prepareSelectedMedia()
                self.loadOlderIfNeeded()
            }
        }
    }

    private func prepareSelectedMedia() {
        guard let workspace, let deviceId else { return }
        media.prepare(
            selectedId: selectedId,
            artifacts: artifacts,
            workspace: workspace,
            deviceId: deviceId
        )
        pager.refresh(selectedId)
    }

    /// Near the oldest loaded creation, ask for the page before it. The
    /// page reaches the pager and filmstrip through `updateProperties`,
    /// whoever loaded it: the grid underneath pages too (the close
    /// transition scrolls it to the current item), and a load already
    /// running makes this request a no-op.
    private func loadOlderIfNeeded() {
        guard session.openedFromGallery,
              !isLoadingOlder,
              browser.galleryHasMore,
              let index = artifacts.firstIndex(where: { $0.id == selectedId }),
              index < Self.olderThreshold,
              let workspace,
              let deviceId else { return }
        isLoadingOlder = true
        Task { [weak self] in
            guard let self else { return }
            defer { isLoadingOlder = false }
            await browser.loadMoreGallery(workspace: workspace, deviceId: deviceId)
        }
    }

    /// How close to the oldest loaded creation the next page is asked for:
    /// a fast filmstrip swipe crosses a few dozen in a second.
    private static let olderThreshold = 40

    /// Tracks the gallery (reads here re-run when it changes): older pages,
    /// new creations and deletions reach a viewer opened from it.
    override func updateProperties() {
        super.updateProperties()
        guard session.openedFromGallery, !isDismissing else { return }
        let gallery = browser.gallery
        guard gallery.count != artifacts.count
            || gallery.first?.id != artifacts.first?.id
            || gallery.last?.id != artifacts.last?.id else { return }
        let current = selectedId
        guard gallery.contains(where: { $0.id == current }) else { return }
        let updated = gallery.map(StudioArtifactDetail.init(item:))
        artifacts = updated
        session.replaceArtifacts(with: updated)
        pager.replaceArtifacts(updated)
        filmstrip.replaceArtifacts(updated)
        // Still near the old edge (a short page): keep going.
        loadOlderIfNeeded()
    }

    // MARK: Chrome

    private func configureChrome() {
        topChrome.backgroundColor = .clear
        bottomChrome.backgroundColor = .clear

        configureCircleButton(closeButton, systemName: "chevron.backward") { [weak self] in
            self?.dismissViewer()
        }
        closeButton.accessibilityLabel = "Close"
        topChrome.addSubview(closeButton)

        configureCircleButton(moreButton, systemName: "ellipsis", action: nil)
        moreButton.showsMenuAsPrimaryAction = true
        // Built on open, so it always reflects the page on screen.
        moreButton.menu = UIMenu(children: [UIDeferredMenuElement.uncached { [weak self] completion in
            completion(self?.actions(source: self?.moreButton) ?? [])
        }])
        moreButton.accessibilityLabel = "More"
        topChrome.addSubview(moreButton)

        configureCircleButton(shareButton, systemName: "square.and.arrow.up") { [weak self] in
            self?.shareSelected()
        }
        shareButton.accessibilityLabel = "Share"
        bottomChrome.addSubview(shareButton)

        configureCircleButton(infoButton, systemName: "info.circle") { [weak self] in
            guard let self else { return }
            setCardOpen(scrollView.contentOffset.y < 1)
        }
        infoButton.accessibilityLabel = "Details"
        bottomChrome.addSubview(infoButton)

        configureCircleButton(saveButton, systemName: "square.and.arrow.down") { [weak self] in
            self?.saveSelected()
        }
        saveButton.accessibilityLabel = "Save to Photos"
        bottomChrome.addSubview(saveButton)

        configureCircleButton(deleteButton, systemName: "trash", destructive: true) { [weak self] in
            self?.confirmDelete(from: self?.deleteButton)
        }
        deleteButton.accessibilityLabel = "Delete"
        bottomChrome.addSubview(deleteButton)

        // Nothing to save, share or delete without a connected machine.
        let connected = workspace != nil && deviceId != nil
        for button in [shareButton, saveButton, deleteButton] { button.isEnabled = connected }
    }

    private func configureCircleButton(
        _ button: UIButton,
        systemName: String,
        destructive: Bool = false,
        action: (() -> Void)?
    ) {
        var configuration = UIButton.Configuration.glass()
        configuration.image = UIImage(
            systemName: systemName,
            withConfiguration: UIImage.SymbolConfiguration(pointSize: 17, weight: .medium)
        )
        configuration.baseForegroundColor = destructive ? Palette.danger : .white
        configuration.cornerStyle = .capsule
        button.configuration = configuration
        if let action { button.addAction(UIAction { _ in action() }, for: .touchUpInside) }
    }

    private func setChromeHidden(_ hidden: Bool) {
        guard chromeHidden != hidden else { return }
        chromeHidden = hidden
        let apply = {
            self.updateChromeAlpha()
            self.setNeedsStatusBarAppearanceUpdate()
        }
        if UIAccessibility.isReduceMotionEnabled {
            apply()
        } else {
            UIView.animate(withDuration: 0.22, delay: 0, options: [.beginFromCurrentState, .allowUserInteraction], animations: apply)
        }
        setNeedsUpdateOfHomeIndicatorAutoHidden()
    }

    /// The chrome fades as the card comes up, and is gone while hidden or
    /// while a transition has the media.
    private func updateChromeAlpha() {
        let offset = max(0, scrollView.contentOffset.y)
        let scrolled = max(0, min(1, 1 - offset / 88))
        let alpha = chromeHidden ? 0 : scrolled * transitionChrome
        topChrome.alpha = alpha
        bottomChrome.alpha = alpha
        // Faded-out chrome mustn't catch taps meant for the media, and
        // VoiceOver shouldn't find invisible buttons.
        let live = alpha > 0.05 && !isDismissing
        topChrome.isUserInteractionEnabled = live
        bottomChrome.isUserInteractionEnabled = live
        topChrome.accessibilityElementsHidden = !live
        bottomChrome.accessibilityElementsHidden = !live
    }

    func contextMenuInteraction(
        _ interaction: UIContextMenuInteraction,
        configurationForMenuAtLocation location: CGPoint
    ) -> UIContextMenuConfiguration? {
        // Videos keep their long press for AVKit; a zoomed image would lift
        // past the screen.
        guard pager.currentImageView != nil, !isDismissing else { return nil }
        return UIContextMenuConfiguration(identifier: selectedId as NSString, previewProvider: nil) { [weak self] _ in
            guard let self else { return nil }
            return UIMenu(children: actions(source: nil))
        }
    }

    func contextMenuInteraction(
        _ interaction: UIContextMenuInteraction,
        configuration: UIContextMenuConfiguration,
        highlightPreviewForItemWithIdentifier identifier: any NSCopying
    ) -> UITargetedPreview? {
        // Lift the image itself, not the whole full-screen pager.
        guard let imageView = pager.currentImageView, imageView.window != nil else { return nil }
        let parameters = UIPreviewParameters()
        parameters.backgroundColor = .clear
        return UITargetedPreview(view: imageView, parameters: parameters)
    }

    private func actions(source: UIView?) -> [UIMenuElement] {
        guard let selected else { return [] }
        let connected = workspace != nil && deviceId != nil
        var primary: [UIMenuElement] = []
        if connected {
            primary.append(UIAction(title: "Save to Photos", image: UIImage(systemName: "square.and.arrow.down")) { [weak self] _ in
                self?.saveSelected()
            })
            primary.append(UIAction(title: "Share…", image: UIImage(systemName: "square.and.arrow.up")) { [weak self] _ in
                self?.shareSelected()
            })
        }
        if !selected.prompt.isEmpty {
            primary.append(UIAction(title: "Copy Prompt", image: UIImage(systemName: "doc.on.doc")) { [weak self] _ in
                UIPasteboard.general.string = selected.prompt
                Toast.show("Copied", in: self?.view.window, above: self?.toastAnchor)
            })
        }
        primary.append(UIAction(title: "Details", image: UIImage(systemName: "info.circle")) { [weak self] _ in
            self?.setCardOpen(true)
        })
        // From a thread, the viewer is already over that thread.
        if session.openedFromGallery {
            primary.append(UIAction(title: "Show in Thread", image: UIImage(systemName: "rectangle.stack")) { [weak self] _ in
                self?.requestThread(selected.conversationId, selected.id)
            })
        }
        var menu: [UIMenuElement] = [UIMenu(options: .displayInline, children: primary)]
        if connected {
            menu.append(UIMenu(options: .displayInline, children: [UIAction(
                title: "Delete",
                image: UIImage(systemName: "trash"),
                attributes: .destructive
            ) { [weak self] _ in
                self?.confirmDelete(from: source)
            }]))
        }
        return menu
    }

    // MARK: Actions

    private func dismissViewer() {
        guard !isDismissing else { return }
        isDismissing = true
        scrollView.isScrollEnabled = false
        updateChromeAlpha()
        requestDismissal()
    }

    private func setBusy(_ action: BusyAction?) {
        busyAction = action
        let connected = workspace != nil && deviceId != nil
        for (button, kind) in [(shareButton, BusyAction.share), (saveButton, .save), (deleteButton, .delete)] {
            button.configuration?.showsActivityIndicator = action == kind
            button.isEnabled = connected && action == nil
        }
        let cardKinds: [BusyAction?] = session.openedFromGallery ? [.save, .share, nil, .delete] : [.save, .share, .delete]
        for (button, kind) in zip(cardActions.arrangedSubviews.compactMap { $0 as? UIButton }, cardKinds) {
            guard let kind else { continue }
            button.configuration?.showsActivityIndicator = action == kind
            button.isEnabled = connected && action == nil
        }
    }

    private func saveSelected() {
        guard let selected, let workspace, let deviceId, busyAction == nil else { return }
        setBusy(.save)
        Task { [weak self] in
            do {
                try await StudioArtifactActions.download(selected, workspace: workspace, deviceId: deviceId)
                guard let self else { return }
                UINotificationFeedbackGenerator().notificationOccurred(.success)
                Toast.show(
                    selected.mediaKind == .video ? "Video saved to Photos" : "Saved to Photos",
                    in: view.window,
                    above: toastAnchor
                )
            } catch StudioArtifactActionError.photoAccessDenied {
                self?.presentPhotoAccessDenied()
            } catch {
                self?.presentError("Couldn't save to Photos", error)
            }
            self?.setBusy(nil)
        }
    }

    /// What a toast floats above: the open card's buttons (a toast at the
    /// bottom chrome's height landed on them), else the bottom bar.
    private var toastAnchor: UIView? {
        if scrollView.contentOffset.y > 1 { return cardActions }
        return bottomChrome.alpha > 0.05 ? bottomChrome : nil
    }

    private func shareSelected() {
        guard let selected, let workspace, let deviceId, busyAction == nil else { return }
        setBusy(.share)
        shareTask = Task { [weak self] in
            do {
                let url = try await StudioArtifactActions.shareableFile(selected, workspace: workspace, deviceId: deviceId)
                guard let self, !Task.isCancelled, !isDismissing, view.window != nil, presentedViewController == nil else {
                    StudioArtifactActions.removeShareableFile(url)
                    self?.setBusy(nil)
                    return
                }
                let sheet = UIActivityViewController(activityItems: [url], applicationActivities: nil)
                sheet.completionWithItemsHandler = { _, _, _, _ in
                    StudioArtifactActions.removeShareableFile(url)
                }
                anchor(sheet.popoverPresentationController, to: shareButton)
                present(sheet, animated: true)
            } catch is CancellationError {
            } catch {
                self?.presentError("Couldn't share", error)
            }
            self?.setBusy(nil)
        }
    }

    private func confirmDelete(from source: UIView?) {
        guard let selected, busyAction == nil else { return }
        let sheet = UIAlertController(
            title: selected.mediaKind == .video ? "Delete this video?" : "Delete this image?",
            message: "This removes it from Studio on the selected machine.",
            preferredStyle: .actionSheet
        )
        sheet.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak self] _ in
            self?.deleteSelected()
        })
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        // An action sheet without an anchor crashes on iPad.
        anchor(sheet.popoverPresentationController, to: source ?? deleteButton)
        present(sheet, animated: true)
    }

    private func deleteSelected() {
        guard let selected, let workspace, let deviceId, busyAction == nil else { return }
        setBusy(.delete)
        Task { [weak self] in
            do {
                try await workspace.deleteStudioArtifact(
                    deviceId: deviceId,
                    artifactId: selected.id
                )
                guard let self else { return }
                browser.removeArtifact(selected.id)
                remove(selected.id)
            } catch {
                self?.presentError("Couldn't delete", error)
            }
            self?.setBusy(nil)
        }
    }

    /// Drop a deleted artifact and land on its neighbor, like Photos; close
    /// once nothing is left.
    private func remove(_ artifactId: String) {
        guard !isDismissing, let index = artifacts.firstIndex(where: { $0.id == artifactId }) else { return }
        var remaining = artifacts
        remaining.remove(at: index)
        guard !remaining.isEmpty else {
            dismissViewer()
            return
        }
        let next = remaining[min(index, remaining.count - 1)].id
        artifacts = remaining
        selectedId = next
        session.replaceArtifacts(with: remaining)
        session.selectedId = next
        UIView.transition(with: pager.view, duration: 0.25, options: [.transitionCrossDissolve, .allowUserInteraction]) {
            self.pager.replaceArtifacts(remaining, selecting: next)
        }
        filmstrip.replaceArtifacts(remaining, selecting: next)
        selectedArtifactChanged(next)
        mediaPrepareTask?.cancel()
        rebuildDetails()
        prepareSelectedMedia()
        UIAccessibility.post(notification: .screenChanged, argument: nil)
    }

    private func anchor(_ popover: UIPopoverPresentationController?, to source: UIView) {
        guard let popover else { return }
        if source.window != nil, source.alpha > 0.05, (source.superview?.alpha ?? 1) > 0.05 {
            popover.sourceView = source
            popover.sourceRect = source.bounds
        } else {
            popover.sourceView = view
            popover.sourceRect = CGRect(x: view.bounds.midX, y: view.bounds.maxY - view.safeAreaInsets.bottom, width: 0, height: 0)
        }
    }

    private func presentPhotoAccessDenied() {
        guard viewIfLoaded?.window != nil, !isDismissing, presentedViewController == nil else { return }
        let alert = UIAlertController(
            title: "Allow Photos access",
            message: "Zeron needs permission to add to your photo library to save Studio media.",
            preferredStyle: .alert
        )
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Open Settings", style: .default) { _ in
            guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
            UIApplication.shared.open(url)
        })
        present(alert, animated: true)
    }

    private func presentError(_ title: String, _ error: Error) {
        guard viewIfLoaded?.window != nil, !isDismissing, presentedViewController == nil else { return }
        let alert = UIAlertController(title: title, message: error.localizedDescription, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }

#if DEBUG
    func selectFilmstripItemForTesting(at index: Int) {
        filmstrip.selectItemForTesting(at: index)
    }

    func centerFilmstripItemUsingCollectionGeometryForTesting(at index: Int) {
        filmstrip.centerItemUsingCollectionGeometryForTesting(at: index)
    }

    func runPerformanceFixtureSequence() {
        guard let initialIndex = artifacts.firstIndex(where: { $0.id == selectedId }) else { return }
        filmstrip.animateToItemForTesting(at: min(artifacts.count - 1, initialIndex + 24)) { [weak self] in
            guard let self else { return }
            self.filmstrip.animateToItemForTesting(at: initialIndex) { [weak self] in
                self?.runVerticalPerformanceFixtureSequence()
            }
        }
    }

    private func runVerticalPerformanceFixtureSequence() {
        let open = cardOpenOffset
        animateFixtureScroll(to: open, duration: 0.5, damping: 0.84) { [weak self] in
            guard let self else { return }
            self.animateFixtureScroll(to: open + 360, duration: 0.65, damping: 0.9) { [weak self] in
                guard let self else { return }
                self.animateFixtureScroll(to: open, duration: 0.55, damping: 0.84) { [weak self] in
                    guard let self else { return }
                    self.animateFixtureScroll(to: 0, duration: 0.5, damping: 0.84) { [weak self] in
                        self?.dismissViewer()
                    }
                }
            }
        }
    }

    private func animateFixtureScroll(
        to offset: CGFloat,
        duration: TimeInterval,
        damping: CGFloat,
        completion: @escaping () -> Void
    ) {
        let animator = UIViewPropertyAnimator(duration: duration, dampingRatio: damping) {
            self.scrollView.contentOffset.y = offset
        }
        animator.addCompletion { _ in completion() }
        animator.startAnimation()
    }

    var selectedArtifactIdForTesting: String { selectedId }
    var residentPageCellCountForTesting: Int { pager.residentCellCountForTesting }
    var residentFilmstripCellCountForTesting: Int { filmstrip.residentCellCountForTesting }
#endif
}

@MainActor
protocol StudioViewerFilmstripDelegate: AnyObject {
    func viewerFilmstrip(
        _ filmstrip: StudioViewerFilmstripController,
        centeredOn artifactId: String,
        settled: Bool
    )
}

private final class StudioCenteredFilmstripLayout: UICollectionViewFlowLayout {
    override func targetContentOffset(
        forProposedContentOffset proposedContentOffset: CGPoint,
        withScrollingVelocity velocity: CGPoint
    ) -> CGPoint {
        guard let collectionView else { return proposedContentOffset }
        let proposedCenter = proposedContentOffset.x + collectionView.bounds.width / 2
        let rect = CGRect(
            x: proposedContentOffset.x,
            y: 0,
            width: collectionView.bounds.width,
            height: collectionView.bounds.height
        )
        guard let closest = layoutAttributesForElements(in: rect)?
            .filter({ $0.representedElementCategory == .cell })
            .min(by: { abs($0.center.x - proposedCenter) < abs($1.center.x - proposedCenter) }) else {
            return proposedContentOffset
        }
        return CGPoint(x: closest.center.x - collectionView.bounds.width / 2, y: proposedContentOffset.y)
    }
}

@MainActor
final class StudioViewerFilmstripController: UIViewController,
    UICollectionViewDataSource,
    UICollectionViewDelegate
{
    weak var delegate: StudioViewerFilmstripDelegate?

    private let layout = StudioCenteredFilmstripLayout()
    private lazy var collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
    private let browser: StudioBrowserStore
    private let workspace: StudioService?
    private let deviceId: String?
    private var artifacts: [StudioArtifactDetail]
    private var artifactIndex: [String: Int]
    private var selectedId: String
    private var pendingSelectionId: String?
    private var lastWidth: CGFloat = 0
    private var userDriving = false
    private var lastReportedCenteredId: String?

    init(
        artifacts: [StudioArtifactDetail],
        selectedId: String,
        browser: StudioBrowserStore,
        workspace: StudioService?,
        deviceId: String?
    ) {
        self.artifacts = artifacts
        self.selectedId = selectedId
        self.browser = browser
        self.workspace = workspace
        self.deviceId = deviceId
        artifactIndex = Self.index(artifacts)
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        layout.scrollDirection = .horizontal
        layout.itemSize = CGSize(width: 48, height: 48)
        layout.minimumLineSpacing = 6

        collectionView.frame = view.bounds
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.showsHorizontalScrollIndicator = false
        collectionView.contentInsetAdjustmentBehavior = .never
        collectionView.decelerationRate = .normal
        collectionView.dataSource = self
        collectionView.delegate = self
        // The pager is how VoiceOver moves between pages; the strip would
        // just repeat every page as a second list.
        collectionView.accessibilityElementsHidden = true
        collectionView.register(
            StudioFilmstripCell.self,
            forCellWithReuseIdentifier: StudioFilmstripCell.reuseIdentifier
        )
        view.addSubview(collectionView)
        pendingSelectionId = selectedId
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        guard view.bounds.width > 0, view.bounds.width != lastWidth else { return }
        lastWidth = view.bounds.width
        // A new width re-centers every item; stay on the selected one.
        if pendingSelectionId == nil { pendingSelectionId = selectedId }
        let inset = max(0, (view.bounds.width - layout.itemSize.width) / 2)
        layout.sectionInset = UIEdgeInsets(top: 5, left: inset, bottom: 5, right: inset)
        layout.invalidateLayout()
        collectionView.layoutIfNeeded()
        positionPendingSelection()
    }

    func replaceArtifacts(_ artifacts: [StudioArtifactDetail], selecting: String? = nil) {
        let keepSelected = selecting ?? selectedId
        guard self.artifacts != artifacts || keepSelected != selectedId else { return }
        let anchor = selecting == nil ? StudioStripAnchor.capture(collectionView, ids: self.artifacts) : nil
        self.artifacts = artifacts
        artifactIndex = Self.index(artifacts)
        selectedId = keepSelected
        lastReportedCenteredId = keepSelected
        collectionView.reloadData()
        collectionView.layoutIfNeeded()
        if let anchor {
            StudioStripAnchor.restore(anchor, in: collectionView, index: artifactIndex[anchor.id])
            return
        }
        pendingSelectionId = selectedId
        positionPendingSelection()
    }

    func select(_ artifactId: String, animated: Bool) {
        guard let index = artifactIndex[artifactId] else { return }
        selectedId = artifactId
        refreshVisibleSelection(centeredId: artifactId)
        guard isViewLoaded, collectionView.bounds.width > 0 else {
            pendingSelectionId = artifactId
            return
        }
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: animated
        )
    }

    func refresh(_ artifactId: String) {
        guard let index = artifactIndex[artifactId],
              let cell = collectionView.cellForItem(at: IndexPath(item: index, section: 0)) as? StudioFilmstripCell else {
            return
        }
        configure(cell, at: index)
    }

    func collectionView(
        _ collectionView: UICollectionView,
        numberOfItemsInSection section: Int
    ) -> Int { artifacts.count }

    func collectionView(
        _ collectionView: UICollectionView,
        cellForItemAt indexPath: IndexPath
    ) -> UICollectionViewCell {
        guard let cell = collectionView.dequeueReusableCell(
            withReuseIdentifier: StudioFilmstripCell.reuseIdentifier,
            for: indexPath
        ) as? StudioFilmstripCell,
        artifacts.indices.contains(indexPath.item) else { return UICollectionViewCell() }
        configure(cell, at: indexPath.item)
        return cell
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        guard artifacts.indices.contains(indexPath.item) else { return }
        let id = artifacts[indexPath.item].id
        selectedId = id
        lastReportedCenteredId = id
        refreshVisibleSelection(centeredId: id)
        collectionView.scrollToItem(at: indexPath, at: .centeredHorizontally, animated: true)
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: true)
    }

    func collectionView(
        _ collectionView: UICollectionView,
        didEndDisplaying cell: UICollectionViewCell,
        forItemAt indexPath: IndexPath
    ) {
        (cell as? StudioFilmstripCell)?.cancelPreviewRequest()
    }

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        userDriving = true
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        guard let id = centeredArtifactId() else { return }
        refreshVisibleSelection(centeredId: id)
        guard userDriving, id != lastReportedCenteredId else { return }
        lastReportedCenteredId = id
        selectedId = id
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: false)
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { settleSelection() }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) { settleSelection() }

    func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        refreshVisibleSelection(centeredId: selectedId)
    }

    private func positionPendingSelection() {
        guard let id = pendingSelectionId, let index = artifactIndex[id] else { return }
        pendingSelectionId = nil
        collectionView.scrollToItem(
            at: IndexPath(item: index, section: 0),
            at: .centeredHorizontally,
            animated: false
        )
        refreshVisibleSelection(centeredId: id)
    }

    private func settleSelection() {
        guard let id = centeredArtifactId() else { return }
        selectedId = id
        lastReportedCenteredId = id
        refreshVisibleSelection(centeredId: id)
        userDriving = false
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: true)
    }

    private func centeredArtifactId() -> String? {
        let center = CGPoint(
            // UIScrollView moves its bounds origin with contentOffset, so
            // bounds.midX is already expressed in content coordinates.
            x: collectionView.bounds.midX,
            y: collectionView.bounds.midY
        )
        let indexPath = collectionView.indexPathForItem(at: center)
            ?? collectionView.indexPathsForVisibleItems.min { lhs, rhs in
                let left = collectionView.layoutAttributesForItem(at: lhs)?.center.x ?? 0
                let right = collectionView.layoutAttributesForItem(at: rhs)?.center.x ?? 0
                return abs(left - center.x) < abs(right - center.x)
            }
        guard let index = indexPath?.item, artifacts.indices.contains(index) else { return nil }
        return artifacts[index].id
    }

    private func refreshVisibleSelection(centeredId: String?) {
        for case let cell as StudioFilmstripCell in collectionView.visibleCells {
            guard let indexPath = collectionView.indexPath(for: cell),
                  artifacts.indices.contains(indexPath.item) else { continue }
            cell.setSelected(artifacts[indexPath.item].id == centeredId)
        }
    }

    private static func index(_ artifacts: [StudioArtifactDetail]) -> [String: Int] {
        Dictionary(artifacts.indices.map { (artifacts[$0].id, $0) }, uniquingKeysWith: { first, _ in first })
    }

    private func configure(_ cell: StudioFilmstripCell, at index: Int) {
        let artifact = artifacts[index]
        cell.configure(
            artifact: artifact,
            selected: artifact.id == selectedId,
            browser: browser,
            workspace: workspace,
            deviceId: deviceId
        )
    }


#if DEBUG
    func selectItemForTesting(at index: Int) {
        guard artifacts.indices.contains(index) else { return }
        let id = artifacts[index].id
        userDriving = true
        lastReportedCenteredId = id
        selectedId = id
        refreshVisibleSelection(centeredId: id)
        delegate?.viewerFilmstrip(self, centeredOn: id, settled: false)
    }

    func centerItemUsingCollectionGeometryForTesting(at index: Int) {
        guard artifacts.indices.contains(index), collectionView.bounds.width > 0 else { return }
        collectionView.layoutIfNeeded()
        guard let attributes = collectionView.layoutAttributesForItem(
            at: IndexPath(item: index, section: 0)
        ) else { return }
        userDriving = true
        collectionView.contentOffset.x = attributes.center.x - collectionView.bounds.width / 2
        scrollViewDidScroll(collectionView)
    }

    var residentCellCountForTesting: Int { collectionView.visibleCells.count }

    func animateToItemForTesting(at index: Int, completion: @escaping () -> Void) {
        guard artifacts.indices.contains(index), collectionView.bounds.width > 0 else {
            completion()
            return
        }
        collectionView.layoutIfNeeded()
        let target = IndexPath(item: index, section: 0)
        guard let attributes = collectionView.layoutAttributesForItem(at: target) else {
            completion()
            return
        }
        userDriving = true
        let offset = attributes.center.x - collectionView.bounds.width / 2
        UIView.animate(
            withDuration: 1.05,
            delay: 0,
            options: [.curveEaseOut, .allowUserInteraction]
        ) {
            self.collectionView.contentOffset.x = offset
        } completion: { _ in
            self.settleSelection()
            completion()
        }
    }
#endif
}

@MainActor
private final class StudioFilmstripCell: UICollectionViewCell {
    static let reuseIdentifier = "StudioFilmstripCell"

    private let imageView = UIImageView()
    private let placeholder = UIImageView()
    private var representedId: String?
    private var loadTask: Task<Void, Never>?
    private var browser: StudioBrowserStore?
    private var requestActive = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        contentView.backgroundColor = Palette.chip
        contentView.layer.cornerRadius = 6
        contentView.layer.cornerCurve = .continuous
        contentView.clipsToBounds = true
        contentView.layer.borderColor = UIColor.white.cgColor

        imageView.frame = contentView.bounds
        imageView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        imageView.contentMode = .scaleAspectFill
        imageView.clipsToBounds = true
        contentView.addSubview(imageView)

        placeholder.frame = contentView.bounds
        placeholder.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        placeholder.contentMode = .center
        placeholder.tintColor = Palette.color(.textFaint).withAlphaComponent(0.5)
        contentView.addSubview(placeholder)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func prepareForReuse() {
        super.prepareForReuse()
        cancelPreviewRequest()
        representedId = nil
        imageView.image = nil
        placeholder.image = nil
        placeholder.isHidden = false
        setSelected(false)
    }

    func configure(
        artifact: StudioArtifactDetail,
        selected: Bool,
        browser: StudioBrowserStore,
        workspace: StudioService?,
        deviceId: String?
    ) {
        if representedId != artifact.id { cancelPreviewRequest() }
        representedId = artifact.id
        self.browser = browser
        setSelected(selected)
        placeholder.image = UIImage(systemName: artifact.mediaKind == .video ? "film" : "photo")
        if let cached = browser.cachedPreview(artifactId: artifact.id) {
            imageView.image = cached
            placeholder.isHidden = true
            return
        }

        let thumbhash = browser.thumbhashImage(artifact.thumbhash, aspectRatio: artifact.aspectRatio)
        imageView.image = thumbhash
        placeholder.isHidden = thumbhash != nil
        guard loadTask == nil, let workspace, let deviceId else { return }
        let artifactId = artifact.id
        loadTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(90))
            guard !Task.isCancelled, let self, representedId == artifactId else { return }
            requestActive = true
            let request = browser.beginPreviewRequest(
                artifactId: artifactId,
                deviceId: deviceId,
                workspace: workspace
            )
            let image = await request.value
            guard !Task.isCancelled, representedId == artifactId else { return }
            if let image { imageView.image = image }
            placeholder.isHidden = image != nil || thumbhash != nil
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

    func setSelected(_ selected: Bool) {
        contentView.layer.borderWidth = selected ? 2 : 0
        // Unselected thumbnails recede so the current one reads at a glance.
        contentView.alpha = selected ? 1 : 0.72
    }
}

/// While a strip is being swiped, a list swap (an older page arriving)
/// keeps the item on screen where it is, shifting the offset by what was
/// inserted, instead of re-centering the selection and killing the fling.
@MainActor
enum StudioStripAnchor {
    static func capture(_ collectionView: UICollectionView, ids: [StudioArtifactDetail]) -> (id: String, x: CGFloat)? {
        guard collectionView.isDragging || collectionView.isDecelerating,
              let first = collectionView.indexPathsForVisibleItems.min(),
              ids.indices.contains(first.item),
              let frame = collectionView.layoutAttributesForItem(at: first)?.frame else { return nil }
        return (ids[first.item].id, frame.minX - collectionView.contentOffset.x)
    }

    static func restore(_ anchor: (id: String, x: CGFloat), in collectionView: UICollectionView, index: Int?) {
        guard let index,
              let frame = collectionView.layoutAttributesForItem(at: IndexPath(item: index, section: 0))?.frame else { return }
        collectionView.contentOffset.x = frame.minX - anchor.x
    }
}
