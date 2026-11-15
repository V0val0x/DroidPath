import CryptoKit
import Foundation
import AndroidShadowGraphKit

public struct DroidPathGraphStats: Sendable, Equatable {
    public let vertexCount: Int
    public let edgeCount: Int
    public let currentStateHash: UInt64?
    public let currentPackage: String
    public let connectionState: DaemonConnectionState
    public let hasFreshShadowDOM: Bool
    public let storagePath: String

    public init(
        vertexCount: Int,
        edgeCount: Int,
        currentStateHash: UInt64?,
        currentPackage: String,
        connectionState: DaemonConnectionState,
        hasFreshShadowDOM: Bool,
        storagePath: String
    ) {
        self.vertexCount = vertexCount
        self.edgeCount = edgeCount
        self.currentStateHash = currentStateHash
        self.currentPackage = currentPackage
        self.connectionState = connectionState
        self.hasFreshShadowDOM = hasFreshShadowDOM
        self.storagePath = storagePath
    }
}

public actor DroidPathPersistenceCoordinator {
    private let storageURL: URL
    private var initialized = false
    private var saveTask: Task<Void, Never>?
    private var lastSavedData: Data?

    public init(storageURL: URL) {
        self.storageURL = storageURL
    }

    public nonisolated var path: String { storageURL.path }

    public static func defaultStorageURL() -> URL {
        let base = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".droidpath", isDirectory: true)
        return base.appendingPathComponent("shadow_graph.json", isDirectory: false)
    }

    public func ensureInitialized(engine: SpatialGraphEngine) async {
        guard !initialized else { return }
        initialized = true

        if FileManager.default.fileExists(atPath: storageURL.path) {
            do {
                let data = try Data(contentsOf: storageURL)
                try await engine.importGraphJSON(data)
                lastSavedData = data
            } catch {
                DroidPathManager.logStderr("Failed to load persisted graph at \(storageURL.path): \(error.localizedDescription)")
            }
        }
        await engine.setOnTopologyChanged { [weak self] in
            await self?.scheduleSave(engine: engine)
        }
    }

    public func scheduleSave(engine: SpatialGraphEngine, debounceNs: UInt64 = 1_500_000_000) {
        saveTask?.cancel()
        saveTask = Task {
            try? await Task.sleep(nanoseconds: debounceNs)
            guard !Task.isCancelled else { return }
            await self.writeSnapshotIfChanged(engine: engine)
        }
    }

    public func flush(engine: SpatialGraphEngine) async {
        saveTask?.cancel()
        saveTask = nil
        await writeSnapshotIfChanged(engine: engine)
    }

    private func writeSnapshotIfChanged(engine: SpatialGraphEngine) async {
        let vertexCount = await engine.vertexCount
        let edgeCount = await engine.edgeCount
        guard vertexCount > 0 || edgeCount > 0 else { return }

        do {
            let data = try await engine.exportGraphJSON(prettyPrinted: false)
            if lastSavedData == data { return }
            let dir = storageURL.deletingLastPathComponent()
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try data.write(to: storageURL, options: .atomic)
            lastSavedData = data
        } catch {
            DroidPathManager.logStderr("Failed to save graph at \(storageURL.path): \(error.localizedDescription)")
        }
    }
}

/// Standalone lifecycle coordinator for `ShadowGraphBridge`, persistent Spatial Graph JSON,
/// idempotent SHA-256 `android-daemon.jar` ADB deploy, and self-healing daemon recovery.
public final class DroidPathManager: @unchecked Sendable {
    public static let shared = DroidPathManager()

    public actor LifecycleGate {
        private var currentTask: Task<Void, Never>?

        public func runSerialized(_ work: @escaping @Sendable () async -> Void) async {
            let previous = currentTask
            let next = Task<Void, Never> {
                if let previous { _ = await previous.result }
                await work()
            }
            currentTask = next
            await next.value
        }
    }

    public let bridge: ShadowGraphBridge
    private let persistence: DroidPathPersistenceCoordinator
    private let lifecycle = LifecycleGate()
    private let lock = NSLock()
    private let explicitDaemonJarPath: String?
    private let adbRunner: DroidPathADBRunner

