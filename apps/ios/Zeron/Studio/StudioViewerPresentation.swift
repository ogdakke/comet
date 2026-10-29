import AVFoundation
import UIKit

/// Where the viewer presents from: whatever is on screen above the window's
/// root (a pushed thread, a sheet).
@MainActor
enum StudioViewerPresentationHost {
    static func resolve(from anchor: UIViewController) -> UIViewController? {
        guard let root = anchor.viewIfLoaded?.window?.rootViewController else { return nil }
        return visibleController(from: root)
    }

    static func visibleController(from controller: UIViewController) -> UIViewController {
        if let presented = controller.presentedViewController, !presented.isBeingDismissed {
            return visibleController(from: presented)
        }
        if let navigation = controller as? UINavigationController, let visible = navigation.visibleViewController {
            return visibleController(from: visible)
        }
        if let tab = controller as? UITabBarController, let selected = tab.selectedViewController {
            return visibleController(from: selected)
        }
        return controller
    }
}

/// The viewer's zoom out of the selected artifact's thumbnail, and back into
/// whichever one is selected when it closes — Photos' transition, built by
/// hand because neither a full-screen modal (which removes the grid) nor the
/// system `.zoom` (which scales the viewer's black backdrop along with the
/// photo) can show the grid behind a dragged photo.
///
/// - The presentation is `.custom` and keeps the presenter's view in the
///   window, so the grid is always there under the viewer's backdrop.
/// - Every phase animates one dedicated `StudioViewerTransitionView` in the
///   container — an aspect-fill image (and the live player layer for video)
///   inside a clipping frame — never the pager's zoomable scroll view. The
///   frame goes between the page's aspect-fit rect and the thumbnail's
///   square, so the crop morphs into the cell's.
/// - The thumbnail is hidden while the transition view stands in for it.
@MainActor
enum StudioViewerNativeTransition {
    static func configure(
        _ controller: StudioGalleryViewerController,
        session: StudioViewerSession,
        source: StudioGalleryTransitionSource
    ) {
        let transition = StudioViewerTransitionController(session: session, source: source)
        transition.viewer = controller
        // `transitioningDelegate` is weak; the viewer keeps it alive.
        controller.transitionController = transition
        controller.transitioningDelegate = transition
        controller.preferredTransition = nil
    }
}

@MainActor
final class StudioViewerTransitionController: NSObject, UIViewControllerTransitioningDelegate {
    weak var viewer: StudioGalleryViewerController?
    /// The viewer's dismiss pan hands its dismissal in here just before it
    /// asks to be dismissed, and it's cleared once that transition ends.
    var interactiveDismissal: StudioViewerInteractiveDismissal?

    private weak var session: StudioViewerSession?
    private weak var source: StudioGalleryTransitionSource?

    init(session: StudioViewerSession, source: StudioGalleryTransitionSource) {
        self.session = session
        self.source = source
    }

    func presentationController(
        forPresented presented: UIViewController,
        presenting: UIViewController?,
        source: UIViewController
    ) -> UIPresentationController? {
        StudioViewerPresentationController(presentedViewController: presented, presenting: presenting)
    }

    func animationController(
        forPresented presented: UIViewController,
        presenting: UIViewController,
        source: UIViewController
    ) -> (any UIViewControllerAnimatedTransitioning)? {
        StudioViewerZoomAnimator(presenting: true, transition: self)
    }

    func animationController(forDismissed dismissed: UIViewController) -> (any UIViewControllerAnimatedTransitioning)? {
        StudioViewerZoomAnimator(presenting: false, transition: self)
    }

    func interactionControllerForDismissal(
        using animator: any UIViewControllerAnimatedTransitioning
    ) -> (any UIViewControllerInteractiveTransitioning)? {
        interactiveDismissal
    }

    /// The selected artifact's thumbnail, scrolled into view if the grid
    /// can, with its frame in `container`'s coordinates.
    func sourceView(in container: UIView, scrollIntoView: Bool) -> (view: UIView, frame: CGRect, radius: CGFloat)? {
        guard let session, let source else { return nil }
        if scrollIntoView {
            source.prepareForDismissal(to: session.selectedId)
        }
        guard let view = source.imageView(for: session.selectedId) else { return nil }
        view.superview?.layoutIfNeeded()
        let frame = view.convert(view.bounds, to: container)
        // A thumbnail scrolled under the chrome or off screen isn't a place to land.
        guard frame.intersects(container.bounds), frame.width > 1, frame.height > 1 else { return nil }
        var radius = view.layer.cornerRadius
        var ancestor = view.superview
        for _ in 0..<3 where radius == 0 {
            radius = ancestor?.layer.cornerRadius ?? 0
            ancestor = ancestor?.superview
        }
        return (view, frame, radius)
    }
}

