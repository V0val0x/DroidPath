import Foundation
import Network

// MARK: - Stdio Transport

public enum DroidPathStdioTransport {
    public static func runBlocking(server: DroidPathMCPServerCore = DroidPathMCPServerCore()) {
        DroidPathManager.logStderr("droidpath-mcp stdio server started (protocol \(DroidPathMCPServerCore.protocolVersion))")

        let loadSem = DispatchSemaphore(value: 0)
        Task {
            await server.manager.loadPersistedGraphIfNeeded()
            loadSem.signal()
        }
        loadSem.wait()

        let stdinHandle = FileHandle.standardInput
        let stdoutHandle = FileHandle.standardOutput
        var buffer = Data()

        while true {
            let chunk = stdinHandle.availableData
            if chunk.isEmpty { break }
            buffer.append(chunk)

            while let newlineIndex = buffer.firstIndex(of: 0x0A) {
                let lineData = buffer.subdata(in: buffer.startIndex..<newlineIndex)
                buffer.removeSubrange(buffer.startIndex...newlineIndex)

                let trimmed = String(data: lineData, encoding: .utf8)?
                    .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                guard !trimmed.isEmpty, let reqData = trimmed.data(using: .utf8) else { continue }

                let semaphore = DispatchSemaphore(value: 0)
                var responseData: Data?
                Task {
                    responseData = await server.handleRawRequestData(reqData)
                    semaphore.signal()
                }
                semaphore.wait()

                if var resp = responseData {
                    resp.append(0x0A)
                    stdoutHandle.write(resp)
                }
            }
        }

        let stopSem = DispatchSemaphore(value: 0)
        Task.detached {
            await server.manager.stop()
            stopSem.signal()
        }
        _ = stopSem.wait(timeout: .now() + 2.5)
        DroidPathManager.logStderr("droidpath-mcp stdio server exiting on EOF")
    }
}

// MARK: - Loopback HTTP/SSE Transport

public final class DroidPathHTTPServer: @unchecked Sendable {
    private static let maxHeaderBytes = 32_768
    private static let maxBodyBytes = 1_048_576

    private let server: DroidPathMCPServerCore
    private let queue = DispatchQueue(label: "com.droidpath.mcp.http", qos: .userInitiated)
    private let lock = NSLock()

    private var listener: NWListener?
    private var sseConnections: [String: NWConnection] = [:]
    private var authToken: String = ""
    public private(set) var port: UInt16 = 8766

    public init(server: DroidPathMCPServerCore = DroidPathMCPServerCore()) {
        self.server = server
    }

