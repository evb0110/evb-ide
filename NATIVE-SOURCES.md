# Native dependency sources

The release source archive includes the application, Cargo.lock, vendored Rust
dependencies, and their native build scripts. Some build scripts download
native components separately. Rust vendoring alone does not make this an
offline build.

## WebRTC

The LiveKit Rust wrappers are pinned in Cargo.lock to
`fbda05eb278b4a7c5be8fc9b2a92e9b62e732236` of
https://github.com/zed-industries/livekit-rust-sdks. Their `webrtc-sys-build`
selects the native release `webrtc-0001d84-4`.

The native release's build source, patches, and configuration are available at:

- https://github.com/zed-industries/livekit-rust-sdks/tree/55ea4a4e0736d5301170594db0b901d5db060a0d/webrtc-sys/libwebrtc
- https://github.com/zed-industries/livekit-rust-sdks/archive/55ea4a4e0736d5301170594db0b901d5db060a0d.tar.gz
- https://github.com/webrtc-sdk/webrtc/tree/m137_release

The native build's `.gclient` selects WebRTC's `m137_release` branch; its DEPS
file supplies the dependent source revisions. The native release's source
archive is also included as `native/livekit-webrtc-build-source.tar.gz` in
the EVB IDE release source download.

To build the native library, extract that archive and run
`./build_macos.sh --arch arm64 --profile release` from its
`webrtc-sys/libwebrtc` directory. This downloads the WebRTC sources and build
tools using `gclient`, applies the included patches, and produces
`mac-arm64-release`. Set `LK_CUSTOM_WEBRTC` to that directory's absolute path
before building EVB IDE to use your local native build.

The unmodified native distribution's combined WebRTC and third-party license
notices are retained in `assets/WEBRTC-LICENSE.md`, the generated in-app
dependency notices, and the app's `Contents/Resources/licenses` directory.

## Other native dependencies

Build scripts and bundled C/C++ sources for dependencies such as SQLite,
libgit2, zstd, and tree-sitter are included in the vendored crates. Inspect each
crate's build script for platform-specific toolchain requirements. Apple's
SDKs and Metal toolchain are installed separately through Xcode. See EVB.md
for the application build prerequisites.
