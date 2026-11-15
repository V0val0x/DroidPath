import XCTest
import Network
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
        XCTAssertTrue(DroidPathHTTPServer.isAllowedLoopbackHost("[::1]:8766"))
        XCTAssertTrue(DroidPathHTTPServer.isAllowedLoopbackHost("[::1]"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedLoopbackHost("evil.com"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedLoopbackHost(nil))

        XCTAssertTrue(DroidPathHTTPServer.isAllowedOrigin(nil))
        XCTAssertTrue(DroidPathHTTPServer.isAllowedOrigin("http://127.0.0.1:3000"))
        XCTAssertTrue(DroidPathHTTPServer.isAllowedOrigin("http://[::1]:3000"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedOrigin("https://evil.example.com"))
        XCTAssertFalse(DroidPathHTTPServer.isAllowedOrigin("javascript:alert(1)"))
    }

    func testShellQuotingPackageValidationAndKeyCodeResolution() {
        let asciiInject = DroidPathMCPServerCore.buildSafeTypeTextCommand("hello; rm -rf /")
        XCTAssertFalse(asciiInject.useClipboard)
        XCTAssertEqual(asciiInject.quotedArg, "'hello;%srm%s-rf%s/'")

        let quoteInject = DroidPathMCPServerCore.buildSafeTypeTextCommand("it's $(id)\0")
        XCTAssertFalse(quoteInject.useClipboard)
        XCTAssertEqual(quoteInject.quotedArg, "'it'\\''s%s$(id)'")

        let unicode = DroidPathMCPServerCore.buildSafeTypeTextCommand("Привет мир")
        XCTAssertTrue(unicode.useClipboard)
        XCTAssertEqual(unicode.quotedArg, "'Привет мир'")

        XCTAssertTrue(DroidPathADBRunner.validatePackageName("com.android.settings"))
        XCTAssertFalse(DroidPathADBRunner.validatePackageName("com.foo;id"))
        XCTAssertFalse(DroidPathADBRunner.validatePackageName("-flag"))

        XCTAssertEqual(DroidPathActionExecutor.resolveKeyCode("HOME"), "3")
        XCTAssertEqual(DroidPathActionExecutor.resolveKeyCode("KEYCODE_APP_SWITCH"), "187")
        XCTAssertEqual(DroidPathActionExecutor.resolveKeyCode("power"), "26")
        XCTAssertEqual(DroidPathActionExecutor.resolveKeyCode("66"), "66")
        XCTAssertNil(DroidPathActionExecutor.resolveKeyCode("4;id"))
    }

    func testLiveHTTPServerSSENegativeContentLengthIPv6HostAndBearerAuth() async throws {
        let tempDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("droidpath-http-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tempDir) }

        let manager = DroidPathManager(port: 0, storageURL: tempDir.appendingPathComponent("graph.json"))
        let core = DroidPathMCPServerCore(manager: manager)
        let http = DroidPathHTTPServer(server: core)
        try http.start(port: 0, requireAuthToken: "secret-token")
        defer { http.stop() }
        let port = http.port
        XCTAssertGreaterThan(port, 0)

        // 1. Unauthorized request without Bearer token -> 401
        let unauth = try await rawHTTPExchange(
            port: port,
            request: "GET /health HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\n\r\n"
        )
        XCTAssertTrue(unauth.contains("401 Unauthorized"), "Got: \(unauth)")

        // 2. Negative Content-Length must return 400 Bad Request (and NOT crash!)
        let badCL = try await rawHTTPExchange(
            port: port,
            request: "POST /message HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\nAuthorization: Bearer secret-token\r\nContent-Length: -1\r\n\r\n"
        )
        XCTAssertTrue(badCL.contains("400 Bad Request"), "Got: \(badCL)")

        // 3. IPv6 loopback Host [::1]:port must be allowed and return 200 OK
        let pingBody = #"{"jsonrpc":"2.0","id":7,"method":"ping"}"#
        let ipv6Resp = try await rawHTTPExchange(
            port: port,
            request: "POST /message HTTP/1.1\r\nHost: [::1]:\(port)\r\nAuthorization: Bearer secret-token\r\nContent-Length: \(pingBody.utf8.count)\r\n\r\n\(pingBody)"
        )
        XCTAssertTrue(ipv6Resp.contains("200 OK"), "Got: \(ipv6Resp)")
        XCTAssertTrue(ipv6Resp.contains(#""id":7"#), "Got: \(ipv6Resp)")

        // 4. Full SSE handshake + POST /message?sessionId=... -> 202 Accepted + SSE event: message
        let sseConn = NWConnection(host: .ipv4(.loopback), port: NWEndpoint.Port(rawValue: port)!, using: .tcp)
        let q = DispatchQueue(label: "test.droidpath.sse")
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            final class Box: @unchecked Sendable { var done = false }
            let box = Box()
            sseConn.stateUpdateHandler = { state in
                guard !box.done else { return }
                switch state {
                case .ready:
                    box.done = true
                    cont.resume()
                case .failed(let err):
                    box.done = true
                    cont.resume(throwing: err)
                default:
                    break
                }
            }
            sseConn.start(queue: q)
        }
        defer { sseConn.cancel() }

        let sseReq = "GET /sse HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\nAuthorization: Bearer secret-token\r\n\r\n"
        try await sendAll(on: sseConn, data: Data(sseReq.utf8))
        let handshakeData = try await receiveChunk(on: sseConn)
        let handshakeText = String(decoding: handshakeData, as: UTF8.self)
        XCTAssertTrue(handshakeText.contains("event: endpoint"), "Got: \(handshakeText)")
        guard let sessionRange = handshakeText.range(of: "sessionId=") else {
            XCTFail("Missing sessionId in SSE handshake: \(handshakeText)")
            return
        }
        let sessionId = String(handshakeText[sessionRange.upperBound...])
            .trimmingCharacters(in: .whitespacesAndNewlines)

        let initBody = #"{"jsonrpc":"2.0","id":99,"method":"initialize","params":{}}"#
        let postResp = try await rawHTTPExchange(
            port: port,
            request: "POST /message?sessionId=\(sessionId) HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\nAuthorization: Bearer secret-token\r\nContent-Length: \(initBody.utf8.count)\r\n\r\n\(initBody)"
        )
        XCTAssertTrue(postResp.contains("202 Accepted"), "Got: \(postResp)")

        let sseEventData = try await receiveChunk(on: sseConn)
        let sseEventText = String(decoding: sseEventData, as: UTF8.self)
        XCTAssertTrue(sseEventText.contains("event: message"), "Got: \(sseEventText)")
        XCTAssertTrue(sseEventText.contains("droidpath-mcp"), "Got: \(sseEventText)")
    }

    private func sendAll(on conn: NWConnection, data: Data) async throws {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            conn.send(content: data, completion: .contentProcessed { err in
                if let err { cont.resume(throwing: err) } else { cont.resume() }
            })
        }
    }

    private func receiveChunk(on conn: NWConnection) async throws -> Data {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Data, Error>) in
            conn.receive(minimumIncompleteLength: 1, maximumLength: 65536) { data, _, _, err in
                if let err { cont.resume(throwing: err) }
                else { cont.resume(returning: data ?? Data()) }
            }
        }
    }

    private func rawHTTPExchange(port: UInt16, request: String) async throws -> String {
        let conn = NWConnection(host: .ipv4(.loopback), port: NWEndpoint.Port(rawValue: port)!, using: .tcp)
        let q = DispatchQueue(label: "test.droidpath.rawhttp")
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            final class Box: @unchecked Sendable { var done = false }
            let box = Box()
            conn.stateUpdateHandler = { state in
                guard !box.done else { return }
                switch state {
                case .ready:
                    box.done = true
                    cont.resume()
                case .failed(let err):
                    box.done = true
                    cont.resume(throwing: err)
                default:
                    break
                }
            }
            conn.start(queue: q)
        }
        defer { conn.cancel() }
        try await sendAll(on: conn, data: Data(request.utf8))
        var full = Data()
        while true {
            let chunk: Data = try await withCheckedThrowingContinuation { cont in
                conn.receive(minimumIncompleteLength: 1, maximumLength: 65536) { data, _, isComplete, err in
                    if let data, !data.isEmpty { cont.resume(returning: data) }
                    else if isComplete || err != nil { cont.resume(returning: Data()) }
                    else { cont.resume(returning: Data()) }
                }
            }
            if chunk.isEmpty { break }
            full.append(chunk)
        }
        return String(decoding: full, as: UTF8.self)
    }
}
