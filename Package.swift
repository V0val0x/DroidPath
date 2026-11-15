// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "DroidPath",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .library(
            name: "AndroidShadowGraphKit",
            targets: ["AndroidShadowGraphKit"]
        ),
        .library(
            name: "DroidPathBridge",
            targets: ["DroidPathBridge"]
        ),
        .executable(
            name: "droidpath-mcp",
            targets: ["DroidPathCLI"]
        )
    ],
    targets: [
        .target(
            name: "AndroidShadowGraphKit",
            path: "Sources/AndroidShadowGraphKit"
        ),
        .target(
            name: "DroidPathBridge",
            dependencies: ["AndroidShadowGraphKit"],
            path: "Sources/DroidPathBridge"
        ),
        .executableTarget(
            name: "DroidPathCLI",
            dependencies: ["DroidPathBridge", "AndroidShadowGraphKit"],
            path: "Sources/DroidPathCLI"
        ),
        .testTarget(
            name: "AndroidShadowGraphKitTests",
            dependencies: ["AndroidShadowGraphKit"],
            path: "Tests/AndroidShadowGraphKitTests"
        ),
        .testTarget(
            name: "DroidPathBridgeTests",
            dependencies: ["DroidPathBridge", "AndroidShadowGraphKit"],
            path: "Tests/DroidPathBridgeTests"
        )
    ]
)