    private var activeSerial: String?
    private var hasStarted = false
    private var graphChangeObserver: Task<Void, Never>?
    private var portForwarder: (@Sendable (String?, UInt16) async -> Void)?
    private var daemonLauncher: (@Sendable (String, UInt16) async throws -> Void)?

    public init(
        port: UInt16 = 27189,
        storageURL: URL = DroidPathPersistenceCoordinator.defaultStorageURL(),
        daemonJarPath: String? = nil,
        adbRunner: DroidPathADBRunner = .shared,
        portForwarder: (@Sendable (String?, UInt16) async -> Void)? = nil,
        daemonLauncher: (@Sendable (String, UInt16) async throws -> Void)? = nil
    ) {
        self.bridge = ShadowGraphBridge(configuration: DaemonEndpointConfiguration(port: port))
        self.persistence = DroidPathPersistenceCoordinator(storageURL: storageURL)
        self.explicitDaemonJarPath = daemonJarPath
        self.adbRunner = adbRunner
        self.portForwarder = portForwarder
        self.daemonLauncher = daemonLauncher

        self.bridge.setDaemonRecoveryHook { [weak self] serial, port in
            guard let self else { return false }
            return await self.recoverDaemonAndAwaitStream(serial: serial, port: port)
        }
    }

    deinit {
        graphChangeObserver?.cancel()
    }

    public func loadPersistedGraphIfNeeded() async {
        await persistence.ensureInitialized(engine: bridge.graphEngine)
    }

    public func flushPersistedGraph() async {
        await persistence.flush(engine: bridge.graphEngine)
    }

    public func ensureStarted(
        serial: String? = nil,
        forceReconnectIfWaiting: Bool = false
    ) async {
        await loadPersistedGraphIfNeeded()
        await lifecycle.runSerialized {
            let validSerial = (serial?.isEmpty == false && DroidPathADBRunner.validateSerial(serial!)) ? serial : nil
            let (shouldConfigureAndStart, serialChanged, acquiredInitialSerial): (Bool, Bool, Bool) = self.lock.withLock {
                let prevSerial = self.activeSerial
                if let validSerial { self.activeSerial = validSerial }
                if !self.hasStarted {
                    self.hasStarted = true
                    return (true, false, false)
                }
                let changed = (validSerial != nil && prevSerial != nil && prevSerial != validSerial)
                let acquired = (validSerial != nil && prevSerial == nil)
                return (false, changed, acquired)
            }

            if shouldConfigureAndStart {
                let port = self.bridge.client.configuration.port
                await self.bridge.setBeforeReconnectHook { [weak self] _ in
                    await self?.forwardDaemonPort(port: port)
                }
                await self.forwardDaemonPort(port: port)
                await self.bridge.start()
            } else {
                let state = await self.bridge.client.state
                switch state {
                case .stopped, .disconnected:
                    let port = self.bridge.client.configuration.port
                    await self.forwardDaemonPort(port: port)
                    await self.bridge.start()
                case .waitingToReconnect:
                    if serialChanged || acquiredInitialSerial || forceReconnectIfWaiting {
                        await self.bridge.reconnect()
                    }
                case .connecting, .connected:
                    if serialChanged || (acquiredInitialSerial && !state.isConnected) {
                        await self.bridge.reconnect()
                    }
                }
            }
        }
    }

    public func stop() async {
        let pendingIndex: ShadowTreeIndex? = if await bridge.domState.hasSessionSnapshot {
            await bridge.domState.currentIndex
        } else {
            nil
        }
        await lifecycle.runSerialized {
            self.lock.withLock { self.hasStarted = false }
            await self.bridge.stop()
        }
        if let pendingIndex {
            let fp = bridge.hasher.fingerprint(for: pendingIndex)
            let currentHash = await bridge.graphEngine.currentStateHash
            if currentHash != fp.layoutHash && (!fp.tokens.isEmpty || !pendingIndex.orderedNodes.isEmpty) {
                await bridge.graphEngine.process(update: ShadowDOMUpdate(
                    index: pendingIndex,
                    isFullSnapshot: true,
                    insertedCount: pendingIndex.orderedNodes.count,
                    updatedCount: 0,
                    removedCount: 0
                ))
            }
        }
        await flushPersistedGraph()
    }

