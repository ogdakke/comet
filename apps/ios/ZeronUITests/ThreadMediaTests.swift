import XCTest

/// A thread's pictures and videos open as one gallery (the demo's veil
/// thread: a sent screenshot, then two the agent shows).
final class ThreadMediaTests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    private func snapshot(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }

    func testLightboxPagesThroughTheThreadsMedia() {
        let app = XCUIApplication()
        app.launchArguments = ["-demo", "-fast", "-route", "chat:chat-veil"]
        app.launch()
        let transcript = app.otherElements["transcript"].firstMatch
        XCTAssertTrue(app.textViews["composer-input"].waitForExistence(timeout: 10))

        // The agent's first picture: scroll up until it's on screen.
        let picture = app.descendants(matching: .any)["/Users/dev/.zeron/uploads/square.png"]
        for _ in 0..<12 where !(picture.exists && picture.isHittable) {
            (transcript.exists ? transcript : app.windows.firstMatch).swipeDown()
        }
        XCTAssertTrue(picture.isHittable, "the agent's picture is in the thread")
        snapshot(app, "thread-pictures")

        picture.tap()
        let pager = app.collectionViews["thread-media-pager"]
        XCTAssertTrue(pager.waitForExistence(timeout: 5))
        // The sent screenshot comes first, so this is the second of three.
        XCTAssertTrue(app.staticTexts["2 of 3"].waitForExistence(timeout: 3))
        snapshot(app, "lightbox-open")

        pager.swipeLeft()
        XCTAssertTrue(app.staticTexts["3 of 3"].waitForExistence(timeout: 3))
        snapshot(app, "lightbox-paged")

        // A tap hides the chrome; another brings it back.
        pager.tap()
        XCTAssertTrue(app.staticTexts["3 of 3"].waitForNonExistence(timeout: 2))
        pager.tap()
        XCTAssertTrue(app.staticTexts["3 of 3"].waitForExistence(timeout: 2))

        // Dragged down, it closes back into the thread.
        pager.swipeDown(velocity: .fast)
        XCTAssertTrue(pager.waitForNonExistence(timeout: 3))
        snapshot(app, "lightbox-closed")

        // The sent screenshot previews as a received picture does (its chip
        // stays in the prompt), with the sent ZIP as a file card.
        let sent = app.descendants(matching: .any)["/Users/dev/.zeron/uploads/4f1c9a2e-Image_1.png"]
        for _ in 0..<12 where !(sent.exists && sent.isHittable) {
            (transcript.exists ? transcript : app.windows.firstMatch).swipeDown()
        }
        XCTAssertTrue(sent.isHittable, "the sent picture shows in the prompt")
        XCTAssertGreaterThan(sent.frame.width, 200, "full size, not a thumbnail")
        snapshot(app, "sent-attachments")
        sent.tap()
        XCTAssertTrue(app.staticTexts["1 of 3"].waitForExistence(timeout: 5))
    }
}
