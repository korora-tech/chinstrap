# Chinstrap

A native recording studio for Linux, built with GPUI and GStreamer. Chinstrap records a camera, a microphone and any number of screens and windows at the same time. Each input goes to its own file, and all of them run on one shared clock so they can be lined up in an editor. Recordings can be cut into segments without stopping.

The window opens on the **Studio** view, which is the recorder. Each finished segment's microphone file can be transcribed on the device with Whisper (see [Transcription](#transcription)). The **Feature guide** tab (`ctrl-2`) is the earlier reference UI for planned features. Nothing on that tab captures, transcribes or automates anything.

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
      transcript.json              transcript of mic.mkv: segments, word timings, model, timing
      mic.srt / mic.vtt            subtitles of mic.mkv
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

## Transcription

Chinstrap transcribes the microphone file of each finished segment on this machine with [whisper.cpp](https://github.com/ggml-org/whisper.cpp) (through `whisper-rs`), using the Vulkan GPU backend and falling back to the CPU. Nothing is sent anywhere.

**Model.** The default is Whisper large-v3-turbo quantized to q8_0 (`ggml-large-v3-turbo-q8_0.bin`, 834.5 MiB). On this project's tests it stayed within one word error per clip of the f16 file (1.5 GiB) and loaded faster; q5_0 (547 MiB) made more mistakes on technical terms. Silero VAD v6.2.0 (864 KiB) is downloaded with it and stops Whisper from inventing text ("Thank you.") on silence and room noise.

**Download.** The model is never downloaded silently. Click **Download model (834.5 MiB)** in the TRANSCRIPTION panel, or run:

```sh
nix develop -c cargo run --release -- --download-model          # default model + VAD
nix develop -c cargo run --release -- --download-model large-v3-turbo
```

Files go to `$XDG_DATA_HOME/chinstrap/models` (default `~/.local/share/chinstrap/models`). Each file comes from a commit-pinned Hugging Face URL and is checked against a pinned SHA-256 before it is used. The download goes to a `.part` file, can be cancelled, and resumes where it stopped.

**When.** With **Transcribe segments automatically** on (the default), each segment is queued when it finishes. Each segment row also has **Transcribe**, **Cancel** and **Retry**. Jobs run one at a time in a separate low-priority process. GPU jobs keep running while you record. CPU jobs pause while recording and continue afterwards. If a job crashes on Vulkan, it is retried on the CPU; if that happens twice and both retries succeed, Chinstrap switches to the CPU for the rest of the run.

**Vocabulary.** Put names and jargon in `$XDG_CONFIG_HOME/chinstrap/glossary.txt` (default `~/.config/chinstrap/glossary.txt`), one term per line; `#` starts a comment. The terms, plus the session folder's name when it is not the default timestamp, are given to Whisper as its initial prompt (at most 1 KiB and 200 tokens; terms past the limit are dropped). Whisper sees the prompt directly only in the first 30-second window of each file; later windows are conditioned on the text already transcribed, which carries the spellings forward.

**Outputs.** Next to `mic.mkv`: `transcript.json`, `mic.srt` and `mic.vtt`. Times are seconds on the file's own timeline, so `0.000` is the start of `mic.mkv`. `transcript.json` has every segment with word-level start and end times (DTW-aligned), the detected language, the model and its hash, the backend and device, the speech spans from the VAD, and the decode, VAD and inference times. `session.json` records each transcript's status (`queued`, `running`, `done`, `failed`, `cancelled` or `interrupted`) and file names under the mic input.

**Headless use.**

```sh
chinstrap --transcribe recording.mkv [--model <name|path>] [--lang <code|auto>] [--cpu]
```

This writes `recording.transcript.json`, `recording.srt` and `recording.vtt` next to the file (`transcript.json` for a file named `mic.*`) and prints a one-line JSON summary. Exit codes: 0 done, 1 failed, 2 usage, 3 model not downloaded (or it failed its checksum and was moved aside to `<file>.bad`).

**Requirements.** The Nix build targets x86-64-v3 (AVX2, FMA, F16C, BMI2) for ggml's CPU code. On an older CPU, transcription fails with `cpu-unsupported`. aarch64 builds are not tested.

## Configuration

| Variable | Meaning |
|---|---|
| `CHINSTRAP_OUTPUT_DIR` | Output root |
| `CHINSTRAP_H264_ENCODER` | Preferred encoder: `vah264enc`, `x264enc` or `openh264enc` |
| `CHINSTRAP_GPU_CONVERT` | Preferred GPU conversion path for DMA-BUF video: `va` or `gl` |
| `CHINSTRAP_SCREEN_FPS` | Screen, window and test-pattern frame rate (default 30, max 60) |
| `CHINSTRAP_SCREEN_BITRATE_KBPS`, `CHINSTRAP_CAMERA_BITRATE_KBPS` | Fixed bitrates |
| `CHINSTRAP_FAKE_ENGINE=1` | UI-only run with a simulated engine |
| `CHINSTRAP_WHISPER_MODEL` | Model name (`large-v3-turbo-q8_0`, `large-v3-turbo-q5_0`, `large-v3-turbo`) or a path to any ggml Whisper file (not hash-checked) |
| `CHINSTRAP_WHISPER_LANG` | Language code, or `auto` (default) |
| `CHINSTRAP_WHISPER_BACKEND=cpu` | Never use the GPU |
| `CHINSTRAP_WHISPER_GPU_DEVICE` | Vulkan device index (default 0) |
| `CHINSTRAP_WHISPER_DEBUG=1` | Show whisper.cpp and ggml logs |

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

When the Whisper model is already downloaded, the self-test also transcribes segment 1's microphone file through the worker process and checks `transcript.json`, the SRT and VTT files, and the manifest status. A quiet room should give 0 words; anything else is reported as a warning. Without the model this check is skipped with a warning, unless `CHINSTRAP_SELFTEST_REQUIRE_MODEL=1`. `CHINSTRAP_SELFTEST_SPEECH=<file.mkv>:<reference.txt>` also transcribes a known recording and fails if the word error rate is above 10%.

Two variables change the test:

- `CHINSTRAP_TEST_SIZE=WxH` sets the test pattern's size.
- `CHINSTRAP_SELFTEST_CAMERA=<name substring>` picks a specific camera. Its file must then be H.264 at the camera's native size and rate. The test also checks that PipeWire links that camera's node rather than the default source.

The self-test also checks PipeWire targeting whenever PipeWire is reachable:

- Targeting a node that does not exist must fail with "target not found". Any other error, such as the busy default camera's EBUSY after a fallback, fails the self-test.
- A temporary test `Video/Source` node is created, and its object id must be the only node linked. This needs no camera.
- With `CHINSTRAP_SELFTEST_CAMERA`, the named camera's object id must be the only node linked.

Checks that cannot run are reported as `skipped: …` in the JSON `targeting` object and in the warnings, never as passes.

`cargo test` runs the unit tests: camera mode choice, including the exact caps of a Sony ILCE-7CM2 and a typical MJPEG webcam, size lock, bitrate, per-input encoder choice, portal targeting and timestamping. For transcription it covers subtitle formatting, word timing, the VAD filter, the prompt size limits, the manifest, the worker's exit handling and orphan protection, the decoder's timeline, and the resumable download against a local HTTP server. Two more tests need a live PipeWire session: `cargo test -- --ignored targeting`. They use a non-default camera when one is attached, else a temporary test node.

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

## Third-party code and models

- [whisper.cpp and ggml](https://github.com/ggml-org/whisper.cpp) (MIT, Georgi Gerganov and contributors) are compiled into the binary through [whisper-rs](https://codeberg.org/tazz4843/whisper-rs) (Unlicense).
- The [Whisper](https://github.com/openai/whisper) large-v3-turbo weights are by OpenAI (MIT); the ggml conversions are downloaded from [ggerganov/whisper.cpp on Hugging Face](https://huggingface.co/ggerganov/whisper.cpp).
- [Silero VAD](https://github.com/snakers4/silero-vad) (MIT, Silero Team), in the ggml conversion published with whisper.cpp ([ggml-org/whisper-vad](https://huggingface.co/ggml-org/whisper-vad)).

The model files are not part of this repository or the Nix package; they are downloaded on request.

## License

Apache-2.0
