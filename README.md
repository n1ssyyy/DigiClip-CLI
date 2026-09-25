<p align="center">
  <h1 align="center">DigiClip CLI</h1>
</p>

<p align="center">
  <strong>Drop a video, get TikTok-ready clips — from the terminal.</strong><br />
  The clipping engine behind <a href="https://github.com/n1ssyyy/DigiClip">DigiClip</a>:
  offline transcription, smart clip picking, and captioned 9:16 renders.
  One portable exe, no cloud render farm required.
</p>

<p align="center">
  <a href="https://github.com/n1ssyyy/DigiClip-CLI/actions/workflows/ci.yml">
    <img src="https://shieldcn.dev/github/n1ssyyy/DigiClip-CLI/ci.svg?variant=default&size=default" alt="CI" />
  </a>
  <a href="https://github.com/n1ssyyy/DigiClip-CLI/releases/latest">
    <img src="https://shieldcn.dev/github/n1ssyyy/DigiClip-CLI/release.svg?variant=default&size=default" alt="Latest Release" />
  </a>
  <a href="https://github.com/n1ssyyy/DigiClip-CLI/blob/main/LICENSE">
    <img src="https://shieldcn.dev/github/n1ssyyy/DigiClip-CLI/license.svg?variant=default&size=default" alt="MIT License" />
  </a>
</p>

<p align="center">
  <img src="https://shieldcn.dev/badge/Rust-Stable-CE422B.svg?logo=rust&variant=default&size=default" alt="Rust" />
  <img src="https://shieldcn.dev/badge/whisper.cpp-STT-000000.svg?logo=huggingface&variant=default&size=default" alt="whisper.cpp" />
  <img src="https://shieldcn.dev/badge/OpenRouter-LLM-000000.svg?logo=openrouter&variant=default&size=default" alt="OpenRouter" />
  <img src="https://shieldcn.dev/badge/ffmpeg-Render-00A8E8.svg?logo=ffmpeg&variant=default&size=default" alt="ffmpeg" />
  <img src="https://shieldcn.dev/badge/ONNX-YuNet-005CED.svg?variant=default&size=default" alt="ONNX YuNet" />
</p>

<p align="center">
  <a href="https://github.com/n1ssyyy"><img src="https://shieldcn.dev/badge/Author-n1ssyyy-181717.svg?logo=github&variant=default&size=default" alt="n1ssyyy" /></a>
  <a href="https://github.com/n1ssyyy/DigiClip"><img src="https://shieldcn.dev/badge/Desktop_App-DigiClip-1e40af.svg?logo=github&variant=default&size=default" alt="DigiClip desktop app" /></a>
</p>

---

## ✨ Features

