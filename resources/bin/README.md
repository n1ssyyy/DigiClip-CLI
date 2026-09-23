# Bundled media binaries

`digiclip_rs::binaries` resolves `ffmpeg`, `ffprobe` and `whisper-cli`
in this order:

1. Provisioned user-data dir (`bin/` under the OS data dir — first-run downloads)
2. `<exe-dir>/resources/bin/<platform>/` in this repo (ships with installers)
3. `<exe-dir>/bin/<platform>/` (dev layout)
4. `<cwd>/resources/bin/<platform>/` (`cargo run` from the repo root)
5. System `PATH`

Platform dirs:

| Dir | Status |
|-----|--------|
| `win-x64/` | `whisper-cli.exe` (CPU) checked in; `ffmpeg.exe`/`ffprobe.exe` + `whisper-cli-vulkan.exe` (GPU) built by CI |
| `linux-x64/` | empty — CI builds `whisper-cli` (+ Vulkan where `glslc` exists); `ffmpeg`/`ffprobe` come from the distro |
| `linux-arm64/` | empty — same as linux-x64 |
| `mac-arm64/` / `mac-x64/` | empty — `ffmpeg` via `brew install ffmpeg`; no Vulkan sidecar (toggle stays CPU with the reason) |

Notes:

- **ffmpeg is a system dependency on macOS/Linux** (`brew install ffmpeg`,
  `sudo apt install ffmpeg`). Only Windows auto-downloads a portable build
  (gyan.dev essentials) because there is no reliable static zip for
  Unix. Every ffmpeg must include **libass** or caption burn-in fails.
- **whisper-cli** — the embedded `whisper-rs` STT is the default path, so the
  sidecar is only needed for GPU (Vulkan) transcription. Build from
  https://github.com/ggerganov/whisper.cpp
  (`cmake -B build && cmake --build build --config Release`), or pin the
  same commit CI uses (see `.github/workflows/ci.yml`).
- Whisper **models** (`ggml-*.bin`, 75 MB–3 GB) are never committed — they
  download themselves on first transcribe into the OS data dir.