    public func graphStats() async -> DroidPathGraphStats {
        await loadPersistedGraphIfNeeded()
        let vCount = await bridge.graphEngine.vertexCount
        let eCount = await bridge.graphEngine.edgeCount
        let hash = await bridge.graphEngine.currentStateHash
        let pkg = await bridge.domState.currentIndex?.activePackage ?? ""
        let conn = await bridge.client.state
        let fresh = await bridge.domState.isFresh(maxAgeSeconds: 15.0)
        return DroidPathGraphStats(
            vertexCount: vCount,
            edgeCount: eCount,
            currentStateHash: hash,
            currentPackage: pkg,
            connectionState: conn,
            hasFreshShadowDOM: fresh,
            storagePath: persistence.path
        )
    }

    public func fastCompactTreeIfFresh(
        maxWaitSeconds: TimeInterval = 0.25,
        maxAgeSeconds: TimeInterval = 15.0
    ) async -> [ShadowCompactNode]? {
        await bridge.fastCompactTreeIfFresh(maxWaitSeconds: maxWaitSeconds, maxAgeSeconds: maxAgeSeconds)
    }

    public func findNode(byText query: String, preferClickable: Bool = true) async -> ShadowUINode? {
        guard await bridge.domState.isFresh(maxAgeSeconds: 15.0) else { return nil }
        return await bridge.findNode(byText: query, mode: .containsCaseInsensitive, preferClickable: preferClickable)
    }

    public func findNode(byResourceID resourceId: String) async -> ShadowUINode? {
        guard await bridge.domState.isFresh(maxAgeSeconds: 15.0) else { return nil }
        return await bridge.findNode(byResourceID: resourceId, mode: .suffixOrExactResourceId)
    }

    public func findNode(byContentDesc query: String) async -> ShadowUINode? {
        guard await bridge.domState.isFresh(maxAgeSeconds: 15.0) else { return nil }
        return await bridge.findNode(byContentDesc: query, mode: .containsCaseInsensitive)
    }

    public func recordAction(_ action: AttributedUIAction) async {
        await bridge.recordAction(action)
    }

    public func navigateToNode(
        serial: String,
        target: String?,
        targetHash: UInt64?,
        maxSteps: Int = 12,
        stepTimeoutMs: Int = 2000,
        executor: MacroActionExecutor? = nil
    ) async throws -> ShadowNavigationOutcome {
        await loadPersistedGraphIfNeeded()
        await ensureStarted(serial: serial, forceReconnectIfWaiting: true)
        let activeExecutor = executor ?? DroidPathActionExecutor(adbRunner: adbRunner)
        return try await bridge.routeAndExecute(
            targetHint: target,
            targetHash: targetHash,
            serial: serial,
            executor: activeExecutor,
            maxSteps: maxSteps,
            stepTimeoutMs: stepTimeoutMs
        )
    }

    // MARK: - Daemon Auto-Recovery & ADB Forwarding

    private func forwardDaemonPort(port: UInt16, explicitSerial: String? = nil) async {
        let targetSerial = explicitSerial ?? lock.withLock { activeSerial }
        if let portForwarder {
            await portForwarder(targetSerial, port)
            return
        }
        var args: [String] = []
        if let targetSerial, !targetSerial.isEmpty {
            args.append(contentsOf: ["-s", targetSerial])
        }
        args.append(contentsOf: ["forward", "tcp:\(port)", "tcp:\(port)"])
        _ = try? await adbRunner.execute(arguments: args, timeoutSeconds: 1.5)
    }

