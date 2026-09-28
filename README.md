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

<h3 align="center">One binary. Any long video. Clips worth posting.</h3>

<p align="center">
  The engine behind <a href="https://github.com/n1ssyyy/DigiClip">DigiClip</a>, unleashed in your terminal:<br />
  it listens, finds the moments, frames the speaker and renders captioned vertical clips — offline.
</p>

```bash
digiclip podcast.mp4
# → 3 captioned 9:16 clips, transcript, subtitles and upload kits. That's it.
```

## ⚡ Why it rips

- 🎙️ **Transcribes offline.** whisper.cpp is compiled right in — word-level timing, no Python, no upload. Got a GPU? Plug in the Vulkan sidecar.
- 🎯 **Picks like an editor.** An [OpenRouter](https://openrouter.ai) model of your choice ranks every moment for hook and payoff; a built-in scorer covers you offline.
- 🎥 **Frames like a camera operator.** YuNet face tracking plus an offline camera planner: the shot locks off while the speaker stays put, follows a walking presenter in one smooth move, cuts (never whip-pans) between speakers, and lands every reframe on the source's own shot cuts.
- ✂️ **Tightens the edit.** Dead air goes, filler words optionally too; every cut is placed in the quietest instant near the word edge, frame-aligned, with click-free audio joins. Loud lines get a punch-in zoom anchored on the face — every cut logged in `cut_plan.json`.
- 💬 **Eight caption styles.** Karaoke, Hormozi, neon, beast and friends, burned in with libass.
- 🚀 **Renders in parallel, in sync.** A streaming decode → compose → encode pipeline, clips side by side on NVENC or VideoToolbox (libx264 fallback). A/V sync is exact by construction and loudness lands on −14 LUFS with a −1 dBTP ceiling.
- 🔒 **Stays local.** Your video never leaves the disk; only clip scoring optionally calls OpenRouter.

```mermaid
flowchart LR
    V(["Video"]) --> A["Extract audio"] --> T["Transcribe"] --> P["Pick clips"] --> F["Track speaker"] --> R["Render 9:16"] --> O(["Clips"])
```

### How the engine works

- **Cuts.** Clip and keep edges snap to word boundaries, then into the quietest nearby instant (a short lead-in before the first word, a natural tail after the last), then onto the output frame grid. Audio is cut by exact sample counts on the same grid, with short fades at every join, so video and audio can't drift apart however many jump cuts a clip has.
- **Tracking.** Faces are sampled at 8–15 Hz. Shot cuts are verified (a one-frame spike, not a pan) and pinned to the exact frame. A speaker who walks or drops out of detection for a moment stays the same person, and the camera switches only between different people who are talking.
- **Camera.** The whole clip is planned before the first frame renders. The camera holds inside a dead zone, cuts on shot changes, speaker switches and layout changes, glides (smootherstep) for everything else, and follows continuous motion with zero-phase smoothing. Zoom never goes past the source resolution, so low-res input stays sharp.
- **Render.** One ffmpeg decode per keep feeds a Rust compositor (crop, scale, blur fill), which feeds one encoder with captions burned in. The audio uses two-pass loudness and a limiter.

## 📥 Get it

Download your binary from the [latest release](https://github.com/n1ssyyy/DigiClip-CLI/releases/latest) — `digiclip-win-x64.exe`, `digiclip-linux-x64` or `digiclip-macos-arm64` — and run it.

- 🎞️ **ffmpeg with libass** — Windows downloads it on first run; macOS `brew install ffmpeg`; Linux `sudo apt install ffmpeg`.
- 🪟 Windows needs the [Visual C++ Redistributable (x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe) (most PCs already have it).
- 🐧 Linux needs glibc 2.38+ (Ubuntu 24.04+, Debian 13+, Fedora 39+). 🍎 macOS: Apple Silicon.

The first run grabs what it needs — whisper model, face model, ffmpeg on Windows — then it works offline for good. `digiclip --provision` fetches it all up front.

## 🚀 Use it

```bash
digiclip input.mp4                              # 3 clips, karaoke captions
digiclip input.mp4 --mode full                  # the whole video, subtitled
digiclip input.mp4 --framing smart              # follow the speaker
digiclip input.mp4 --count 5 --style hormozi    # more clips, louder captions
digiclip input.mp4 --min-len 20 --max-len 45    # clip length window (default 15–90s)
digiclip input.mp4 --tighten punchy             # also cut filler words
digiclip input.mp4 --merge                      # stitch the picks into one supercut
digiclip input.mp4 --dry-run                    # plan it, don't render
```

Everything lands in `<input-name>-digiclip/`: `clip-01-9x16.mp4` with its `.ass`/`.srt` captions and upload kit, plus `transcript.json/.srt` and `clips.json`. `digiclip --help` lists every knob.

## ⚙️ Tune it

| Variable / flag | Default | What it does |
|---|---|---|
| `OPENROUTER_API_KEY` / `--openrouter-key` | — | Unlocks LLM clip picking (without it, the offline scorer runs). |
| `OPENROUTER_MODEL` / `--openrouter-model` | `nvidia/nemotron-3-ultra-550b-a55b:free` | Scoring model. |
| `--model` | `base.en` | Whisper model: `tiny.en`, `base.en`, `large-v3-turbo(-q5_0)`, `large-v3`. |
| `--gpu` | `true` | GPU everything: NVENC / VideoToolbox renders, DirectML tracking, Vulkan STT. |
| `--style` | `karaoke` | `tiktok` · `karaoke` · `hormozi` · `minimal` · `beast` · `neon` · `highlight` · `ghost` |
| `DIGICLIP_RENDER_JOBS` | auto | How many clips render at once. |
| `DIGICLIP_FFMPEG` | — | Use a specific ffmpeg. |

A `.env` file works too — see `.env.example`.

## 🛠 Build it

Rust stable plus C++ tools for whisper.cpp:

- 🪟 **Windows** — VS Build Tools (C++), CMake, LLVM (`LIBCLANG_PATH=C:\Program Files\LLVM\bin`)
- 🐧 **Linux** — `build-essential cmake clang libclang-dev`
- 🍎 **macOS** — Xcode CLT, `brew install cmake llvm`

```bash
cargo build --release   # → target/release/digiclip
cargo test              # unit + integration tests
```

CI builds and tests on Windows, Linux and macOS; tag `v*` and the three binaries publish themselves.

`cargo run --release --example render_smoke` renders synthetic flash/click clips through the real engine and measures A/V sync across off-grid jump cuts. `DIGICLIP_CAMERA_DUMP=<dir>` writes each clip's framing targets, camera path and shot cuts as CSV for inspection.

## 🩺 Something off?

| Problem | Fix |
|---|---|
| `ffmpeg not found` | See **Get it** — or point `DIGICLIP_FFMPEG` at one. |
| Captions don't burn in | Your ffmpeg lacks libass: `ffmpeg -hide_banner -buildconf \| grep libass`. |
| Faces not tracked | It falls back to a center crop — `digiclip --provision` fetches the face model. |
| LLM scoring fails | Check the key; any failure drops to the offline scorer. |

## 📄 License

MIT — see `LICENSE`. Your footage stays yours and stays local.

## 🙏 Acknowledgements

Developed by [n1ssyyy](https://github.com/n1ssyyy) in collaboration with
[Shkolla Digjitale](https://shkolladigjitale.com/) (Prizren).

Powered by whisper.cpp · ffmpeg · ONNX Runtime · OpenRouter · axum.
Drives the [DigiClip](https://github.com/n1ssyyy/DigiClip) desktop app.
