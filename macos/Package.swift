// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "Hearth",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "Hearth", targets: ["Hearth"]),
        .library(name: "HearthCore", targets: ["HearthCore"]),
    ],
    targets: [
        .target(name: "HearthCore"),
        .executableTarget(name: "Hearth", dependencies: ["HearthCore"]),
        .testTarget(name: "HearthCoreTests", dependencies: ["HearthCore"]),
        .testTarget(name: "HearthTests", dependencies: ["Hearth", "HearthCore"]),
    ]
)
