import Foundation
import AndroidShadowGraphKit
import DroidPathBridge

struct CLIConfig {
    var mode: String = "stdio"
    var port: UInt16 = 8766
    var daemonPort: UInt16 = 27189
    var graphPath: URL = DroidPathPersistenceCoordinator.defaultStorageURL()
    var daemonJarPath: String? = nil
    var authToken: String? = nil
    var showHelp: Bool = false
    var showVersion: Bool = false
}

func parseArgs(_ args: [String]) -> CLIConfig {
    var cfg = CLIConfig()
    var i = 1
    while i < args.count {
        let arg = args[i]
        switch arg {
        case "--stdio":
            cfg.mode = "stdio"
        case "--http":
            cfg.mode = "http"
        case "--port":
            if i + 1 < args.count, let p = UInt16(args[i + 1]) {
                cfg.port = p
                i += 1
            }
        case "--daemon-port":
            if i + 1 < args.count, let p = UInt16(args[i + 1]) {
                cfg.daemonPort = p
                i += 1
            }
        case "--graph-path":
            if i + 1 < args.count {
                cfg.graphPath = URL(fileURLWithPath: args[i + 1])
                i += 1
            }
        case "--daemon-jar":
            if i + 1 < args.count {
                cfg.daemonJarPath = args[i + 1]
                i += 1
            }
        case "--auth-token":
            if i + 1 < args.count {
                cfg.authToken = args[i + 1]
                i += 1
            }
        case "-h", "--help":
            cfg.showHelp = true
        case "-v", "--version":
            cfg.showVersion = true
        default:
            break
        }
        i += 1
    }
    return cfg
}

private let usageText = """
droidpath-mcp v\(DroidPathMCPServerCore.serverVersion) — Persistent A* Symbolic UI Router & Shadow DOM MCP Server

USAGE:
  droidpath-mcp [--stdio | --http] [OPTIONS]

OPTIONS:
  --stdio                 Run JSON-RPC 2.0 MCP server over stdin/stdout (default)
  --http                  Run loopback HTTP/SSE MCP server on 127.0.0.1:<port>
  --port <port>           HTTP/SSE listen port (default: 8766)
  --daemon-port <port>    Android Shadow DOM daemon TCP forward port (default: 27189)
  --graph-path <path>     Spatial Graph JSON persistence path (default: ~/.droidpath/shadow_graph.json)
  --daemon-jar <path>     Path to single-DEX android-daemon.jar
  --auth-token <token>    Optional Bearer token required on HTTP/SSE requests
  -v, --version           Print version
  -h, --help              Show this help
"""

let cfg = parseArgs(CommandLine.arguments)

if cfg.showVersion {
    print("droidpath-mcp \(DroidPathMCPServerCore.serverVersion)")
    exit(0)
}

if cfg.showHelp {
    print(usageText)
    exit(0)
}

let manager = DroidPathManager(
    port: cfg.daemonPort,
    storageURL: cfg.graphPath,
    daemonJarPath: cfg.daemonJarPath
)
let serverCore = DroidPathMCPServerCore(manager: manager)

if cfg.mode == "http" {
    let http = DroidPathHTTPServer(server: serverCore)
    do {
        try http.start(port: cfg.port, requireAuthToken: cfg.authToken)
        RunLoop.main.run()
    } catch {
        DroidPathManager.logStderr("Failed to start HTTP server: \(error.localizedDescription)")
        exit(1)
    }
} else {
    DroidPathStdioTransport.runBlocking(server: serverCore)
}
