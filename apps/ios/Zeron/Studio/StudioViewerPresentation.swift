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

/// The viewer's native zoom: out of the selected artifact's thumbnail, and
/// back into whichever one is selected when it closes.
@MainActor
enum StudioViewerNativeTransition {
    static func configure(
        _ controller: StudioGalleryViewerController,
        session: StudioViewerSession,
        source: StudioGalleryTransitionSource
    ) {
        let options = UIViewController.Transition.ZoomOptions()
        options.dimmingColor = UIColor.black.withAlphaComponent(0.97)
        options.interactiveDismissShouldBegin = { [weak controller] context in
            context.willBegin && (controller?.canBeginInteractiveZoomDismissal ?? false)
        }
        options.alignmentRectProvider = { [weak controller] _ in
            guard let controller, controller.isViewLoaded else { return nil }
            return controller.transitionImageFrame(in: controller.view)
        }
        controller.preferredTransition = .zoom(options: options) { [weak source, weak session] _ in
            guard let session else { return nil }
            return source?.imageView(for: session.selectedId)
        }
    }
}
