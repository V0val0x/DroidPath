import XCTest
@testable import DroidPathBridge
import AndroidShadowGraphKit

final class DroidPathBridgeTests: XCTestCase {
    func testSerialValidationRejectsInjectionAndFlags() {
        XCTAssertTrue(DroidPathADBRunner.validateSerial("61061JEA307110"))
        XCTAssertTrue(DroidPathADBRunner.validateSerial("192.168.1.42:5555"))
        XCTAssertTrue(DroidPathADBRunner.validateSerial("adb-61061JEA307110-abc._tls-connect"))

        XCTAssertFalse(DroidPathADBRunner.validateSerial(""))
        XCTAssertFalse(DroidPathADBRunner.validateSerial("-s"))
        XCTAssertFalse(DroidPathADBRunner.validateSerial("6106; rm -rf /"))
        XCTAssertFalse(DroidPathADBRunner.validateSerial("serial with spaces"))
    }

    func testParseDevicesOutputExtractsUSBAndWireless() {
        let sample = """
        * daemon started successfully
        List of devices attached
        61061JEA307110         device usb:1-1 product:tegu model:Pixel_10a device:tegu transport_id:4
        192.168.1.80:5555      device product:panther model:Pixel_7 device:panther transport_id:5
        offline_serial         offline transport_id:6
        """
        let parsed = DroidPathADBRunner.parseDevicesOutput(sample)
        XCTAssertEqual(parsed.count, 2)
        XCTAssertEqual(parsed[0].serial, "61061JEA307110")
        XCTAssertEqual(parsed[0].model, "Pixel 10a")
        XCTAssertFalse(parsed[0].isWireless)

        XCTAssertEqual(parsed[1].serial, "192.168.1.80:5555")
        XCTAssertTrue(parsed[1].isWireless)
    }

    func testMCPServerInitializeToolsListAndGraphStats() async throws {
        let tempDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("droidpath-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tempDir) }

        let graphURL = tempDir.appendingPathComponent("shadow_graph.json")
        let manager = DroidPathManager(port: 0, storageURL: graphURL)
        let server = DroidPathMCPServerCore(manager: manager)

        let initReq = #"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.data(using: .utf8)!
        let initResp = await server.handleRawRequestData(initReq)
        XCTAssertNotNil(initResp)
        let initJSON = String(decoding: initResp!, as: UTF8.self)
        XCTAssertTrue(initJSON.contains("droidpath-mcp"))
        XCTAssertTrue(initJSON.contains("2024-11-05"))

        let listReq = #"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.data(using: .utf8)!
        let listResp = await server.handleRawRequestData(listReq)
        let listJSON = String(decoding: listResp!, as: UTF8.self)
        XCTAssertTrue(listJSON.contains("android_navigate_to_node"))
        XCTAssertTrue(listJSON.contains("android_graph_stats"))
        XCTAssertTrue(listJSON.contains("android_get_ui_tree"))

        let statsReq = #"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"android_graph_stats","arguments":{}}}"#.data(using: .utf8)!
        let statsResp = await server.handleRawRequestData(statsReq)
        let statsJSON = String(decoding: statsResp!, as: UTF8.self)
        XCTAssertTrue(statsJSON.contains("vertex_count"))
        XCTAssertTrue(statsJSON.contains("storage_path"))
    }

    func testGraphPersistenceRoundTripAcrossManagerRestart() async throws {
        let tempDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("droidpath-persist-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tempDir) }

        let graphURL = tempDir.appendingPathComponent("shadow_graph.json")
        let manager1 = DroidPathManager(port: 0, storageURL: graphURL)

        let screenA = ShadowTreeIndex(
            seq: 1,
            timestampMs: 1_000,
            activePackage: "com.android.launcher",
            activeWindowId: 1,
            nodes: [
                ShadowUINode(
                    index: 0,
                    text: "Settings",
                    resourceId: "icon_settings",
                    contentDesc: "",
                    className: "TextView",
                    clickable: true,
                    scrollable: false,
                    bounds: "[100,200][200,300]",
                    centerX: 150,
                    centerY: 250,
                    nodeId: 1,
                    parentId: nil,
                    windowId: 1,
                    childIndex: 0,
                    depth: 1,
                    packageName: "com.android.launcher",
                    editable: false,
                    focused: false,
                    checked: false,
                    enabled: true,
                    visible: true
                )
            ]
        )
        let screenB = ShadowTreeIndex(
            seq: 2,
            timestampMs: 2_000,
            activePackage: "com.android.settings",
            activeWindowId: 2,
            nodes: [
                ShadowUINode(
                    index: 0,
                    text: "Night Light",
                    resourceId: "night_light",
                    contentDesc: "",
                    className: "TextView",
                    clickable: true,
                    scrollable: false,
                    bounds: "[50,400][600,500]",
                    centerX: 325,
                    centerY: 450,
                    nodeId: 2,
                    parentId: nil,
                    windowId: 2,
                    childIndex: 0,
                    depth: 1,
                    packageName: "com.android.settings",
                    editable: false,
                    focused: false,
                    checked: false,
                    enabled: true,
                    visible: true
                )
            ]
        )

        _ = await manager1.bridge.graphEngine.process(update: ShadowDOMUpdate(
            index: screenA, isFullSnapshot: true, insertedCount: 1, updatedCount: 0, removedCount: 0
        ))
        await manager1.recordAction(AttributedUIAction(
            kind: .tap,
            targetDescription: "tap Settings",
            x: 150,
            y: 250,
            selectorResourceId: "icon_settings",
            selectorText: "Settings"
        ))
        _ = await manager1.bridge.graphEngine.process(update: ShadowDOMUpdate(
            index: screenB, isFullSnapshot: true, insertedCount: 1, updatedCount: 0, removedCount: 0
        ))
        await manager1.flushPersistedGraph()

        XCTAssertTrue(FileManager.default.fileExists(atPath: graphURL.path))

        let manager2 = DroidPathManager(port: 0, storageURL: graphURL)
        let stats = await manager2.graphStats()
        XCTAssertGreaterThanOrEqual(stats.vertexCount, 2)
        XCTAssertGreaterThanOrEqual(stats.edgeCount, 1)
    }

    func testLoopbackHTTPChecks() {
        XCTAssertTrue(DroidPathHTTPServer.isAllowedLoopbackHost("127.0.0.1:8766"))
        XCTAssertTrue(DroidPathHTTPServer.isAllowedLoopbackHost("localhost"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedLoopbackHost("evil.com"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedLoopbackHost(nil))

        XCTAssertTrue(DroidPathHTTPServer.isAllowedOrigin(nil))
        XCTAssertTrue(DroidPathHTTPServer.isAllowedOrigin("http://127.0.0.1:3000"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedOrigin("https://evil.example.com"))
    }
}