    internal func recoverDaemonAndAwaitStream(serial: String, port: UInt16) async -> Bool {
        guard DroidPathADBRunner.validateSerial(serial) else { return false }
        await ensureStarted(serial: serial, forceReconnectIfWaiting: false)

        if await bridge.ensureShadowStreamReady(maxWaitSeconds: 0.25, maxAgeSeconds: 15.0),
           await bridge.domState.currentIndex != nil {
            return true
        }

        do {
            if let daemonLauncher {
                try await daemonLauncher(serial, port)
            } else {
                try await launchDaemonOnDevice(serial: serial, port: port)
            }
        } catch is CancellationError {
            return false
        } catch {
            Self.logStderr("Daemon auto-recovery launch failed for \(serial): \(error.localizedDescription)")
            return false
        }

        await forwardDaemonPort(port: port, explicitSerial: serial)
        if await bridge.client.state == .stopped {
            await bridge.start()
        } else {
            await bridge.reconnect()
        }

        let deadline = Date().addingTimeInterval(5.0)
        while Date() < deadline {
            if Task.isCancelled { return false }
            let state = await bridge.client.state
            if state == .stopped { return false }

            let slice = min(1.0, max(0.1, deadline.timeIntervalSinceNow))
            if await bridge.ensureShadowStreamReady(maxWaitSeconds: slice, maxAgeSeconds: 15.0),
               await bridge.domState.currentIndex != nil {
                return true
            }

            if case .waitingToReconnect = state {
                try? await Task.sleep(nanoseconds: 150_000_000)
                await forwardDaemonPort(port: port, explicitSerial: serial)
                await bridge.reconnect()
            } else if state == .disconnected {
                try? await Task.sleep(nanoseconds: 150_000_000)
                await forwardDaemonPort(port: port, explicitSerial: serial)
                await bridge.reconnect()
            }
        }

        let ready = await bridge.ensureShadowStreamReady(maxWaitSeconds: 0.1, maxAgeSeconds: 15.0)
        let hasIndex = await bridge.domState.currentIndex != nil
        return ready && hasIndex
    }

    public func resolveDaemonJarPath() -> String? {
        let env = ProcessInfo.processInfo.environment
        let cwd = FileManager.default.currentDirectoryPath
        let execDir = (CommandLine.arguments.first.map { URL(fileURLWithPath: $0).deletingLastPathComponent().path }) ?? cwd
        let candidates = [
            explicitDaemonJarPath,
            env["DROIDPATH_DAEMON_JAR"],
            Bundle.main.resourceURL?.appendingPathComponent("bin/android-daemon.jar").path,
            Bundle.main.resourceURL?.appendingPathComponent("android-daemon.jar").path,
            execDir + "/android-daemon.jar",
            execDir + "/../share/droidpath/android-daemon.jar",
            cwd + "/android-daemon/build/dist/android-daemon.jar",
            "/usr/local/share/droidpath/android-daemon.jar",
            "/opt/homebrew/share/droidpath/android-daemon.jar"
        ].compactMap { $0 }
        return candidates.first(where: { FileManager.default.fileExists(atPath: $0) })
    }

