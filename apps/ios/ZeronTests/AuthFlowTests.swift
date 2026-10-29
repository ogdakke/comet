import XCTest
@testable import Zeron

final class AuthFlowTests: XCTestCase {
    func testAuthorizeURLUsesOnlyTheSelfHostedEdge() throws {
        let edge = try XCTUnwrap(URL(string: "https://edge.example"))
        let url = try XCTUnwrap(Endpoints.authorizeURL(edgeURL: edge, state: "state-123"))

        XCTAssertEqual(url.scheme, "https")
        XCTAssertEqual(url.host, "edge.example")
        XCTAssertEqual(url.path, "/auth/ios/authorize")
        XCTAssertEqual(
            URLComponents(url: url, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "state" })?.value,
            "state-123"
        )
        XCTAssertFalse(url.absoluteString.contains("client_id"))
        XCTAssertFalse(url.absoluteString.contains("workos"))
    }

    func testKeychainServicesAreIsolatedByDeployment() {
        XCTAssertEqual(Endpoints.keychainService(deployment: "staging"), "sh.zeron.ios.staging")
        XCTAssertEqual(Endpoints.keychainService(deployment: "production"), "sh.zeron.ios.production")
        XCTAssertNotEqual(
            Endpoints.keychainService(deployment: "staging"),
            Endpoints.keychainService(deployment: "production")
        )
    }
}
