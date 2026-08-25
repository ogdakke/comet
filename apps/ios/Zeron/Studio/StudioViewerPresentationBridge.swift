import SwiftUI
import UIKit

struct StudioViewerPresentationBridge: UIViewControllerRepresentable {
    @Binding var session: StudioViewerSession?
    let browser: StudioBrowserStore
    let model: AppModel
    let transitionSource: StudioGalleryTransitionSource
    let showThread: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeUIViewController(context: Context) -> StudioPresentationAnchorViewController {
        let viewController = StudioPresentationAnchorViewController()
        viewController.view.isUserInteractionEnabled = false
        viewController.view.backgroundColor = .clear
        context.coordinator.anchor = viewController
        return viewController
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
        private weak var presentedController: UIViewController?
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
            guard let anchor, anchor.viewIfLoaded?.window != nil else {
                Task { @MainActor [weak self] in
                    await Task.yield()
                    self?.updatePresentation()
                }
                return
            }

            let hosting = StudioViewerHostingController(
                session: session,
                rootView: StudioArtifactViewer(
                    session: session,
                    browser: parent.browser,
                    onDismiss: { [weak self] in self?.dismissViewer() },
                    showThread: { [weak self] threadId in
                        self?.dismissViewer { self?.parent.showThread(threadId) }
                    }
                )
                .environment(parent.model)
            )
            hosting.modalPresentationStyle = .fullScreen
            hosting.view.backgroundColor = .clear
            let options = UIViewController.Transition.ZoomOptions()
            options.dimmingColor = .black
            if session.openedFromGallery {
                hosting.preferredTransition = .zoom(options: options) { [weak session, weak transitionSource = parent.transitionSource] _ in
                    guard let session else { return nil }
                    return transitionSource?.imageView(for: session.selectedId)
                }
            }
            hosting.presentationController?.delegate = self
            presentedController = hosting
            presentedSessionId = session.id
            anchor.present(hosting, animated: true)
        }

        func dismissViewer(completion: (() -> Void)? = nil) {
            guard let controller = presentedController else {
                parent.session = nil
                completion?()
                return
            }
            (controller as? StudioViewerHostingSession)?.viewerSession.presentationActive = false
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
    override func loadView() {
        view = UIView(frame: .zero)
    }
}

@MainActor
private protocol StudioViewerHostingSession: AnyObject {
    var viewerSession: StudioViewerSession { get }
}

@MainActor
final class StudioViewerHostingController<Content: View>: UIHostingController<Content>, StudioViewerHostingSession {
    let viewerSession: StudioViewerSession

    init(
        session: StudioViewerSession,
        rootView: Content
    ) {
        viewerSession = session
        super.init(rootView: rootView)
    }

    @available(*, unavailable)
    dynamic required init?(coder aDecoder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        viewerSession.presentationActive = true
    }

    override func viewWillDisappear(_ animated: Bool) {
        viewerSession.presentationActive = false
        super.viewWillDisappear(animated)
    }
}