    private func launchDaemonOnDevice(serial: String, port: UInt16) async throws {
        let remoteJar = "/data/local/tmp/android-daemon.jar"
        let remoteLog = "/data/local/tmp/android-daemon.log"

        if let localJar = resolveDaemonJarPath() {
            let jarData = try Data(contentsOf: URL(fileURLWithPath: localJar))
            let localSha = SHA256.hash(data: jarData).map { String(format: "%02x", $0) }.joined()
            let remoteShaRaw = (try? await adbRunner.execute(
                arguments: ["-s", serial, "shell", "sha256sum '\(remoteJar)' 2>/dev/null || toybox sha256sum '\(remoteJar)' 2>/dev/null"],
                timeoutSeconds: 3.0
            )) ?? ""
            let remoteSha = remoteShaRaw
                .trimmingCharacters(in: .whitespacesAndNewlines)
                .split(whereSeparator: \.isWhitespace)
                .first
                .map(String.init) ?? ""

            if remoteSha != localSha {
                _ = try await adbRunner.execute(
                    arguments: ["-s", serial, "push", localJar, remoteJar],
                    timeoutSeconds: 12.0
                )
                _ = try? await adbRunner.execute(
                    arguments: ["-s", serial, "shell", "chmod", "644", remoteJar],
                    timeoutSeconds: 3.0
                )
            }
        } else {
            let existsOut = (try? await adbRunner.execute(
                arguments: ["-s", serial, "shell", "test -f '\(remoteJar)' && echo OK"],
                timeoutSeconds: 3.0
            )) ?? ""
            guard existsOut.contains("OK") else {
                throw NSError(
                    domain: "DroidPathManager",
                    code: 404,
                    userInfo: [NSLocalizedDescriptionKey: "android-daemon.jar not found locally or at \(remoteJar)."]
                )
            }
        }

        let killCmd = "pkill -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true; i=0; while [ $i -lt 15 ]; do pgrep -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || break; sleep 0.05; i=$((i+1)); done; pkill -9 -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true"
        _ = try? await adbRunner.execute(arguments: ["-s", serial, "shell", killCmd], timeoutSeconds: 4.0)

        let daemonCmd = "env CLASSPATH='\(remoteJar)' app_process / com.shadowgraph.daemon.DaemonMain --port=\(Int(port)) --debounce-ms=20 --max-depth=48 --heartbeat-ms=5000 --no-stdin-watchdog"
        let launchCmd = "rm -f '\(remoteLog)'; if command -v setsid >/dev/null 2>&1; then (setsid \(daemonCmd) </dev/null >'\(remoteLog)' 2>&1 &) </dev/null >/dev/null 2>&1; else (nohup \(daemonCmd) </dev/null >'\(remoteLog)' 2>&1 &) </dev/null >/dev/null 2>&1; fi"
        _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", launchCmd], timeoutSeconds: 4.0)

        let readyDeadline = Date().addingTimeInterval(4.0)
        while Date() < readyDeadline {
            try Task.checkCancellation()
            let checkCmd = "if grep -q 'SHADOWGRAPH_DAEMON_READY' '\(remoteLog)' 2>/dev/null; then echo READY; elif grep -q 'Fatal startup error' '\(remoteLog)' 2>/dev/null; then echo FAILED; fi"
            let out = (try? await adbRunner.execute(arguments: ["-s", serial, "shell", checkCmd], timeoutSeconds: 2.0)) ?? ""
            if out.contains("READY") { return }
            if out.contains("FAILED") {
                let errLog = (try? await adbRunner.execute(arguments: ["-s", serial, "shell", "cat '\(remoteLog)' 2>/dev/null"], timeoutSeconds: 2.0)) ?? ""
                throw NSError(
                    domain: "DroidPathManager",
                    code: 500,
                    userInfo: [NSLocalizedDescriptionKey: "android-daemon failed to start: \(errLog)"]
                )
            }
            try await Task.sleep(nanoseconds: 100_000_000)
        }

        let tailLog = (try? await adbRunner.execute(arguments: ["-s", serial, "shell", "cat '\(remoteLog)' 2>/dev/null"], timeoutSeconds: 2.0)) ?? ""
        let suffix = tailLog.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "" : " Log: \(tailLog)"
        throw NSError(
            domain: "DroidPathManager",
            code: 504,
            userInfo: [NSLocalizedDescriptionKey: "Timed out waiting for SHADOWGRAPH_DAEMON_READY.\(suffix)"]
        )
    }

    public static func logStderr(_ message: String) {
        if let data = "[DroidPath] \(message)\n".data(using: .utf8) {
            FileHandle.standardError.write(data)
        }
    }
}

public struct DroidPathActionExecutor: MacroActionExecutor {
    private let adbRunner: DroidPathADBRunner

    public init(adbRunner: DroidPathADBRunner = .shared) {
        self.adbRunner = adbRunner
    }

