import Foundation
import AndroidShadowGraphKit

// MARK: - JSON-RPC 2.0 & MCP Types

public enum MCPJSONValue: Codable, Sendable, Equatable {
    case null
    case bool(Bool)
    case int(Int)
    case double(Double)
    case string(String)
    case array([MCPJSONValue])
    case object([String: MCPJSONValue])

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let b = try? container.decode(Bool.self) {
            self = .bool(b)
        } else if let i = try? container.decode(Int.self) {
            self = .int(i)
        } else if let d = try? container.decode(Double.self) {
            self = .double(d)
        } else if let s = try? container.decode(String.self) {
            self = .string(s)
        } else if let arr = try? container.decode([MCPJSONValue].self) {
            self = .array(arr)
        } else if let obj = try? container.decode([String: MCPJSONValue].self) {
            self = .object(obj)
        } else {
            throw DecodingError.dataCorruptedError(in: container, debugDescription: "Unsupported JSON value")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null: try container.encodeNil()
        case .bool(let b): try container.encode(b)
        case .int(let i): try container.encode(i)
        case .double(let d): try container.encode(d)
        case .string(let s): try container.encode(s)
        case .array(let arr): try container.encode(arr)
        case .object(let obj): try container.encode(obj)
        }
    }

    public var stringValue: String? {
        if case .string(let s) = self { return s }
        return nil
    }

    public var intValue: Int? {
        if case .int(let i) = self { return i }
        if case .double(let d) = self { return Int(d) }
        return nil
    }

    public var boolValue: Bool? {
        if case .bool(let b) = self { return b }
        return nil
    }

    public var objectValue: [String: MCPJSONValue]? {
        if case .object(let o) = self { return o }
        return nil
    }
}

public struct MCPJSONRPCRequest: Codable, Sendable {
    public let jsonrpc: String?
    public let id: MCPJSONValue?
    public let method: String
    public let params: [String: MCPJSONValue]?
}

public struct MCPJSONRPCError: Codable, Sendable {
    public let code: Int
    public let message: String
}

public struct MCPJSONRPCResponse: Codable, Sendable {
    public let jsonrpc: String
    public let id: MCPJSONValue
    public let result: MCPJSONValue?
    public let error: MCPJSONRPCError?

    public init(id: MCPJSONValue, result: MCPJSONValue) {
        self.jsonrpc = "2.0"
        self.id = id
        self.result = result
        self.error = nil
    }

    public init(id: MCPJSONValue, error: MCPJSONRPCError) {
        self.jsonrpc = "2.0"
        self.id = id
        self.result = nil
        self.error = error
    }
}

public struct MCPToolDefinition: Sendable {
    public let name: String
    public let description: String
    public let inputSchema: MCPJSONValue

    public func toJSONValue() -> MCPJSONValue {
        .object([
            "name": .string(name),
            "description": .string(description),
            "inputSchema": inputSchema
        ])
    }
}

public struct MCPToolResult: Sendable {
    public let text: String
    public let isError: Bool

    public init(text: String, isError: Bool = false) {
        self.text = text
        self.isError = isError
    }

    public static func text(_ text: String, isError: Bool = false) -> MCPToolResult {
        MCPToolResult(text: text, isError: isError)
    }

    public func toJSONValue() -> MCPJSONValue {
        .object([
            "content": .array([
                .object([
                    "type": .string("text"),
                    "text": .string(text)
                ])
            ]),
            "isError": .bool(isError)
        ])
    }
}

// MARK: - XML Fallback Parser for Cold-Start uiautomator

public final class DroidPathUITreeFallbackParser: NSObject, XMLParserDelegate {
    private var nodes: [ShadowCompactNode] = []
    private var nodeIndex = 0

    public static func parse(xmlString: String) -> [ShadowCompactNode] {
        guard let start = xmlString.range(of: "<hierarchy"),
              let end = xmlString.range(of: "</hierarchy>", options: .backwards) else {
            return []
        }
        let xmlSlice = String(xmlString[start.lowerBound..<end.upperBound])
        guard let data = xmlSlice.data(using: .utf8) else { return [] }

        let delegate = DroidPathUITreeFallbackParser()
        let parser = XMLParser(data: data)
        parser.delegate = delegate
        parser.parse()
        return delegate.nodes
    }

