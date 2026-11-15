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
    private let server: DroidPathMCPServerCore
    private let queue = DispatchQueue(label: "com.droidpath.mcp.http", qos: .userInitiated)
    private let lock = NSLock()

    private var listener: NWListener?
    private var sseConnections: [ObjectIdentifier: NWConnection] = [:]
    private var authToken: String = ""
    public private(set) var port: UInt16 = 8766

    public init(server: DroidPathMCPServerCore = DroidPathMCPServerCore()) {
        self.server = server
    }

    public func start(port: UInt16 = 8766, requireAuthToken: String? = nil) throws {
        stop()
        self.port = port
        self.authToken = requireAuthToken ?? ""

        let nwPort = NWEndpoint.Port(rawValue: port) ?? 8766
        let params = NWParameters.tcp
        params.requiredLocalEndpoint = NWEndpoint.hostPort(host: .ipv4(.loopback), port: nwPort)

        let listener = try NWListener(using: params)
        listener.newConnectionHandler = { [weak self] conn in
            self?.handleConnection(conn)
        }
        listener.start(queue: queue)
        self.listener = listener
        DroidPathManager.logStderr("droidpath-mcp HTTP/SSE server listening on http://127.0.0.1:\(port)/sse")
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
        guard let rawHost, !rawHost.isEmpty else { return false }
        let hostOnly = rawHost.split(separator: ":").first.map(String.init)?.lowercased() ?? ""
        return hostOnly == "127.0.0.1" || hostOnly == "localhost" || hostOnly == "[::1]" || hostOnly == "::1"
    }

    public static func isAllowedOrigin(_ rawOrigin: String?) -> Bool {
        guard let rawOrigin, !rawOrigin.isEmpty else { return true }
        guard let url = URL(string: rawOrigin), let host = url.host?.lowercased() else { return false }
        return host == "127.0.0.1" || host == "localhost" || host == "::1"
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
                if !isComplete && buf.count < 32768 {
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

            let method = String(parts[0])
            let targetPath = String(parts[1]).split(separator: "?").first.map(String.init) ?? "/"

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
                let expected = "Bearer \(self.authToken)"
                if headers["authorization"] != expected {
                    self.sendHTTPResponse(on: connection, status: "401 Unauthorized", contentType: "text/plain", body: Data("Unauthorized".utf8))
                    return
                }
            }

            let contentLength = Int(headers["content-length"] ?? "0") ?? 0
            let bodyStart = headerEndRange.upperBound
            let currentBodyCount = buf.count - bodyStart
            if currentBodyCount < contentLength && !isComplete {
                self.receiveHTTPRequest(on: connection, buffer: buf)
                return
            }

            let bodyData = buf.subdata(in: bodyStart..<min(buf.count, bodyStart + contentLength))

            if method == "GET" && (targetPath == "/sse" || targetPath == "/") {
                let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /message\n\n"
                connection.send(content: Data(header.utf8), completion: .contentProcessed { [weak self] sendErr in
                    if sendErr != nil { connection.cancel(); return }
                    let cid = ObjectIdentifier(connection)
                    self?.lock.withLock { self?.sseConnections[cid] = connection }
                })
                return
            }

            if method == "POST" && (targetPath == "/message" || targetPath == "/") {
                Task {
                    let respData = await self.server.handleRawRequestData(bodyData)
                    if let respData {
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
        let header = "HTTP/1.1 \(status)\r\nContent-Type: \(contentType)\r\nContent-Length: \(body.count)\r\nConnection: close\r\n\r\n"
        var full = Data(header.utf8)
        full.append(body)
        connection.send(content: full, completion: .contentProcessed { _ in
            connection.cancel()
        })
    }
}
