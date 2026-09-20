import XCTest
@testable import Zeron

@MainActor
final class SessionStoreDurabilityTests: XCTestCase {
    func testCommitNowSavesImmediatelyAndNotifies() {
        var saves = 0
        var callbacks = 0
        let saver = DocSaver {
            saves += 1
            return true
        }
        saver.onSaved = { callbacks += 1 }
        saver.poke()

        XCTAssertTrue(saver.commitNow())
        XCTAssertEqual(saves, 1)
        XCTAssertEqual(callbacks, 1)
        XCTAssertFalse(saver.isDirty)
    }

    func testFailedCommitStaysDirtyAndLaterFlushNotifies() {
        var shouldSucceed = false
        var callbacks = 0
        let saver = DocSaver { shouldSucceed }
        saver.onSaved = { callbacks += 1 }
        saver.poke()

        XCTAssertFalse(saver.commitNow())
        XCTAssertTrue(saver.isDirty)
        XCTAssertEqual(callbacks, 0)

        shouldSucceed = true
        saver.flush()
        XCTAssertFalse(saver.isDirty)
        XCTAssertEqual(callbacks, 1)
    }
}
