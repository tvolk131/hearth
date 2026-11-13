// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "PveDevApp",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "PveDevApp", targets: ["PveDevApp"]),
        .library(name: "PveDevCore", targets: ["PveDevCore"]),
    ],
    targets: [
        .target(name: "PveDevCore"),
        .executableTarget(name: "PveDevApp", dependencies: ["PveDevCore"]),
        .testTarget(name: "PveDevCoreTests", dependencies: ["PveDevCore"]),
        .testTarget(name: "PveDevAppTests", dependencies: ["PveDevApp", "PveDevCore"]),
    ]
)
