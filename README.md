# Chinstrap

A native recording studio for Linux, built with GPUI and GStreamer. Chinstrap records a camera, a microphone and any number of screens and windows at the same time. Each input goes to its own file, and all of them run on one shared clock so they can be lined up in an editor. Recordings can be cut into segments without stopping.

The window opens on the **Studio** view, which is the recorder. The **Feature guide** tab (`ctrl-2`) is the earlier reference UI for planned features. Nothing on that tab captures, transcribes or automates anything.

## Run

```sh
nix develop -c cargo run --release            # build and launch
nix run                                       # or build the package and launch it
nix develop -c cargo run -- --output ~/Takes  # choose the output folder
```

The dev shell provides the Rust toolchain, GPUI's native dependencies and the GStreamer plugins. Those plugins are `gst-plugins-base`, `-good`, `-bad` and `-ugly`, plus PipeWire's `pipewiresrc`. Screen and window capture use the xdg-desktop-portal ScreenCast interface through PipeWire, so you need a portal backend for your compositor (for example GNOME, KDE, wlroots or niri). Wayland and X11 are both supported.

### Keys (Studio view)

| Key      | Action                                               |
|----------|------------------------------------------------------|
| `r`      | Start recording, or stop if already recording        |
| `c`      | Cut: close the current segment and start the next one |
| `ctrl-1` | Studio view                                          |
| `ctrl-2` | Feature guide                                        |
| `ctrl-q` | Quit (any recording in progress is finalized first)  |

Use the panel to pick a camera and a microphone. **Add windows/screens…** opens the portal picker, and **Add test pattern** adds a synthetic source.

## Output

```
<output>/                          default: $XDG_VIDEOS_DIR/Chinstrap (or $CHINSTRAP_OUTPUT_DIR)
  2026-10-21T18-31-02/             one folder per session
    session.json                   manifest: segments, base times, per-input size, rate, encoder, status
    segment-001/
      camera.mkv                   H.264, camera's native size and frame rate
      mic.mkv                      Opus, 48 kHz
      screen-3.mkv / window-4.mkv  H.264, one file per portal stream
      test-1.mkv                   H.264 test pattern
    segment-002/ …
```

Every file in a segment is timestamped against the same base time. The manifest records the first and last timestamp of each file, so drift and start latency can be measured. Each manifest write is atomic, and quitting during a recording finalizes the files first.

## 4K recording

- **Cameras** record at their native resolution and frame rate, up to 4096×2160 and 60 fps. A 25 fps camera stays at 25 fps.
  - Chinstrap picks the camera mode with the highest resolution at or under 4K, then the highest frame rate. Modes of about 24 fps or more are preferred over slower ones.
  - When raw video and MJPEG reach the same size and rate, raw is used. MJPEG is the next choice, and DMA-BUF is used only when nothing else works.
  - A camera is downscaled only when its mode is larger than 4096×2160.
  - Virtual and loopback cameras that accept any size (size or rate ranges, no native mode) record at 1920×1080 and at most 30 fps, rather than being pushed to 4K60.
  - The camera list shows what will be recorded, for example `3840×2160 @25 · NV12`.
- **Screens and windows** are locked to the size of their first frame, up to 3840×2160, at 30 fps. Set `CHINSTRAP_SCREEN_FPS` (1–60) to change the rate.
- **Encoding** uses H.264, preferring `vah264enc`, then `x264enc`, then `openh264enc`.
  - At startup each encoder is timed on 3840×2160 frames (at most about 4 s per encoder), and each input gets the first encoder that can keep up with its size and rate. The timing is for one stream; inputs sharing an encoder are not added up (on the tested AMD GPU two concurrent 4K encodes each kept the single-stream rate).
  - Screens and the test pattern only get an encoder that the GPU conversion path was checked with at startup.
  - If a hardware encoder cannot do 4K, 4K inputs fall back to the next encoder.
  - If no encoder can do 4K, inputs are downscaled to 1080p.
  - The H.264 level is chosen by the encoder: 5.1 for 4K at 25 or 30 fps.