    public func execute(action: AttributedUIAction, serial: String) async throws {
        switch action.kind {
        case .tap:
            guard let x = action.x, let y = action.y else {
                throw PathDeviationError.actionDispatchFailed(
                    stepIndex: -1,
                    actionSignature: action.signature,
                    underlyingMessage: "Missing tap coordinates (x, y)."
                )
            }
            _ = try await adbRunner.execute(
                arguments: ["-s", serial, "shell", "input", "tap", "\(x)", "\(y)"],
                timeoutSeconds: 5.0
            )

        case .swipe:
            guard let x1 = action.x, let y1 = action.y, let x2 = action.endX, let y2 = action.endY else {
                throw PathDeviationError.actionDispatchFailed(
                    stepIndex: -1,
                    actionSignature: action.signature,
                    underlyingMessage: "Missing swipe coordinates."
                )
            }
            let duration = max(120, action.durationMs ?? 280)
            _ = try await adbRunner.execute(
                arguments: ["-s", serial, "shell", "input", "swipe", "\(x1)", "\(y1)", "\(x2)", "\(y2)", "\(duration)"],
                timeoutSeconds: 5.0
            )

        case .keyevent:
            guard let keyCode = Self.resolveKeyCode(action.targetDescription) else {
                throw PathDeviationError.actionDispatchFailed(
                    stepIndex: -1,
                    actionSignature: action.signature,
                    underlyingMessage: "Unsupported or unsafe keyevent '\(action.targetDescription)'."
                )
            }
            _ = try await adbRunner.execute(
                arguments: ["-s", serial, "shell", "input", "keyevent", keyCode],
                timeoutSeconds: 4.0
            )

        case .typeText:
            let text = action.textInput ?? action.targetDescription
            guard !text.isEmpty else {
                throw PathDeviationError.actionDispatchFailed(
                    stepIndex: -1,
                    actionSignature: action.signature,
                    underlyingMessage: "Missing textInput payload."
                )
            }
            let cmd = DroidPathMCPServerCore.buildSafeTypeTextCommand(text)
            if !cmd.useClipboard {
                _ = try await adbRunner.execute(
                    arguments: ["-s", serial, "shell", "input", "text", cmd.quotedArg],
                    timeoutSeconds: 5.0
                )
            } else {
                _ = try await adbRunner.execute(
                    arguments: ["-s", serial, "shell", "cmd", "clipboard", "set", "text", cmd.quotedArg],
                    timeoutSeconds: 4.0
                )
                _ = try await adbRunner.execute(
                    arguments: ["-s", serial, "shell", "input", "keyevent", "279"],
                    timeoutSeconds: 4.0
                )
            }
            if action.pressEnter == true {
                _ = try await adbRunner.execute(
                    arguments: ["-s", serial, "shell", "input", "keyevent", "66"],
                    timeoutSeconds: 4.0
                )
            }

        case .launchApp:
            let pkg = action.targetDescription.trimmingCharacters(in: .whitespacesAndNewlines)
            guard DroidPathADBRunner.validatePackageName(pkg) else {
                throw PathDeviationError.actionDispatchFailed(
                    stepIndex: -1,
                    actionSignature: action.signature,
                    underlyingMessage: "Invalid package name '\(pkg)'."
                )
            }
            _ = try await adbRunner.execute(
                arguments: ["-s", serial, "shell", "monkey", "-p", pkg, "-c", "android.intent.category.LAUNCHER", "1"],
                timeoutSeconds: 6.0
            )
        }
    }

    public static func resolveKeyCode(_ raw: String) -> String? {
        var trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines).uppercased()
        if trimmed.hasPrefix("KEYCODE_") {
            trimmed = String(trimmed.dropFirst("KEYCODE_".count))
        }
        guard !trimmed.isEmpty else { return nil }
        if trimmed.allSatisfy(\.isNumber) { return trimmed }
        let map: [String: String] = [
            "BACK": "4", "HOME": "3", "RECENTS": "187", "APP_SWITCH": "187",
            "ENTER": "66", "DELETE": "67", "VOLUME_UP": "24", "VOLUME_DOWN": "25",
            "POWER": "26", "WAKEUP": "224", "NOTIFICATION": "83", "PASTE": "279"
        ]
        return map[trimmed]
    }
}
