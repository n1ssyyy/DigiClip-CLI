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

The clipping engine behind the [DigiClip](https://github.com/n1ssyyy/DigiClip) desktop app, as one portable binary. Point it at a video and it transcribes, picks the best moments and renders captioned vertical clips.

## ✨ What it does

- **Clips or full video** — `clips` cuts captioned 9:16 highlights (3 by default); `full` subtitles the whole video.
- **Offline transcription** — whisper.cpp built in, word-level timing, optional Vulkan GPU sidecar.
- **Smart picks** — an [OpenRouter](https://openrouter.ai) model scores moments (bring your own key), with an offline heuristic fallback.
- **Speaker autofocus** — face tracking (YuNet) keeps whoever is talking in frame.
- **Polished output** — 1080×1920 H.264, 8 caption styles, loudness-normalized audio, pause/filler tightening, punch-in zooms, merge compilations, upload kits. Clips render in parallel.
- **Private** — everything stays on your disk; only clip scoring optionally calls OpenRouter.

```mermaid
flowchart LR
    V(["Video"]) --> A["Extract audio"] --> T["Transcribe"] --> P["Pick clips"] --> F["Track speaker"] --> R["Render 9:16"] --> O(["Clips"])
```

## 📥 Install

Download the binary for your system from the [latest release](https://github.com/n1ssyyy/DigiClip-CLI/releases/latest): `digiclip-win-x64.exe`, `digiclip-linux-x64` or `digiclip-macos-arm64`.

- **ffmpeg with libass** — Windows downloads it automatically on first run; macOS `brew install ffmpeg`; Linux `sudo apt install ffmpeg`.
- **Windows** needs the [Visual C++ Redistributable (x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe) (most PCs have it).
- **Linux** needs glibc 2.38+ (Ubuntu 24.04+, Debian 13+, Fedora 39+). **macOS** is Apple Silicon.

The first run downloads what it needs (whisper model, face model, ffmpeg on Windows) into your user-data folder; after that it works offline. `digiclip --provision` fetches it all up front.

## 🚀 Usage

```bash
digiclip input.mp4                                  # 3 clips, karaoke captions
digiclip input.mp4 --mode full                      # whole video, subtitled
digiclip input.mp4 --framing smart                  # follow the speaker
digiclip input.mp4 --count 5 --style hormozi        # more clips, another caption style
digiclip input.mp4 --min-len 20 --max-len 45        # clip length window (default 15–90s)
digiclip input.mp4 --tighten punchy                 # also cut filler words
digiclip input.mp4 --merge                          # compile the picks into one clip
digiclip input.mp4 --out-dir out/ --dry-run         # plan only, no render
```

Output lands in `<input-name>-digiclip/`: `clip-01-9x16.mp4` (+ `.ass`/`.srt` captions and an upload kit per clip), `transcript.json/.srt`, `clips.json`. Run `digiclip --help` for every option.

## ⚙️ Configuration

| Variable / flag | Default | What it does |
|---|---|---|
| `OPENROUTER_API_KEY` / `--openrouter-key` | — | Key for LLM clip scoring; without one the offline scorer is used. |
| `OPENROUTER_MODEL` / `--openrouter-model` | `nvidia/nemotron-3-ultra-550b-a55b:free` | Scoring model. |
| `--model` | `base.en` | Whisper model: `tiny.en`, `base.en`, `large-v3-turbo(-q5_0)`, `large-v3`. |
| `--gpu` | `true` | Use GPU paths (NVENC / VideoToolbox renders, DirectML tracking, Vulkan STT) where available. |
| `--style` | `karaoke` | `tiktok`, `karaoke`, `hormozi`, `minimal`, `beast`, `neon`, `highlight`, `ghost`. |
| `DIGICLIP_FFMPEG` | — | Use a specific ffmpeg. |
| `DIGICLIP_RENDER_JOBS` | auto | How many clips render at once. |

A `.env` file next to where you run it works too (see `.env.example`).

## 🛠 Building

Rust stable plus C++ tools for whisper.cpp:

- **Windows** — VS Build Tools (C++), CMake, LLVM (`LIBCLANG_PATH=C:\Program Files\LLVM\bin`)
- **Linux** — `build-essential cmake clang libclang-dev`
- **macOS** — Xcode CLT, `brew install cmake llvm`

```bash
cargo build --release     # target/release/digiclip
cargo test                # unit + integration tests
```

CI tests and builds on Windows, Linux and macOS; tagging `v*` publishes the three binaries.

## 🩺 Troubleshooting

| Problem | Fix |
|---|---|
| `ffmpeg not found` | See Install — or set `DIGICLIP_FFMPEG`. |
| Captions don't burn in | Your ffmpeg lacks libass (`ffmpeg -hide_banner -buildconf \| grep libass`). |
| No faces tracked | It falls back to a center crop — run `digiclip --provision` to fetch the face model. |
| LLM scoring fails | Check the key; any failure falls back to the offline scorer. |

## 📄 License

MIT — see `LICENSE`. Video you process stays yours and stays local.

## 🙏 Acknowledgements

Developed by [n1ssyyy](https://github.com/n1ssyyy) in collaboration with
[Shkolla Digjitale](https://shkolladigjitale.com/) (Prizren).

Built on whisper.cpp · ffmpeg · ONNX Runtime · OpenRouter · axum.
Powers the [DigiClip](https://github.com/n1ssyyy/DigiClip) desktop app.
