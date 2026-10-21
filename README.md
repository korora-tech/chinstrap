# Chinstrap

A native GPUI screen recorder for Linux. It presents the capture, production, editing, transcription, reliability, and automation features in a Linux desktop layout.

This is currently a reference UI prototype: it does not yet capture media, transcribe audio, or run the listed automation integrations.

## Run

This app uses the upstream GPUI and `gpui_platform` crates from a pinned Zed revision. It needs a recent stable Rust toolchain and the Linux graphics/windowing development packages required by GPUI, including Vulkan, X11, Wayland, xkbcommon, and fontconfig.

```sh
cargo run
```

Both X11 and Wayland backends are enabled.