/// Keeps the presenter on screen under the viewer.
final class StudioViewerPresentationController: UIPresentationController {
    override var shouldRemovePresentersView: Bool { false }

    override var frameOfPresentedViewInContainerView: CGRect { containerView?.bounds ?? .zero }

    override func containerViewWillLayoutSubviews() {
        super.containerViewWillLayoutSubviews()
        presentedView?.frame = frameOfPresentedViewInContainerView
    }
}

/// The image in flight: a clipping frame whose bounds and corner radius
/// animate, around an aspect-fill image (and player layer) that's laid out
/// to fill it — so an aspect-fit photo becomes the grid's square crop.
final class StudioViewerTransitionView: UIView {
    private let imageView = UIImageView()
    private let playerView = StudioViewerPlayerLayerView()
    private let aspectRatio: CGFloat

    init(image: UIImage?, player: AVPlayer?, aspectRatio: CGFloat) {
        self.aspectRatio = max(0.01, aspectRatio)
        super.init(frame: .zero)
        clipsToBounds = true
        layer.cornerCurve = .continuous
        imageView.image = image
        imageView.contentMode = .scaleAspectFill
        addSubview(imageView)
        if let player {
            playerView.playerLayer.player = player
            playerView.playerLayer.videoGravity = .resizeAspectFill
            addSubview(playerView)
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layoutSubviews() {
        super.layoutSubviews()
        let fill: CGRect
        let boundsAspect = bounds.width / max(bounds.height, 0.01)
        if boundsAspect > aspectRatio {
            let height = bounds.width / aspectRatio
            fill = CGRect(x: 0, y: (bounds.height - height) / 2, width: bounds.width, height: height)
        } else {
            let width = bounds.height * aspectRatio
            fill = CGRect(x: (bounds.width - width) / 2, y: 0, width: width, height: bounds.height)
        }
        imageView.frame = fill
        playerView.frame = fill
    }

    /// Move to `frame` with `radius`; inside an animation block, the crop
    /// animates with it.
    func place(_ frame: CGRect, radius: CGFloat) {
        self.frame = frame
        layer.cornerRadius = radius
        layoutIfNeeded()
    }
}

final class StudioViewerPlayerLayerView: UIView {
    override class var layerClass: AnyClass { AVPlayerLayer.self }
    var playerLayer: AVPlayerLayer { layer as! AVPlayerLayer }
}

/// Open and programmatic close (the close button, a deletion, Show in
/// Thread). The dragged close is `StudioViewerInteractiveDismissal`.
final class StudioViewerZoomAnimator: NSObject, UIViewControllerAnimatedTransitioning {
    private let presenting: Bool
    private weak var transition: StudioViewerTransitionController?

    init(presenting: Bool, transition: StudioViewerTransitionController) {
        self.presenting = presenting
        self.transition = transition
    }

    func transitionDuration(using transitionContext: (any UIViewControllerContextTransitioning)?) -> TimeInterval {
        UIAccessibility.isReduceMotionEnabled ? 0.25 : 0.45
    }

    func animateTransition(using context: any UIViewControllerContextTransitioning) {
        MainActor.assumeIsolated {
            presenting ? present(context) : dismiss(context)
        }
    }

    @MainActor
    private func present(_ context: any UIViewControllerContextTransitioning) {
        let container = context.containerView
        guard let toView = context.view(forKey: .to),
              let viewer = context.viewController(forKey: .to) as? StudioGalleryViewerController else {
            if let toView = context.view(forKey: .to) { container.addSubview(toView) }
            context.completeTransition(true)
            return
        }
        toView.frame = context.finalFrame(for: viewer)
        container.addSubview(toView)
        toView.layoutIfNeeded()

        let duration = transitionDuration(using: context)
        let source = transition?.sourceView(in: container, scrollIntoView: false)
        guard let source, !UIAccessibility.isReduceMotionEnabled else {
            toView.alpha = 0
            UIView.animate(withDuration: duration, delay: 0, options: [.curveEaseOut]) {
                toView.alpha = 1
            } completion: { _ in
                context.completeTransition(!context.transitionWasCancelled)
            }
            return
        }

        let target = viewer.transitionImageFrame(in: container)
        let content = viewer.transitionContent
        let flying = StudioViewerTransitionView(
            image: content.image,
            player: content.player,
            aspectRatio: target.width / max(target.height, 1)
        )
        flying.place(source.frame, radius: source.radius)
        container.addSubview(flying)
        source.view.alpha = 0
        viewer.setTransitionState(backdrop: 0, chrome: 0, mediaHidden: true)

        let animator = UIViewPropertyAnimator(
            duration: duration,
            timingParameters: UISpringTimingParameters(dampingRatio: 0.86)
        )
        animator.addAnimations {
            flying.place(target, radius: 0)
            viewer.setTransitionState(backdrop: 1, chrome: 1, mediaHidden: true)
        }
        animator.addCompletion { _ in
            viewer.setTransitionState(backdrop: 1, chrome: 1, mediaHidden: false)
            flying.removeFromSuperview()
            source.view.alpha = 1
            context.completeTransition(!context.transitionWasCancelled)
        }
        animator.startAnimation()
    }

    @MainActor
    private func dismiss(_ context: any UIViewControllerContextTransitioning) {
        let container = context.containerView
        guard let fromView = context.view(forKey: .from),
              let viewer = context.viewController(forKey: .from) as? StudioGalleryViewerController else {
            context.view(forKey: .from)?.removeFromSuperview()
            context.completeTransition(true)
            return
        }
        let duration = transitionDuration(using: context)
        let start = viewer.transitionImageFrame(in: container)
        let source = UIAccessibility.isReduceMotionEnabled
            ? nil
            : transition?.sourceView(in: container, scrollIntoView: true)
        let content = viewer.transitionContent
        let flying = StudioViewerTransitionView(
            image: content.image,
            player: content.player,
            aspectRatio: start.width / max(start.height, 1)
        )
        flying.place(start, radius: 0)
        container.addSubview(flying)
        source?.view.alpha = 0
        viewer.setTransitionState(backdrop: 1, chrome: 0, mediaHidden: true)

        let animator = UIViewPropertyAnimator(
            duration: duration,
            timingParameters: UISpringTimingParameters(dampingRatio: source == nil ? 1 : 0.88)
        )
        animator.addAnimations {
            if let source {
                flying.place(source.frame, radius: source.radius)
            } else {
                // Nowhere to land (scrolled away, Reduce Motion): fade out.
                flying.alpha = 0
                if !UIAccessibility.isReduceMotionEnabled {
                    flying.transform = CGAffineTransform(scaleX: 0.92, y: 0.92)
                }
            }
            viewer.setTransitionState(backdrop: 0, chrome: 0, mediaHidden: true)
        }
        animator.addCompletion { _ in
            let finished = !context.transitionWasCancelled
            source?.view.alpha = 1
            flying.removeFromSuperview()
            if finished {
                fromView.removeFromSuperview()
            } else {
                viewer.setTransitionState(backdrop: 1, chrome: 1, mediaHidden: false)
            }
            context.completeTransition(finished)
        }
        animator.startAnimation()
    }
}

/// The dragged close, driven 1:1 by the viewer's dismiss pan: the photo
/// follows the finger and shrinks toward 0.75 with distance down, the
/// backdrop thins so the grid shows through, and on release it springs into
/// the thumbnail (with the finger's velocity) or back onto the page.
@MainActor
final class StudioViewerInteractiveDismissal: NSObject, UIViewControllerInteractiveTransitioning {
    private weak var transition: StudioViewerTransitionController?
    private weak var viewer: StudioGalleryViewerController?
    private let touchStart: CGPoint
    private var context: (any UIViewControllerContextTransitioning)?
    private var flying: StudioViewerTransitionView?
    private var startFrame: CGRect = .zero
    private var translation: CGPoint = .zero
    private var pendingEnd: CGPoint?
    private var armed = false
    private let haptics = UIImpactFeedbackGenerator(style: .light)

    /// How far down counts as all the way (the backdrop is gone, the
    /// photo at its smallest).
    private static let fullDistance: CGFloat = 420
    /// A release past this (and not flung back up) closes.
    private static let closeDistance: CGFloat = 110

    init(transition: StudioViewerTransitionController, viewer: StudioGalleryViewerController, touch: CGPoint) {
        self.transition = transition
        self.viewer = viewer
        touchStart = touch
        super.init()
        haptics.prepare()
    }

    nonisolated func startInteractiveTransition(_ transitionContext: any UIViewControllerContextTransitioning) {
        MainActor.assumeIsolated { start(transitionContext) }
    }

    private func start(_ context: any UIViewControllerContextTransitioning) {
        self.context = context
        let container = context.containerView
        guard let viewer else {
            context.cancelInteractiveTransition()
            context.completeTransition(false)
            return
        }
        startFrame = viewer.transitionImageFrame(in: container)
        let content = viewer.transitionContent
        let flying = StudioViewerTransitionView(
            image: content.image,
            player: content.player,
            aspectRatio: startFrame.width / max(startFrame.height, 1)
        )
        flying.place(startFrame, radius: 0)
        container.addSubview(flying)
        self.flying = flying
        viewer.setTransitionState(backdrop: 1, chrome: 0, mediaHidden: true)
        apply()
        if let pendingEnd { end(velocity: pendingEnd) }
    }

    func update(translation: CGPoint) {
        self.translation = translation
        apply()
    }

    /// The pan ended (or was cancelled, with zero velocity and no travel).
    func end(velocity: CGPoint) {
        guard let context, let flying, let viewer else {
            pendingEnd = velocity
            return
        }
        pendingEnd = nil
        let closes = (translation.y > Self.closeDistance && velocity.y > -250) || velocity.y > 900
        let container = context.containerView
        if closes {
            let source = transition?.sourceView(in: container, scrollIntoView: true)
            let target = source?.frame
            let animator = UIViewPropertyAnimator(
                duration: 0.5,
                timingParameters: UISpringTimingParameters(
                    dampingRatio: 0.88,
                    initialVelocity: relativeVelocity(velocity, from: flying.frame, to: target ?? flying.frame)
                )
            )
            source?.view.alpha = 0
            animator.addAnimations {
                if let source {
                    flying.place(source.frame, radius: source.radius)
                } else {
                    // The thumbnail is gone: carry on along the fling and fade.
                    var frame = flying.frame
                    frame.origin.x += velocity.x * 0.12
                    frame.origin.y += max(velocity.y, 300) * 0.12
                    flying.place(frame, radius: 0)
                    flying.alpha = 0
                }
                viewer.setTransitionState(backdrop: 0, chrome: 0, mediaHidden: true)
            }
            animator.addCompletion { [weak self] _ in
                source?.view.alpha = 1
                flying.removeFromSuperview()
                context.view(forKey: .from)?.removeFromSuperview()
                context.completeTransition(true)
                self?.transition?.interactiveDismissal = nil
            }
            context.finishInteractiveTransition()
            animator.startAnimation()
        } else {
            let animator = UIViewPropertyAnimator(
                duration: 0.45,
                timingParameters: UISpringTimingParameters(
                    dampingRatio: 0.82,
                    initialVelocity: relativeVelocity(velocity, from: flying.frame, to: startFrame)
                )
            )
            let startFrame = startFrame
            animator.addAnimations {
                flying.place(startFrame, radius: 0)
                viewer.setTransitionState(backdrop: 1, chrome: 1, mediaHidden: true)
            }
            animator.addCompletion { [weak self] _ in
                viewer.setTransitionState(backdrop: 1, chrome: 1, mediaHidden: false)
                flying.removeFromSuperview()
                context.cancelInteractiveTransition()
                context.completeTransition(false)
                viewer.interactiveDismissalDidCancel()
                self?.transition?.interactiveDismissal = nil
            }
            animator.startAnimation()
        }
    }

    private func apply() {
        guard let context, let flying, let viewer else { return }
        let down = max(0, translation.y)
        let progress = min(1, down / Self.fullDistance)
        let scale = 1 - 0.25 * progress
        // Scale about the finger, so the spot under it stays under it.
        let anchor = CGPoint(x: startFrame.midX - touchStart.x, y: startFrame.midY - touchStart.y)
        let center = CGPoint(
            x: touchStart.x + translation.x + anchor.x * scale,
            y: touchStart.y + translation.y + anchor.y * scale
        )
        let size = CGSize(width: startFrame.width * scale, height: startFrame.height * scale)
        flying.place(
            CGRect(x: center.x - size.width / 2, y: center.y - size.height / 2, width: size.width, height: size.height),
            radius: 0
        )
        viewer.setTransitionState(backdrop: 1 - progress, chrome: 0, mediaHidden: true)
        context.updateInteractiveTransition(progress)

        let nowArmed = translation.y > Self.closeDistance
        if nowArmed != armed {
            armed = nowArmed
            haptics.impactOccurred(intensity: nowArmed ? 0.7 : 0.4)
            haptics.prepare()
        }
    }

    /// UIKit's spring velocity is relative: points per second over the
    /// distance left to travel.
    private func relativeVelocity(_ velocity: CGPoint, from: CGRect, to: CGRect) -> CGVector {
        let dx = to.midX - from.midX
        let dy = to.midY - from.midY
        // Clamped: a target a few points away would turn any fling into a lurch.
        func relative(_ speed: CGFloat, _ distance: CGFloat) -> CGFloat {
            abs(distance) > 8 ? max(-12, min(12, speed / distance)) : 0
        }
        return CGVector(dx: relative(velocity.x, dx), dy: relative(velocity.y, dy))
    }
}
