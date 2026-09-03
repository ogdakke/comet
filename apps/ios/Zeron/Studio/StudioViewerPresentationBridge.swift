import SwiftUI
import UIKit

struct StudioViewerPresentationBridge: UIViewControllerRepresentable {
    @Binding var session: StudioViewerSession?
    let browser: StudioBrowserStore
    let model: AppModel
    let transitionSource: StudioGalleryTransitionSource
    let showThread: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    func makeUIViewController(context: Context) -> StudioPresentationAnchorViewController {
        let controller = StudioPresentationAnchorViewController()
        controller.view.isUserInteractionEnabled = false
        controller.view.backgroundColor = .clear
        context.coordinator.anchor = controller
        return controller
    }

    func updateUIViewController(
        _ viewController: StudioPresentationAnchorViewController,
        context: Context
    ) {
        context.coordinator.parent = self
        context.coordinator.updatePresentation()
    }

    @MainActor
    final class Coordinator: NSObject, UIAdaptivePresentationControllerDelegate {
        var parent: StudioViewerPresentationBridge
        weak var anchor: UIViewController?
        private weak var presentedController: StudioGalleryViewerController?
        private var presentedSessionId: UUID?

        init(parent: StudioViewerPresentationBridge) {
            self.parent = parent
        }

        func updatePresentation() {
            guard let session = parent.session else {
                if presentedController != nil { dismissViewer() }
                return
            }
            guard presentedSessionId != session.id else { return }
            guard let anchor,
                  anchor.viewIfLoaded?.window != nil,
                  let presenter = StudioViewerPresentationHost.resolve(from: anchor) else {
                Task { @MainActor [weak self] in
                    await Task.yield()
                    self?.updatePresentation()
                }
                return
            }

            let controller = StudioGalleryViewerController(
                session: session,
                browser: parent.browser,
                workspace: parent.model.workspace,
                deviceId: parent.browser.selectedDeviceId,
                requestDismissal: { [weak self] in self?.dismissViewer() },
                requestThread: { [weak self] threadId in
                    self?.dismissViewer { self?.parent.showThread(threadId) }
                },
                selectedArtifactChanged: { [weak transitionSource = parent.transitionSource] artifactId in
                    transitionSource?.prepareForDismissal(to: artifactId)
                }
            )
            StudioViewerNativeTransition.configure(
                controller,
                session: session,
                source: parent.transitionSource
            )
            controller.modalPresentationStyle = .fullScreen

            presentedController = controller
            presentedSessionId = session.id
            presenter.present(controller, animated: true) { [weak self, weak controller] in
                controller?.presentationController?.delegate = self
            }
        }

        func dismissViewer(completion: (() -> Void)? = nil) {
            guard let controller = presentedController else {
                parent.session = nil
                completion?()
                return
            }
            controller.session.presentationActive = false
            controller.dismiss(animated: true) { [weak self] in
                self?.presentedController = nil
                self?.presentedSessionId = nil
                self?.parent.session = nil
                completion?()
            }
        }

        func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
            presentedController = nil
            presentedSessionId = nil
            parent.session = nil
        }

    }
}

final class StudioPresentationAnchorViewController: UIViewController {
    override func loadView() { view = UIView(frame: .zero) }
}

@MainActor
enum StudioViewerPresentationHost {
    static func resolve(from anchor: UIViewController) -> UIViewController? {
        guard let root = anchor.view.window?.rootViewController else { return nil }
        return visibleController(from: root)
    }

    static func visibleController(from controller: UIViewController) -> UIViewController {
        if let presented = controller.presentedViewController,
           !presented.isBeingDismissed {
            return visibleController(from: presented)
        }
        if let navigation = controller as? UINavigationController,
           let visible = navigation.visibleViewController {
            return visibleController(from: visible)
        }
        if let tab = controller as? UITabBarController,
           let selected = tab.selectedViewController {
            return visibleController(from: selected)
        }
        return controller
    }
}

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