- **One input, two outputs** — `clips` mode cuts captioned 9:16 highlights (default 3), `full` mode keeps the whole video with burned-in subtitles.
- **Offline transcription** — whisper.cpp statically linked via whisper-rs (no sidecar to ship), word-level timestamps with token timing + even-split fallback. Vulkan sidecar optional for GPU STT.
- **Smart clip picking** — bring-your-own-key [OpenRouter](https://openrouter.ai) LLM (`submit_clips` tool call, content + markdown fallbacks, 3 retries), with an offline heuristic fallback when no key is set.
- **Speaker autofocus** — YuNet face tracker (MIT, 230KB, auto-downloaded) follows whoever is talking: mouth-motion speaker signal, 2.5s anti-ping-pong lock, group two-shot framing, shot-cut snaps, handheld-pan glides.
- **Vertical renders** — 1080×1920 H.264 + faststart, loudness-normalized mobile audio (`loudnorm`), encoder auto-pick (NVENC / VideoToolbox / libx264).
- **8 caption presets** — `tiktok`, `karaoke` (default), `hormozi`, `minimal`, `beast`, `neon`, `highlight`, `ghost`, burned in via libass (plus `.srt` sidecars).
- **Tightening + punch-ins** — `light` shrinks long pauses, `punchy` also cuts filler words (audited in `cut_plan.json`); loud words earn a brief eased zoom.
- **Merge compilations** — bare `--merge` compiles the picks chronologically (repetitions dropped, overlaps fused), or join explicit `A-B,C-D` ranges with hard cuts or white-flash joins.
- **Upload kits** — every clip gets a `clip-XX-upload.txt` (title, description, hashtags).
- **`--serve` daemon** — the desktop app spawns `digiclip --serve` and talks to it over a token-gated localhost WebSocket (`/ws` + `/art` + `/src`); same pipeline, zero drift from CLI semantics.
- **Private by default** — video, transcripts and renders stay on your disk; only clip *scoring* (and optional vision punch-ins) call OpenRouter.

## 🧭 How it works

```mermaid
flowchart LR
    Upload(["Drop video"]) --> Extract["Extract audio\nffmpeg · 16 kHz mono"]
    Extract --> Transcribe["Transcribe\nwhisper embedded · Vulkan sidecar optional"]
    Transcribe --> Analyze["Pick clips\nLLM or heuristic"]
    Analyze --> Track["Track speaker\nYuNet · smart framing only"]
    Track --> Render["Render vertical\nffmpeg · NVENC/VideoToolbox/x264"]
    Render --> Done(["Watch and download"])
```

1. **Ingest** probes the source (`ffprobe`, `ffmpeg -i` fallback) and extracts 16 kHz mono WAV + a poster frame.
2. **Transcribe** runs embedded whisper-rs (or the Vulkan sidecar in GPU mode) and stores words/segments.
3. **Pick** asks OpenRouter for ranked candidates (15–90 s, default 3 clips) or falls back to the heuristic scorer; the validator clamps ranges, dedupes overlaps/near-dupe hooks, and snaps boundaries to finished sentences.
4. **Track** (smart framing only) samples picked ranges at 8–15 fps, confirms faces temporally, scores the speaker by mouth motion, and emits a 30 Hz bezier camera path (single glide per handoff, snaps on hard cuts).
5. **Render** cuts each candidate to captioned 1080×1920 MP4 in one continuous pass (no chunk files, no drift) with per-stage timings.

## 🛠 Tech stack

| Layer    | Choice |
|----------|--------|
| CLI      | Rust (clap, tokio, anyhow, tracing) |
| STT      | whisper.cpp via whisper-rs (embedded) + optional Vulkan `whisper-cli` sidecar |
| Clip AI  | OpenRouter (BYOK, default `nvidia/nemotron-3-ultra-550b-a55b:free`) with offline heuristic fallback |
| Tracking | ONNX Runtime (YuNet) — DirectML EP on Windows, CPU elsewhere |
| Render   | ffmpeg + libass captions, loudnorm audio |
| Serve    | axum WebSocket daemon (`--serve`) for the desktop UI |
| Data     | OS user-data dir, resumable downloads, no database |

## 🚀 Getting started

### Prerequisites

**Users:** just the exe (first run needs network once to provision).

- **Windows** — needs the [Microsoft Visual C++ Redistributable (x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe)
  (whisper.cpp / ONNX Runtime link the DLL runtime). Most PCs already have
  it; the DigiClip desktop app ships these DLLs itself.
- **Linux** — glibc 2.38+ (Ubuntu 24.04+, Debian 13+, Fedora 39+): ONNX
  Runtime's prebuilt libraries set that floor.
- **macOS** — Apple Silicon.

| OS | ffmpeg | Notes |
|----|--------|-------|
| Windows | auto-downloaded (~80MB gyan build) or `winget install Gyan.FFmpeg` | zero setup either way |
| macOS | `brew install ffmpeg` | must include libass; VideoToolbox used for GPU renders. Homebrew/MacPorts bins are found even when launched from Finder (GUI apps don't get the shell PATH) |
| Linux | `sudo apt install ffmpeg` (or `dnf`) | must include libass; NVENC used when an NVIDIA GPU is present |

**Building:**

- Rust stable, plus C++ build tools for whisper-rs:
  - Windows: VS Build Tools (C++), CMake, LLVM (`winget install LLVM.LLVM`, set `LIBCLANG_PATH=C:\Program Files\LLVM\bin`)
  - Linux: `build-essential cmake clang libclang-dev`
  - macOS: Xcode CLT (`xcode-select --install`), `brew install cmake llvm`
- GPU STT (optional): build `whisper-cli` with `-DGGML_VULKAN=1` once — see below.

```powershell
cargo build --release
.\target\release\digiclip.exe input.mp4
```

```bash
cargo build --release
./target/release/digiclip input.mp4
```

First run provisions `%APPDATA%/digiclip` (Windows), `~/.local/share/digiclip`
(Linux) or `~/Library/Application Support/digiclip` (macOS):

| What | Source | Size | When |
|---|---|---|---|
| `bin/ffmpeg` + `ffprobe` | gyan.dev (Windows only) | ~80MB | only if none on PATH |
| `yunet_2026may.onnx` | opencv_zoo (MIT) | 230KB | only for `--framing smart` |
| `models/ggml-*.bin` | HuggingFace whisper.cpp | 75MB–3GB | on first transcribe |
| `fonts/*.ttf` | embedded in the exe | 1MB | always (for libass) |

Prefetch everything up front (or warm a machine before going offline):

```bash
digiclip --provision [--model base.en]
```

Every download resumes (`.part` + Range) with progress bars and is skipped
when a good copy already exists — system ffmpeg installs are respected, never
re-downloaded. `DIGICLIP_FFMPEG=/path/to/ffmpeg` overrides resolution.

### Usage

```bash
digiclip input.mp4                                # 3 smart clips, karaoke captions
digiclip input.mp4 --mode full                    # whole video, subtitled
digiclip input.mp4 --framing smart                # face-tracked 9:16 crop
digiclip input.mp4 --framing plan --crop-plan plan.json
digiclip input.mp4 --count 5 --style hormozi --model large-v3-turbo-q5_0
digiclip input.mp4 --count 0                      # auto: keep whatever clears the merit bar
digiclip input.mp4 --min-len 20 --max-len 45      # duration window (default: 15–90)
digiclip input.mp4 --min-len 30 --max-len 30      # exact 30s renders (window widens past cuts, tail trims)
digiclip input.mp4 --kind complete                # finished thoughts, flexible length
digiclip input.mp4 --kind moments --seed 42       # seeded random windows
digiclip input.mp4 --kind timecut --span 0-300 --timecut-len 15 --take 5
digiclip input.mp4 --tighten punchy              # also cut filler words (default: light = pauses only)
digiclip input.mp4 --merge                             # compile the picks into one clip
digiclip input.mp4 --merge "23.8-38.8,87.1-102.1"  # join explicit ranges (overlaps fuse)
digiclip input.mp4 --merge --merge-flash            # white-flash joins instead of hard cuts
digiclip input.mp4 --out-dir out/                 # custom output dir
digiclip input.mp4 --dry-run                      # preflight only
digiclip --serve --port 4317                      # daemon for the desktop UI
```

Outputs next to `--out-dir` (`<input-stem>-digiclip/` by default):
`audio.wav`, `transcript.json/.srt`, clips mode → `clip-01-9x16.mp4` + `.ass/.srt` + `clips.json`,
full mode → `full-9x16.mp4` + `full.ass/.srt`.

## ⚙️ Configuration

| Key / flag | Default | What it does |
|-----|---------|--------------|
| `OPENROUTER_API_KEY` / `--openrouter-key` | — | BYOK key for LLM clip scoring. Unset → offline heuristic scorer. |
| `OPENROUTER_MODEL` / `--openrouter-model` | `nvidia/nemotron-3-ultra-550b-a55b:free` | Scoring model. |
| `OPENROUTER_TIMEOUT_S` | `300` | HTTP timeout for scoring calls. |
| `OPENROUTER_VISION_MODEL` / `--vision-model` | `google/gemini-2.5-flash` | Vision punch-in suggestions for long wides (needs a key). |
| `--model` | `base.en` | `tiny.en`, `base.en`, `large-v3-turbo(-q5_0)`, `large-v3`. |
| `--gpu` / `--no-gpu` | on | Master switch: Vulkan STT + DirectML tracking + NVENC/VideoToolbox vs all-CPU. |
| `DIGICLIP_FFMPEG` | — | Explicit ffmpeg path override. |
| `--framing` | `center` | `center` (static crop), `plan` (crop-plan JSON), `smart` (face tracker). |
| `--tighten` | `light` | `light` (pauses), `punchy` (+ filler words), `off`. |
| `--style` | `karaoke` | Any of the 8 caption presets. |

Copy `.env.example` to `.env` (or export) for local runs.

## 🎮 GPU policy: one switch, everything follows it

`--gpu` (default on) / `--no-gpu`. GPU mode uses every GPU path available
and degrades loudly but never fatally; `--no-gpu` forces all-CPU:

| Step | GPU on (RTX 4060, measured) | All CPU |
|---|---|---|
| Transcribe | whisper.cpp Vulkan sidecar, 631s audio in **36s** | embedded whisper-rs |
| Track faces | YuNet via DirectML EP, 15fps (Windows) | YuNet CPU ort, 8fps |
| Render | NVENC / VideoToolbox | libx264 |

GPU STT needs a Vulkan `whisper-cli` on PATH or in `resources/bin/<platform>/`.
No official Windows binaries exist to download, so build it once (Vulkan SDK +
the whisper.cpp tree, ~5min — first process start compiles shaders once, ~20s):

```powershell
cmake -S whisper.cpp -B whisper.cpp/build-vulkan -DBUILD_SHARED_LIBS=OFF -DGGML_VULKAN=1
cmake --build whisper.cpp/build-vulkan --config Release --target whisper-cli
```

Missing sidecar in GPU mode warns and uses embedded CPU STT. On macOS/Linux
there is no Vulkan sidecar or DirectML EP — GPU mode means VideoToolbox/NVENC
renders with CPU tracking, which is still much faster than full CPU.

## 📦 Media binaries & models

`digiclip_rs::binaries` resolves each binary in order (see `resources/bin/README.md`):

1. Provisioned user-data `bin/` (first-run downloads)
2. `<exe-dir>/resources/bin/<platform>/` (ships with installers)
3. System `PATH`

Caption fonts (Archivo Black, Anton, Inter, JetBrains Mono) are embedded in
the exe and written out for libass on first run. Whisper models download
themselves on first transcribe (`--provision` to prefetch).

## 🧪 Tests & code style

```bash
cargo test   # 17 integration + 39 unit tests: Win path escaping, ASS/SRT,
             # whisper flags (-ng/-dev, never -ngl), Vulkan parse, crop plan, YuNet decode math
cargo run --example render_smoke  # real ffmpeg proof: 1080x1920 out
cargo run --example track_smoke   # YuNet session + inference path proof
cargo run --example stt_check     # embedded STT vs whisper-cli ground truth
```

## 🤖 CI / CD

`.github/workflows/ci.yml` runs on pushes to `main`, PRs and tags:

| Job | Runner | Does |
|-----|--------|------|
| `test` | `ubuntu-24.04` | `cargo test` (all targets) + fmt check |
| `build` | `windows-latest`, `ubuntu-24.04`, `macos-latest` | `cargo build --release` → `--help`/`--version` smoke → serve-watchdog test (`.github/scripts/watchdog-test.*`) → `digiclip(.exe)` artifact per OS |
| `release` | `ubuntu-24.04` | on `v*` tags only: attaches all three binaries to the GitHub Release |

Cut a release:

```bash
git tag v2.0.0 && git push origin v2.0.0
```

## 🗺 Project map

```
src/main.rs             CLI entry (parse args, serve vs pipeline runtime)
src/cli.rs              every flag (mode/kind/count/tighten/merge/framing/gpu/serve)
src/pipeline.rs         provision -> ingest -> transcribe -> pick -> track -> render
src/serve.rs            localhost daemon for the desktop UI (/ws + /art + /src)
src/stt.rs              embedded whisper-rs transcription (token timestamps)
src/whisper.rs          Vulkan sidecar command builder + JSON parser
src/binaries.rs         bundled-or-PATH ffmpeg/ffprobe/whisper-cli resolution
src/models.rs           ggml weight manager (resume downloads, first-boot seeding)
src/provision.rs        first-run provisioning (ffmpeg/Windows, YuNet, fonts)
src/ffmpeg.rs           extract WAV / poster / probe (ffprobe + stderr fallback)
src/gpu.rs              GPU inventory (nvidia-smi + WMI/lspci) + master switch
src/track.rs            YuNet speaker tracker + bezier camera path
src/framing.rs          crop plans + window geometry
src/render.rs           one-pass 9:16 renders (NVENC/VideoToolbox/x264 + libass)
src/captions/           AssBuilder (8 presets) + SrtBuilder
src/scorer.rs           offline heuristic clip scorer
src/openrouter.rs       LLM scoring client (tool call + fallbacks + retries)
src/validator.rs        range clamping + dedupe + completeness/hook gates
src/timeline.rs         tighten plans + merge joins
src/punch.rs            emphasis punch-in windows
src/vision.rs           VLM punch-in suggestions for long wides
src/kit.rs              upload-kit text files
src/progress.rs         progress/cancel plumbing shared by CLI + serve
```

## 🩺 Troubleshooting

| Symptom | Fix |
|---------|-----|
| `ffmpeg not found` | Windows: let it auto-download, or `winget install Gyan.FFmpeg`. macOS/Linux: install via package manager (see Prerequisites). |
| `whisper-cli` / Vulkan missing | Expected without a GPU sidecar — embedded CPU STT runs instead. Build the Vulkan sidecar for GPU STT. |
| Caption burn-in fails | ffmpeg must include libass (`ffmpeg -hide_banner -buildconf`). Distro `ffmpeg` sometimes splits it into `ffmpeg-libs` extras. |
| No faces in smart mode | Tracker degrades to center crop with a warning — check the YuNet model downloaded (`--provision`). |
| Slow tracking in debug | Debug builds are ~10× slower at the pixel loop — always measure with `--release`. |
| LLM scoring fails | Check `OPENROUTER_API_KEY`; any failure falls back to the heuristic scorer (auth errors fail loudly). |

## 🗺 Roadmap

- [ ] Prebuilt `whisper-cli-vulkan` per platform in CI (today: Windows CPU in git, Vulkan built locally).
- [ ] Signed releases + checksums.
- [ ] Chunked/resumable model downloads with mirror fallback.
- [ ] Two-person / letterbox framing presets.

## 🤝 Contributing

PRs welcome: fork, branch, `cargo test` green, open a PR against `main`. CI must stay green on Windows, Linux and macOS.

## 📄 License

MIT — see `LICENSE`. Video you process stays yours and stays local.

## 🙏 Acknowledgements

Developed by [n1ssyyy](https://github.com/n1ssyyy) in collaboration with
[Shkolla Digjitale](https://shkolladigjitale.com/) (Prizren).

Built on whisper.cpp · ffmpeg · ONNX Runtime · OpenRouter · axum.
 Powers the [DigiClip](https://github.com/n1ssyyy/DigiClip) desktop app.
