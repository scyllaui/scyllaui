import XCTest

final class InstallLocationTests: XCTestCase {
    /// 系统挪位与 DMG 卷里要提示；「应用程序」、用户目录与构建目录不提示。
    func testNeedsMove() {
        XCTAssertTrue(InstallLocation.needsMove(bundlePath: "/private/var/folders/8h/x/T/AppTranslocation/0A1B/d/ScyllaUI.app"))
        XCTAssertTrue(InstallLocation.needsMove(bundlePath: "/Volumes/ScyllaUI 1.0.0/ScyllaUI.app"))
        XCTAssertFalse(InstallLocation.needsMove(bundlePath: "/Applications/ScyllaUI.app"))
        XCTAssertFalse(InstallLocation.needsMove(bundlePath: "/Users/a/Applications/ScyllaUI.app"))
        XCTAssertFalse(InstallLocation.needsMove(bundlePath: "/Users/a/dev/output/build/Build/Products/Release/ScyllaUI.app"))
    }
}