    public static func parseCenter(from bounds: String) -> (x: Int, y: Int)? {
        let numbers = bounds
            .components(separatedBy: CharacterSet.decimalDigits.inverted)
            .compactMap { Int($0) }
        guard numbers.count == 4 else { return nil }
        return ((numbers[0] + numbers[2]) / 2, (numbers[1] + numbers[3]) / 2)
    }

    public func parser(
        _ parser: XMLParser,
        didStartElement elementName: String,
        namespaceURI: String?,
        qualifiedName qName: String?,
        attributes attributeDict: [String: String] = [:]
    ) {
        guard elementName == "node" else { return }
        let text = (attributeDict["text"] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let resourceId = (attributeDict["resource-id"] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let contentDesc = (attributeDict["content-desc"] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let className = (attributeDict["class"] ?? "").split(separator: ".").last.map(String.init) ?? ""
        let clickable = attributeDict["clickable"] == "true"
        let scrollable = attributeDict["scrollable"] == "true"
        let bounds = attributeDict["bounds"] ?? ""

        let isMeaningful = clickable || scrollable || !text.isEmpty || !contentDesc.isEmpty || !resourceId.isEmpty
        guard isMeaningful, let center = Self.parseCenter(from: bounds) else { return }

        nodes.append(ShadowCompactNode(
            index: nodeIndex,
            text: text,
            resource_id: resourceId,
            content_desc: contentDesc,
            class: className,
            clickable: clickable,
            scrollable: scrollable,
            bounds: bounds,
            center_x: center.x,
            center_y: center.y
        ))
        nodeIndex += 1
    }
}

// MARK: - Standalone DroidPath MCP Server Core

public final class DroidPathMCPServerCore: @unchecked Sendable {
    public static let protocolVersion = "2024-11-05"
    public static let serverVersion = "1.0.1"

    public let manager: DroidPathManager
    public let adbRunner: DroidPathADBRunner

    public init(
        manager: DroidPathManager = .shared,
        adbRunner: DroidPathADBRunner = .shared
    ) {
        self.manager = manager
        self.adbRunner = adbRunner
    }

    public func handleRawRequestData(_ data: Data) async -> Data? {
        let decoder = JSONDecoder()
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]

        let request: MCPJSONRPCRequest
        do {
            request = try decoder.decode(MCPJSONRPCRequest.self, from: data)
        } catch {
            let errResp = MCPJSONRPCResponse(
                id: .null,
                error: MCPJSONRPCError(code: -32700, message: "Parse error: \(error.localizedDescription)")
            )
            return try? encoder.encode(errResp)
        }

        if request.id == nil {
            return nil
        }

        let reqId = request.id ?? .null
        let response: MCPJSONRPCResponse

        switch request.method {
        case "initialize":
            response = MCPJSONRPCResponse(
                id: reqId,
                result: .object([
                    "protocolVersion": .string(Self.protocolVersion),
                    "capabilities": .object([
                        "tools": .object(["listChanged": .bool(false)])
                    ]),
                    "serverInfo": .object([
                        "name": .string("droidpath-mcp"),
                        "version": .string(Self.serverVersion)
                    ])
                ])
            )

        case "ping":
            response = MCPJSONRPCResponse(id: reqId, result: .object([:]))

        case "tools/list":
            let defs = Self.toolDefinitions().map { $0.toJSONValue() }
            response = MCPJSONRPCResponse(id: reqId, result: .object(["tools": .array(defs)]))

        case "tools/call":
            let name = request.params?["name"]?.stringValue ?? ""
            let arguments = request.params?["arguments"]?.objectValue ?? [:]
            if name.isEmpty {
                response = MCPJSONRPCResponse(
                    id: reqId,
                    error: MCPJSONRPCError(code: -32602, message: "Missing tool name in params.name")
                )
            } else {
                let result = await executeTool(name: name, arguments: arguments)
                response = MCPJSONRPCResponse(id: reqId, result: result.toJSONValue())
            }

        default:
            response = MCPJSONRPCResponse(
                id: reqId,
                error: MCPJSONRPCError(code: -32601, message: "Method not found: \(request.method)")
            )
        }

        return try? encoder.encode(response)
    }

    public static func toolDefinitions() -> [MCPToolDefinition] {
        let serialProp: MCPJSONValue = .object([
            "type": .string("string"),
            "description": .string("Optional ADB device serial. Defaults to the first connected device.")
        ])

        return [
            MCPToolDefinition(
                name: "android_navigate_to_node",
                description: "Layer 3 Persistent A* Symbolic UI Router. Computes the shortest path across known screens in the Spatial Graph and executes taps/swipes via live Shadow DOM feedback without intermediate LLM calls.",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([
                        "target": .object([
                            "type": .string("string"),
                            "description": .string("Target UI text, resource-id, package name, or screen title (e.g. 'Night Light', 'Display and touch').")
                        ]),
                        "target_hash": .object([
                            "type": .string("string"),
                            "description": .string("Optional exact target layout hash in 0x... hex or decimal format.")
                        ]),
                        "max_steps": .object([
                            "type": .string("integer"),
                            "description": .string("Maximum navigation hops allowed (1-30). Default: 12.")
                        ]),
                        "step_timeout_ms": .object([
                            "type": .string("integer"),
                            "description": .string("Per-step Shadow DOM settlement timeout in ms (250-10000). Default: 2000.")
                        ]),
                        "device_serial": serialProp
                    ]),
                    "required": .array([])
                ])
            ),
            MCPToolDefinition(
                name: "android_get_ui_tree",
                description: "Reads interactive Android UI elements from the sub-millisecond Kotlin Shadow DOM stream (falls back to uiautomator XML if daemon stream is cold).",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([
                        "device_serial": serialProp
                    ]),
                    "required": .array([])
                ])
            ),
            MCPToolDefinition(
                name: "android_graph_stats",
                description: "Returns Spatial Graph telemetry: learned screen vertices, action edges, active canonical layout hash, daemon state, and persisted graph path.",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([:]),
                    "required": .array([])
                ])
            ),
            MCPToolDefinition(
                name: "android_list_devices",
                description: "Lists connected Android devices visible over USB or Wireless ADB.",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([:]),
                    "required": .array([])
                ])
            ),
            MCPToolDefinition(
                name: "android_tap_element",
                description: "Taps an Android UI element by coordinates (x, y) or semantic text / resource_id / content_desc, recording the transition edge into the Spatial Graph.",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([
                        "x": .object(["type": .string("integer"), "description": .string("Exact screen X coordinate.")]),
                        "y": .object(["type": .string("integer"), "description": .string("Exact screen Y coordinate.")]),
                        "text": .object(["type": .string("string"), "description": .string("Case-insensitive substring match against node text.")]),
                        "resource_id": .object(["type": .string("string"), "description": .string("Match against view resource-id.")]),
                        "content_desc": .object(["type": .string("string"), "description": .string("Substring match against content-desc.")]),
                        "device_serial": serialProp
                    ]),
                    "required": .array([])
                ])
            ),
            MCPToolDefinition(
                name: "android_swipe",
                description: "Performs a touch swipe from (x1, y1) to (x2, y2) and attributes the action to the Spatial Graph.",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([
                        "x1": .object(["type": .string("integer")]),
                        "y1": .object(["type": .string("integer")]),
                        "x2": .object(["type": .string("integer")]),
                        "y2": .object(["type": .string("integer")]),
                        "duration_ms": .object(["type": .string("integer"), "description": .string("Swipe duration in ms (default 300).")]),
                        "device_serial": serialProp
                    ]),
                    "required": .array([.string("x1"), .string("y1"), .string("x2"), .string("y2")])
                ])
            ),
            MCPToolDefinition(
                name: "android_type_text",
                description: "Types ASCII or Unicode text into the focused Android input field with optional press_enter.",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([
                        "text": .object(["type": .string("string"), "description": .string("Text to type.")]),
                        "press_enter": .object(["type": .string("boolean"), "description": .string("Send ENTER keyevent after typing.")]),
                        "device_serial": serialProp
                    ]),
                    "required": .array([.string("text")])
                ])
            ),
            MCPToolDefinition(
                name: "android_keyevent",
                description: "Sends an Android keyevent (e.g. HOME, BACK, ENTER, APP_SWITCH, POWER).",
                inputSchema: .object([
                    "type": .string("object"),
                    "properties": .object([
                        "keycode": .object(["type": .string("string"), "description": .string("Key name or integer keycode.")]),
                        "device_serial": serialProp
                    ]),
                    "required": .array([.string("keycode")])
                ])
            )
        ]
    }

    public func executeTool(name: String, arguments: [String: MCPJSONValue]) async -> MCPToolResult {
        do {
            switch name {
            case "android_graph_stats":
                let stats = await manager.graphStats()
                let obj: MCPJSONValue = .object([
                    "vertex_count": .int(stats.vertexCount),
                    "edge_count": .int(stats.edgeCount),
                    "current_state_hash": stats.currentStateHash.map { .string(Self.hex($0)) } ?? .null,
                    "current_package": .string(stats.currentPackage),
                    "daemon_state": .string(Self.describeConnectionState(stats.connectionState)),
                    "has_fresh_shadow_dom": .bool(stats.hasFreshShadowDOM),
                    "storage_path": .string(stats.storagePath)
                ])
                return .text(Self.encodeJSON(obj), isError: false)

            case "android_list_devices":
                let devices = try await adbRunner.listDevices()
                let rows: [MCPJSONValue] = devices.map {
                    .object([
                        "serial": .string($0.serial),
                        "model": .string($0.model),
                        "connection": .string($0.isWireless ? "wifi" : "usb")
                    ])
                }
                return .text(Self.encodeJSON(.array(rows)), isError: false)

            case "android_get_ui_tree":
                let serial = try await adbRunner.resolveTargetSerial(arguments["device_serial"]?.stringValue)
                await manager.ensureStarted(serial: serial)
                if let cached = await manager.fastCompactTreeIfFresh(maxWaitSeconds: 0.35, maxAgeSeconds: 15.0) {
                    let data = try JSONEncoder().encode(cached)
                    return .text(String(decoding: data, as: UTF8.self), isError: false)
                }
                let xml = try await fetchFallbackUIXML(serial: serial)
                let parsed = DroidPathUITreeFallbackParser.parse(xmlString: xml)
                let data = try JSONEncoder().encode(parsed)
                return .text(String(decoding: data, as: UTF8.self), isError: false)

            case "android_navigate_to_node":
                let targetRaw = arguments["target"]?.stringValue?
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let target = (targetRaw?.isEmpty == false) ? targetRaw : nil
                let targetHash = Self.parseHash(arguments["target_hash"])

                guard target != nil || targetHash != nil else {
                    return .text("Provide either 'target' or 'target_hash'.", isError: true)
                }

                let serial = try await adbRunner.resolveTargetSerial(arguments["device_serial"]?.stringValue)
                let maxSteps = min(max(1, arguments["max_steps"]?.intValue ?? 12), 30)
                let stepTimeoutMs = min(max(250, arguments["step_timeout_ms"]?.intValue ?? 2000), 10_000)

                do {
                    let outcome = try await manager.navigateToNode(
                        serial: serial,
                        target: target,
                        targetHash: targetHash,
                        maxSteps: maxSteps,
                        stepTimeoutMs: stepTimeoutMs
                    )
                    return Self.formatNavigationOutcome(outcome)
                } catch let deviation as PathDeviationError {
                    return Self.formatPathDeviationResult(deviation)
                }

            case "android_tap_element":
                let serial = try await adbRunner.resolveTargetSerial(arguments["device_serial"]?.stringValue)
                if let x = arguments["x"]?.intValue, let y = arguments["y"]?.intValue {
                    await manager.ensureStarted(serial: serial)
                    await manager.recordAction(AttributedUIAction(kind: .tap, targetDescription: "(\(x),\(y))", x: x, y: y))
                    _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "input", "tap", "\(x)", "\(y)"], timeoutSeconds: 5.0)
                    return .text("Tapped at (\(x), \(y)).", isError: false)
                }

                let textQ = arguments["text"]?.stringValue
                let idQ = arguments["resource_id"]?.stringValue
                let descQ = arguments["content_desc"]?.stringValue
                guard textQ != nil || idQ != nil || descQ != nil else {
                    return .text("Provide either (x, y) or one of text, resource_id, content_desc.", isError: true)
                }

                await manager.ensureStarted(serial: serial)
                var resolved: ShadowCompactNode?
                if let t = textQ, idQ == nil, descQ == nil, let n = await manager.findNode(byText: t) {
                    resolved = n.toCompactNode()
                } else if let rid = idQ, textQ == nil, descQ == nil, let n = await manager.findNode(byResourceID: rid) {
                    resolved = n.toCompactNode()
                } else if let desc = descQ, textQ == nil, idQ == nil, let n = await manager.findNode(byContentDesc: desc) {
                    resolved = n.toCompactNode()
                } else if let liveNodes = await manager.fastCompactTreeIfFresh(maxWaitSeconds: 0.25, maxAgeSeconds: 15.0) {
                    resolved = liveNodes.first(where: { node in
                        if let t = textQ?.lowercased(), !node.text.lowercased().contains(t) { return false }
                        if let rid = idQ?.lowercased(), !node.resource_id.lowercased().contains(rid) { return false }
                        if let desc = descQ?.lowercased(), !node.content_desc.lowercased().contains(desc) { return false }
                        return true
                    })
                }

                if resolved == nil {
                    let xml = try await fetchFallbackUIXML(serial: serial)
                    let nodes = DroidPathUITreeFallbackParser.parse(xmlString: xml)
                    resolved = nodes.first(where: { node in
                        if let t = textQ?.lowercased(), !node.text.lowercased().contains(t) { return false }
                        if let rid = idQ?.lowercased(), !node.resource_id.lowercased().contains(rid) { return false }
                        if let desc = descQ?.lowercased(), !node.content_desc.lowercased().contains(desc) { return false }
                        return true
                    })
                }

                guard let match = resolved else {
                    return .text("No matching UI element found.", isError: true)
                }
                await manager.recordAction(AttributedUIAction(
                    kind: .tap,
                    targetDescription: !match.text.isEmpty ? match.text : (!match.content_desc.isEmpty ? match.content_desc : match.resource_id),
                    x: match.center_x,
                    y: match.center_y,
                    selectorResourceId: match.resource_id.isEmpty ? nil : match.resource_id,
                    selectorText: match.text.isEmpty ? nil : match.text,
                    selectorContentDesc: match.content_desc.isEmpty ? nil : match.content_desc
                ))
                _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "input", "tap", "\(match.center_x)", "\(match.center_y)"], timeoutSeconds: 5.0)
                return .text("Tapped '\(!match.text.isEmpty ? match.text : match.resource_id)' at (\(match.center_x), \(match.center_y)).", isError: false)

            case "android_swipe":
                guard let x1 = arguments["x1"]?.intValue,
                      let y1 = arguments["y1"]?.intValue,
                      let x2 = arguments["x2"]?.intValue,
                      let y2 = arguments["y2"]?.intValue else {
                    return .text("Missing required swipe coordinates (x1, y1, x2, y2).", isError: true)
                }
                let duration = min(max(50, arguments["duration_ms"]?.intValue ?? 300), 5000)
                let serial = try await adbRunner.resolveTargetSerial(arguments["device_serial"]?.stringValue)
                await manager.ensureStarted(serial: serial)
                await manager.recordAction(AttributedUIAction(
                    kind: .swipe,
                    targetDescription: "swipe (\(x1),\(y1))->(\(x2),\(y2))",
                    x: x1, y: y1, endX: x2, endY: y2, durationMs: duration
                ))
                _ = try await adbRunner.execute(
                    arguments: ["-s", serial, "shell", "input", "swipe", "\(x1)", "\(y1)", "\(x2)", "\(y2)", "\(duration)"],
                    timeoutSeconds: 6.0
                )
                return .text("Swiped (\(x1),\(y1)) -> (\(x2),\(y2)) in \(duration)ms.", isError: false)

            case "android_type_text":
                guard let text = arguments["text"]?.stringValue, !text.isEmpty else {
                    return .text("Missing 'text' parameter.", isError: true)
                }
                let pressEnter = arguments["press_enter"]?.boolValue ?? false
                let serial = try await adbRunner.resolveTargetSerial(arguments["device_serial"]?.stringValue)
                await manager.ensureStarted(serial: serial)
                await manager.recordAction(AttributedUIAction(
                    kind: .typeText,
                    targetDescription: String(text.prefix(32)),
                    textInput: text,
                    pressEnter: pressEnter
                ))
                let cmd = Self.buildSafeTypeTextCommand(text)
                if !cmd.useClipboard {
                    _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "input", "text", cmd.quotedArg], timeoutSeconds: 5.0)
                } else {
                    _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "cmd", "clipboard", "set", "text", cmd.quotedArg], timeoutSeconds: 4.0)
                    _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "input", "keyevent", "279"], timeoutSeconds: 4.0)
                }
                if pressEnter {
                    _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "input", "keyevent", "66"], timeoutSeconds: 4.0)
                }
                return .text("Typed \(text.count) characters\(pressEnter ? " + ENTER" : "").", isError: false)

            case "android_keyevent":
                guard let raw = arguments["keycode"]?.stringValue?.trimmingCharacters(in: .whitespacesAndNewlines), !raw.isEmpty else {
                    return .text("Missing 'keycode' parameter.", isError: true)
                }
                guard let keyCode = DroidPathActionExecutor.resolveKeyCode(raw) else {
                    return .text("Unsupported or invalid keyevent '\(raw)'.", isError: true)
                }
                let serial = try await adbRunner.resolveTargetSerial(arguments["device_serial"]?.stringValue)
                await manager.ensureStarted(serial: serial)
                await manager.recordAction(AttributedUIAction(kind: .keyevent, targetDescription: raw.uppercased()))
                _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "input", "keyevent", keyCode], timeoutSeconds: 4.0)
                return .text("Sent keyevent \(raw.uppercased()) (\(keyCode)).", isError: false)

            default:
                return .text("Unknown tool '\(name)'.", isError: true)
            }
        } catch {
            return .text("Tool '\(name)' error: \(error.localizedDescription)", isError: true)
        }
    }

    private func fetchFallbackUIXML(serial: String) async throws -> String {
        var xml = (try? await adbRunner.execute(arguments: ["-s", serial, "exec-out", "uiautomator", "dump", "/dev/tty"], timeoutSeconds: 8.0)) ?? ""
        if !xml.contains("<hierarchy") {
            let remoteDump = "/sdcard/droidpath_dump.xml"
            _ = try await adbRunner.execute(arguments: ["-s", serial, "shell", "uiautomator", "dump", remoteDump], timeoutSeconds: 8.0)
            xml = try await adbRunner.execute(arguments: ["-s", serial, "shell", "cat", remoteDump], timeoutSeconds: 6.0)
            _ = try? await adbRunner.execute(arguments: ["-s", serial, "shell", "rm", "-f", remoteDump], timeoutSeconds: 3.0)
        }
        return xml
    }

    // MARK: - Formatting Helpers

    public static func buildSafeTypeTextCommand(_ rawText: String) -> (useClipboard: Bool, quotedArg: String) {
        let sanitized = rawText.replacingOccurrences(of: "\0", with: "")
        let isSimpleASCII = sanitized.unicodeScalars.allSatisfy { scalar in
            scalar.isASCII && scalar.value >= 0x20 && scalar.value != 0x7F
        }
        if isSimpleASCII {
            let spaceEncoded = sanitized.replacingOccurrences(of: " ", with: "%s")
            return (false, DroidPathADBRunner.shellQuote(spaceEncoded))
        } else {
            return (true, DroidPathADBRunner.shellQuote(sanitized))
        }
    }

    public static func hex(_ hash: UInt64) -> String {
        String(format: "0x%016llx", hash)
    }

    public static func parseHash(_ val: MCPJSONValue?) -> UInt64? {
        guard let val else { return nil }
        if let intVal = val.intValue, intVal >= 0 { return UInt64(intVal) }
        guard var str = val.stringValue?.trimmingCharacters(in: .whitespacesAndNewlines), !str.isEmpty else { return nil }
        if str.lowercased().hasPrefix("0x") {
            str = String(str.dropFirst(2))
            return UInt64(str, radix: 16)
        }
        return UInt64(str, radix: 10) ?? UInt64(str, radix: 16)
    }

    private static func describeConnectionState(_ state: DaemonConnectionState) -> String {
        switch state {
        case .stopped: return "stopped"
        case .connecting: return "connecting"
        case .connected: return "connected"
        case .waitingToReconnect(let attempt, _, _): return "waiting_reconnect_\(attempt)"
        case .disconnected: return "disconnected"
        }
    }

    public static func formatNavigationOutcome(_ outcome: ShadowNavigationOutcome) -> MCPToolResult {
        switch outcome {
        case .arrived(let report):
            let summary: [MCPJSONValue] = report.steps.map { step in
                .object([
                    "step": .int(step.stepIndex),
                    "action": .string(step.actionSignature),
                    "target_hash": .string(hex(step.targetHash)),
                    "elapsed_ms": .int(step.elapsedMs),
                    "matched_by_exact_hash": .bool(step.matchedByExactHash),
                    "skipped": .bool(step.wasSkipped)
                ])
            }
            return .text(encodeJSON(.object([
                "status": .string("arrived"),
                "start_hash": .string(hex(report.startHash)),
                "target_hash": .string(hex(report.finalHash)),
                "steps_executed": .int(report.steps.count),
                "total_elapsed_ms": .int(report.totalElapsedMs),
                "path_summary": .array(summary)
            ])), isError: false)

        case .alreadyAtTarget(let stateHash, let activePackage):
            return .text(encodeJSON(.object([
                "status": .string("already_at_target"),
                "current_hash": .string(hex(stateHash)),
                "active_package": .string(activePackage),
                "steps_executed": .int(0),
                "total_elapsed_ms": .int(0)
            ])), isError: false)

        case .noRoute(let startHash, let activePackage, let candidates, let reachableCount, let knownCount):
            return .text(encodeJSON(.object([
                "status": .string("no_route"),
                "current_hash": .string(hex(startHash)),
                "active_package": .string(activePackage),
                "candidate_target_hashes": .array(candidates.map { .string(hex($0)) }),
                "reachable_vertex_count": .int(reachableCount),
                "known_vertex_count": .int(knownCount)
            ])), isError: true)

        case .targetNotFound(let query, let knownCount):
            return .text(encodeJSON(.object([
                "status": .string("target_not_found"),
                "query": .string(query),
                "known_vertex_count": .int(knownCount)
            ])), isError: true)

        case .streamUnavailable:
            return .text(encodeJSON(.object([
                "status": .string("stream_unavailable"),
                "message": .string("Shadow DOM stream is unavailable; ensure android-daemon is running and connected.")
            ])), isError: true)
        }
    }

    public static func formatPathDeviationResult(_ error: PathDeviationError) -> MCPToolResult {
        var obj: [String: MCPJSONValue] = [
            "status": .string("path_deviation"),
            "message": .string(error.description)
        ]
        switch error {
        case .executionAlreadyInProgress:
            obj["error_type"] = .string("execution_already_in_progress")
        case .shadowStreamUnavailable:
            obj["error_type"] = .string("shadow_stream_unavailable")
        case .missingActionPayload(let stepIndex, let edgeId):
            obj["error_type"] = .string("missing_action_payload")
            obj["step_index"] = .int(stepIndex)
            obj["edge_id"] = .string(edgeId.uuidString)
        case .unexpectedStartState(let expectedHash, let actualHash, let actualPackage):
            obj["error_type"] = .string("unexpected_start_state")
            obj["expected_hash"] = .string(hex(expectedHash))
            obj["last_observed_hash"] = actualHash.map { .string(hex($0)) } ?? .null
            obj["last_observed_package"] = .string(actualPackage)
        case .actionDispatchFailed(let stepIndex, let actionSignature, let underlyingMessage):
            obj["error_type"] = .string("action_dispatch_failed")
            obj["step_index"] = .int(stepIndex)
            obj["action"] = .string(actionSignature)
            obj["underlying_error"] = .string(underlyingMessage)
        case .stepTimeout(let stepIndex, let expectedTargetHash, let lastObservedHash, let lastObservedPackage, let timeoutMs):
            obj["error_type"] = .string("step_timeout")
            obj["step_index"] = .int(stepIndex)
            obj["expected_target_hash"] = .string(hex(expectedTargetHash))
            obj["last_observed_hash"] = lastObservedHash.map { .string(hex($0)) } ?? .null
            obj["last_observed_package"] = .string(lastObservedPackage)
            obj["timeout_ms"] = .int(timeoutMs)
        }
        return .text(encodeJSON(.object(obj)), isError: true)
    }

    public static func encodeJSON(_ value: MCPJSONValue) -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        guard let data = try? encoder.encode(value) else { return "{}" }
        return String(decoding: data, as: UTF8.self)
    }
}
