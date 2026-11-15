import Foundation

public struct DroidPathDevice: Sendable, Equatable {
    public let serial: String
    public let model: String
    public let isWireless: Bool

    public init(serial: String, model: String, isWireless: Bool) {
        self.serial = serial
        self.model = model
        self.isWireless = isWireless
    }
}

public enum DroidPathADBError: Error, LocalizedError, Sendable {
    case binaryNotFound
    case invalidSerial(String)
    case commandFailed(exitCode: Int32, stderr: String)
    case timedOut(seconds: TimeInterval)

    public var errorDescription: String? {
        switch self {
        case .binaryNotFound:
            return "ADB executable not found. Install android-platform-tools (`brew install android-platform-tools`) or set ANDROID_HOME."
        case .invalidSerial(let serial):
            return "Invalid ADB device serial '\(serial)'."
        case .commandFailed(let code, let stderr):
            return "ADB failed (exit \(code)): \(stderr)"
        case .timedOut(let seconds):
            return "ADB command timed out after \(String(format: "%.1f", seconds))s."
        }
    }
}

/// Minimal, zero-dependency ADB subprocess runner with strict serial validation and timeout termination.
public actor DroidPathADBRunner {
    public static let shared = DroidPathADBRunner()

    private var cachedADBPath: String?

    public init(adbPath: String? = nil) {
        self.cachedADBPath = adbPath
    }

    public static func validateSerial(_ serial: String) -> Bool {
        let trimmed = serial.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed.count <= 128, !trimmed.hasPrefix("-") else { return false }
        let allowed = CharacterSet(charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789.:_-")
        return trimmed.unicodeScalars.allSatisfy { allowed.contains($0) }
    }

    public static func resolveADBPath() -> String? {
        let env = ProcessInfo.processInfo.environment
        var candidates: [String] = []
        if let explicit = env["DROIDPATH_ADB_PATH"], !explicit.isEmpty {
            candidates.append(explicit)
        }
        if let androidHome = env["ANDROID_HOME"], !androidHome.isEmpty {
            candidates.append("\(androidHome)/platform-tools/adb")
        }
        if let sdkRoot = env["ANDROID_SDK_ROOT"], !sdkRoot.isEmpty {
            candidates.append("\(sdkRoot)/platform-tools/adb")
        }
        candidates.append(contentsOf: [
            "/opt/homebrew/bin/adb",
            "/usr/local/bin/adb",
            "/Applications/AndOnMac.app/Contents/Resources/bin/adb"
        ])
        if let pathEnv = env["PATH"] {
            for dir in pathEnv.split(separator: ":") {
                candidates.append("\(dir)/adb")
            }
        }
        let fm = FileManager.default
        return candidates.first(where: { fm.isExecutableFile(atPath: $0) })
    }

    public func adbBinaryPath() throws -> String {
        if let cachedADBPath, FileManager.default.isExecutableFile(atPath: cachedADBPath) {
            return cachedADBPath
        }
        guard let resolved = Self.resolveADBPath() else {
            throw DroidPathADBError.binaryNotFound
        }
        cachedADBPath = resolved
        return resolved
    }

    public func execute(arguments: [String], timeoutSeconds: TimeInterval = 5.0) async throws -> String {
        let adbPath = try adbBinaryPath()

        let process = Process()
        process.executableURL = URL(fileURLWithPath: adbPath)
        process.arguments = arguments

        let stdoutPipe = Pipe()
        let stderrPipe = Pipe()
        process.standardOutput = stdoutPipe
        process.standardError = stderrPipe
        process.standardInput = FileHandle.nullDevice

        final class PipeAccumulator: @unchecked Sendable {
            private let lock = NSLock()
            private var out = Data()
            private var err = Data()

            func appendOut(_ chunk: Data) {
                guard !chunk.isEmpty else { return }
                lock.lock()
                out.append(chunk)
                lock.unlock()
            }

            func appendErr(_ chunk: Data) {
                guard !chunk.isEmpty else { return }
                lock.lock()
                err.append(chunk)
                lock.unlock()
            }

            func snapshot() -> (Data, Data) {
                lock.lock()
                defer { lock.unlock() }
                return (out, err)
            }
        }

        let acc = PipeAccumulator()
        stdoutPipe.fileHandleForReading.readabilityHandler = { handle in
            acc.appendOut(handle.availableData)
        }
        stderrPipe.fileHandleForReading.readabilityHandler = { handle in
            acc.appendErr(handle.availableData)
        }

        try process.run()

        let deadline = Date().addingTimeInterval(max(0.2, timeoutSeconds))
        while process.isRunning {
            if Task.isCancelled {
                process.terminate()
                stdoutPipe.fileHandleForReading.readabilityHandler = nil
                stderrPipe.fileHandleForReading.readabilityHandler = nil
                throw CancellationError()
            }
            if Date() >= deadline {
                process.terminate()
                try? await Task.sleep(nanoseconds: 50_000_000)
                if process.isRunning {
                    kill(process.processIdentifier, SIGKILL)
                }
                stdoutPipe.fileHandleForReading.readabilityHandler = nil
                stderrPipe.fileHandleForReading.readabilityHandler = nil
                throw DroidPathADBError.timedOut(seconds: timeoutSeconds)
            }
            try await Task.sleep(nanoseconds: 25_000_000)
        }

        stdoutPipe.fileHandleForReading.readabilityHandler = nil
        stderrPipe.fileHandleForReading.readabilityHandler = nil
        acc.appendOut(stdoutPipe.fileHandleForReading.readDataToEndOfFile())
        acc.appendErr(stderrPipe.fileHandleForReading.readDataToEndOfFile())

        let (outData, errData) = acc.snapshot()
        let outStr = String(data: outData, encoding: .utf8) ?? ""
        let errStr = String(data: errData, encoding: .utf8) ?? ""

        if process.terminationStatus != 0 {
            throw DroidPathADBError.commandFailed(
                exitCode: process.terminationStatus,
                stderr: errStr.trimmingCharacters(in: .whitespacesAndNewlines)
            )
        }
        return outStr
    }

    public func listDevices() async throws -> [DroidPathDevice] {
        let output = try await execute(arguments: ["devices", "-l"], timeoutSeconds: 4.0)
        return Self.parseDevicesOutput(output)
    }

    public static func parseDevicesOutput(_ output: String) -> [DroidPathDevice] {
        var devices: [DroidPathDevice] = []
        for rawLine in output.components(separatedBy: .newlines) {
            let line = rawLine.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !line.isEmpty, !line.hasPrefix("List of devices"), !line.hasPrefix("*") else { continue }
            let parts = line.split(whereSeparator: \.isWhitespace).map(String.init)
            guard parts.count >= 2, parts[1] == "device" else { continue }
            let serial = parts[0]
            guard validateSerial(serial) else { continue }
            let modelToken = parts.first(where: { $0.hasPrefix("model:") })
            let model = modelToken?
                .replacingOccurrences(of: "model:", with: "")
                .replacingOccurrences(of: "_", with: " ") ?? serial
            let isWireless = serial.contains(":") || serial.contains("_adb-tls-connect")
            devices.append(DroidPathDevice(serial: serial, model: model, isWireless: isWireless))
        }
        return devices
    }

    public func resolveTargetSerial(_ requested: String?) async throws -> String {
        if let requested, !requested.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            let trimmed = requested.trimmingCharacters(in: .whitespacesAndNewlines)
            guard Self.validateSerial(trimmed) else {
                throw DroidPathADBError.invalidSerial(trimmed)
            }
            return trimmed
        }
        let devices = try await listDevices()
        guard let first = devices.first else {
            throw DroidPathADBError.commandFailed(exitCode: 1, stderr: "No authorized Android devices found via adb devices.")
        }
        return first.serial
    }
}
