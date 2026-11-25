// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "PixCapMac",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(
            name: "PixCapMac",
            targets: ["PixCapMac"]
        )
    ],
    targets: [
        .executableTarget(
            name: "PixCapMac",
            dependencies: [],
            path: "Sources/PixCapMac",
            cSettings: [
                .headerSearchPath("Bridge")
            ],
            linkerSettings: [
                // Each configuration links its own Rust build. A single
                // hard-coded target/debug path made release bundles embed
                // the unoptimised debug core (about 3x larger and slower).
                .unsafeFlags(["-L../../../target/debug"], .when(configuration: .debug)),
                .unsafeFlags(["-L../../../target/release"], .when(configuration: .release)),
                .linkedLibrary("pixcap_ffi")
            ]
        )
    ]
)