- **Bitrate** follows the pixel rate: 0.18 bits per pixel for screens (about 11 Mbit/s at 1080p30 and 45 Mbit/s at 4K30) and 0.12 for cameras (about 25 Mbit/s at 4K25). Nothing goes below 0.1 bits per pixel. Set `CHINSTRAP_SCREEN_BITRATE_KBPS` or `CHINSTRAP_CAMERA_BITRATE_KBPS` to use a fixed bitrate instead.
  - These are targets. In short tests `vah264enc` (CBR) came in about 20–30% over target on a busy 4K test pattern and about 35% under on a mostly static camera scene.

## Configuration

| Variable | Meaning |
|---|---|
| `CHINSTRAP_OUTPUT_DIR` | Output root |
| `CHINSTRAP_H264_ENCODER` | Preferred encoder: `vah264enc`, `x264enc` or `openh264enc` |
| `CHINSTRAP_GPU_CONVERT` | Preferred GPU conversion path for DMA-BUF video: `va` or `gl` |
| `CHINSTRAP_SCREEN_FPS` | Screen, window and test-pattern frame rate (default 30, max 60) |
| `CHINSTRAP_SCREEN_BITRATE_KBPS`, `CHINSTRAP_CAMERA_BITRATE_KBPS` | Fixed bitrates |
| `CHINSTRAP_FAKE_ENGINE=1` | UI-only run with a simulated engine |

## Self-test

```sh
nix develop -c cargo run -- --selftest /tmp/chinstrap-selftest
CHINSTRAP_TEST_SIZE=3840x2160 nix develop -c cargo run -- --selftest /tmp/st-4k
CHINSTRAP_SELFTEST_CAMERA=ILCE nix develop -c cargo run -- --selftest /tmp/st-cam
```

The self-test runs the real engine without the portal: a test pattern, the first usable camera and the default microphone. It records two segments with a cut between them, then records a third and quits during it. It checks the following:

- Every file opens in GStreamer's Discoverer as H.264 or Opus.
- The test file keeps the size of its first frame, even after the selftest renegotiates it to a different size.
- File durations are within 0.6 s of the segment's duration.
- The microphone and test pattern start within 100 ms of each other.
- The gap between segments is under 1.5 s.
- The manifest matches the files on disk.
- Files are finalized on quit.

Two variables change the test:

- `CHINSTRAP_TEST_SIZE=WxH` sets the test pattern's size.
- `CHINSTRAP_SELFTEST_CAMERA=<name substring>` picks a specific camera. Its file must then be H.264 at the camera's native size and rate. The test also checks that PipeWire links that camera's node rather than the default source.

The self-test also checks PipeWire targeting whenever PipeWire is reachable:

- Targeting a node that does not exist must fail with "target not found". Any other error, such as the busy default camera's EBUSY after a fallback, fails the self-test.
- A temporary test `Video/Source` node is created, and its object id must be the only node linked. This needs no camera.
- With `CHINSTRAP_SELFTEST_CAMERA`, the named camera's object id must be the only node linked.

Checks that cannot run are reported as `skipped: …` in the JSON `targeting` object and in the warnings, never as passes.

`cargo test` runs the unit tests: camera mode choice, including the exact caps of a Sony ILCE-7CM2 and a typical MJPEG webcam, size lock, bitrate, per-input encoder choice, portal targeting and timestamping. Two more tests need a live PipeWire session: `cargo test -- --ignored targeting`. They use a non-default camera when one is attached, else a temporary test node.

## What is verified, and what is not

These have been checked on one machine: NixOS, niri, PipeWire 1.6.9 and WirePlumber 0.5.18, AMD GPUs using `vah264enc`.

- The unit tests.
- All three self-tests above.
- The PipeWire targeting checks, against a Sony ILCE-7CM2 recording 3840×2160 at 25 fps, H.264 High at level 5.1.

These have not been checked:

- Screen and window capture through the portal in an automated test. It needs a person to pick a source.
- Other compositors and portal backends.
- NVIDIA and Intel hardware.
- MJPEG and DMA-BUF cameras at 4K, and camera frame rates above 30 fps.
- Long recordings.

The ILCE-7CM2 delivers truncated frames for a short time after it is reopened. Chinstrap drops frames that PipeWire marks as corrupted and fills the gap by repeating frames, so a camera file can start slightly late.

## License

Apache-2.0