    public func start(port: UInt16 = 8766, requireAuthToken: String? = nil) throws {
        stop()
        self.port = port
        self.authToken = requireAuthToken ?? ""

        let nwPort: NWEndpoint.Port = port == 0 ? .any : (NWEndpoint.Port(rawValue: port) ?? 8766)
        let params = NWParameters.tcp
        params.allowLocalEndpointReuse = true
        params.requiredLocalEndpoint = NWEndpoint.hostPort(host: .ipv4(.loopback), port: nwPort)

        let readySem = DispatchSemaphore(value: 0)
        final class StartupBox: @unchecked Sendable {
            var startError: Error?
            var resolvedPort: UInt16?
            var didSignal = false
        }
        let box = StartupBox()

        let listener = try NWListener(using: params)
        listener.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                if let assigned = listener.port?.rawValue {
                    self?.port = assigned
                    box.resolvedPort = assigned
                }
                if !box.didSignal {
                    box.didSignal = true
                    readySem.signal()
                }
            case .failed(let err):
                box.startError = err
                if !box.didSignal {
                    box.didSignal = true
                    readySem.signal()
                }
            case .cancelled:
                if !box.didSignal {
                    box.didSignal = true
                    readySem.signal()
                }
            default:
                break
            }
        }
        listener.newConnectionHandler = { [weak self] conn in
            self?.handleConnection(conn)
        }
        self.listener = listener
        listener.start(queue: queue)
        _ = readySem.wait(timeout: .now() + 2.0)
        if let startError = box.startError {
            stop()
            throw startError
        }
        let boundPort = box.resolvedPort ?? port
        DroidPathManager.logStderr("droidpath-mcp HTTP/SSE server listening on http://127.0.0.1:\(boundPort)/sse")
    }

    public func stop() {
        listener?.cancel()
        listener = nil
        let conns: [NWConnection] = lock.withLock {
            let list = Array(sseConnections.values)
            sseConnections.removeAll()
            return list
        }
        for c in conns { c.cancel() }
    }

    public static func isAllowedLoopbackHost(_ rawHost: String?) -> Bool {
        guard let rawHost else { return false }
        let trimmed = rawHost.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !trimmed.isEmpty else { return false }
        guard let parsedHost = URL(string: "http://\(trimmed)")?.host?.lowercased() else { return false }
        return parsedHost == "127.0.0.1" || parsedHost == "localhost" || parsedHost == "::1" || parsedHost == "[::1]"
    }

    public static func isAllowedOrigin(_ rawOrigin: String?) -> Bool {
        guard let rawOrigin else { return true }
        let trimmed = rawOrigin.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return true }
        guard let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              let host = url.host?.lowercased() else {
            return false
        }
        return host == "127.0.0.1" || host == "localhost" || host == "::1" || host == "[::1]"
    }

    private func handleConnection(_ connection: NWConnection) {
        connection.start(queue: queue)
        receiveHTTPRequest(on: connection, buffer: Data())
    }

    private func receiveHTTPRequest(on connection: NWConnection, buffer: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65536) { [weak self] data, _, isComplete, error in
            guard let self else { connection.cancel(); return }
            if error != nil { connection.cancel(); return }

            var buf = buffer
            if let data { buf.append(data) }

            let separator = Data([0x0D, 0x0A, 0x0D, 0x0A])
            guard let headerEndRange = buf.range(of: separator) else {
                if !isComplete && buf.count < Self.maxHeaderBytes {
                    self.receiveHTTPRequest(on: connection, buffer: buf)
                } else {
                    connection.cancel()
                }
                return
            }

            let headerData = buf.subdata(in: 0..<headerEndRange.lowerBound)
            let headerText = String(data: headerData, encoding: .utf8) ?? ""
            let lines = headerText.components(separatedBy: "\r\n")
            guard let requestLine = lines.first else { connection.cancel(); return }
            let parts = requestLine.split(separator: " ", omittingEmptySubsequences: true)
            guard parts.count >= 2 else { connection.cancel(); return }

            let method = String(parts[0]).uppercased()
            let rawTarget = String(parts[1])
            let targetPath = rawTarget.split(separator: "?", maxSplits: 1).first.map(String.init) ?? "/"

            var headers: [String: String] = [:]
            for line in lines.dropFirst() {
                if let colon = line.firstIndex(of: ":") {
                    let k = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
                    let v = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
                    headers[k] = v
                }
            }

            guard Self.isAllowedLoopbackHost(headers["host"]), Self.isAllowedOrigin(headers["origin"]) else {
                self.sendHTTPResponse(on: connection, status: "403 Forbidden", contentType: "text/plain", body: Data("Forbidden host/origin".utf8))
                return
            }

            if !self.authToken.isEmpty {
                let authHeader = headers["authorization"] ?? ""
                guard authHeader.lowercased().hasPrefix("bearer ") else {
                    self.sendHTTPResponse(on: connection, status: "401 Unauthorized", contentType: "text/plain", body: Data("Unauthorized".utf8))
                    return
                }
                let provided = String(authHeader.dropFirst("bearer ".count)).trimmingCharacters(in: .whitespaces)
                guard Self.constantTimeEquals(provided, self.authToken) else {
                    self.sendHTTPResponse(on: connection, status: "401 Unauthorized", contentType: "text/plain", body: Data("Unauthorized".utf8))
                    return
                }
            }

            if method == "OPTIONS" {
                self.sendHTTPResponse(on: connection, status: "204 No Content", contentType: "text/plain", body: Data())
                return
            }

            let contentLength: Int
            if let rawCL = headers["content-length"] {
                guard let parsed = Int(rawCL), parsed >= 0 else {
                    self.sendHTTPResponse(on: connection, status: "400 Bad Request", contentType: "text/plain", body: Data("Invalid Content-Length".utf8))
                    return
                }
                guard parsed <= Self.maxBodyBytes else {
                    self.sendHTTPResponse(on: connection, status: "413 Payload Too Large", contentType: "text/plain", body: Data("Payload Too Large".utf8))
                    return
                }
                contentLength = parsed
            } else {
                contentLength = 0
            }

            let bodyStart = headerEndRange.upperBound
            let currentBodyCount = buf.count - bodyStart
            if currentBodyCount < contentLength {
                if isComplete {
                    self.sendHTTPResponse(on: connection, status: "400 Bad Request", contentType: "text/plain", body: Data("Incomplete request body".utf8))
                } else {
                    self.receiveHTTPRequest(on: connection, buffer: buf)
                }
                return
            }

            let bodyData = buf.subdata(in: bodyStart..<(bodyStart + contentLength))

            if method == "GET" && (targetPath == "/sse" || targetPath == "/") {
                let sessionId = UUID().uuidString
                let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /message?sessionId=\(sessionId)\n\n"
                connection.stateUpdateHandler = { [weak self] state in
                    switch state {
                    case .failed, .cancelled:
                        _ = self?.lock.withLock { self?.sseConnections.removeValue(forKey: sessionId) }
                    default:
                        break
                    }
                }
                self.lock.withLock { self.sseConnections[sessionId] = connection }
                connection.send(content: Data(header.utf8), completion: .contentProcessed { [weak self] sendErr in
                    if sendErr != nil {
                        _ = self?.lock.withLock { self?.sseConnections.removeValue(forKey: sessionId) }
                        connection.cancel()
                    }
                })
                return
            }

            if method == "GET" && targetPath == "/health" {
                let health = "{\"server\":\"droidpath-mcp\",\"version\":\"\(DroidPathMCPServerCore.serverVersion)\",\"protocol\":\"\(DroidPathMCPServerCore.protocolVersion)\",\"status\":\"running\"}"
                self.sendHTTPResponse(on: connection, status: "200 OK", contentType: "application/json", body: Data(health.utf8))
                return
            }

            if method == "POST" && (targetPath == "/message" || targetPath == "/") {
                let sessionId = Self.extractQueryParam("sessionId", from: rawTarget)
                Task {
                    let respData = await self.server.handleRawRequestData(bodyData)
                    if let sessionId,
                       let sseConn = self.lock.withLock({ self.sseConnections[sessionId] }),
                       let respData,
                       let jsonString = String(data: respData, encoding: .utf8) {
                        let sseEvent = "event: message\ndata: \(jsonString)\n\n"
                        sseConn.send(content: Data(sseEvent.utf8), completion: .contentProcessed { [weak self] err in
                            if err != nil {
                                _ = self?.lock.withLock { self?.sseConnections.removeValue(forKey: sessionId) }
                                sseConn.cancel()
                            }
                        })
                        self.sendHTTPResponse(on: connection, status: "202 Accepted", contentType: "application/json", body: Data("{\"ok\":true}".utf8))
                    } else if let respData {
                        self.sendHTTPResponse(on: connection, status: "200 OK", contentType: "application/json", body: respData)
                    } else {
                        self.sendHTTPResponse(on: connection, status: "202 Accepted", contentType: "application/json", body: Data())
                    }
                }
                return
            }

            self.sendHTTPResponse(on: connection, status: "404 Not Found", contentType: "text/plain", body: Data("Not Found".utf8))
        }
    }

    private func sendHTTPResponse(on connection: NWConnection, status: String, contentType: String, body: Data) {
        let header = "HTTP/1.1 \(status)\r\nContent-Type: \(contentType)\r\nContent-Length: \(body.count)\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        var full = Data(header.utf8)
        full.append(body)
        connection.send(content: full, completion: .contentProcessed { _ in
            connection.cancel()
        })
    }

    private static func extractQueryParam(_ key: String, from rawTarget: String) -> String? {
        guard let qIndex = rawTarget.firstIndex(of: "?") else { return nil }
        let query = rawTarget[rawTarget.index(after: qIndex)...]
        for pair in query.split(separator: "&") {
            let kv = pair.split(separator: "=", maxSplits: 1)
            if kv.count == 2, String(kv[0]) == key {
                return String(kv[1])
            }
        }
        return nil
    }

    private static func constantTimeEquals(_ lhs: String, _ rhs: String) -> Bool {
        let a = Array(lhs.utf8)
        let b = Array(rhs.utf8)
        guard a.count == b.count else { return false }
        var diff: UInt8 = 0
        for i in 0..<a.count {
            diff |= a[i] ^ b[i]
        }
        return diff == 0
    }
}
