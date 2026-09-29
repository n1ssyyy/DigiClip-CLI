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
- 🎯 **Picks like an editor.** A model of your choice ([OpenRouter](https://openrouter.ai), OpenAI, Anthropic, Gemini, Ollama, Groq and more) ranks every moment for hook and payoff; a built-in scorer covers you offline. A System One model then re-judges each candidate with calibrated odds (hook, standalone, complete, value, shareability): TypeSafe's hosted **Jev**, or **Laya** running locally on your CPU.
- 🎥 **Frames like a camera operator.** YuNet face tracking plus an offline camera planner: the shot locks off while the speaker stays put, follows a walking presenter in one smooth move, cuts (never whip-pans) between speakers, and lands every reframe on the source's own shot cuts.
- ✂️ **Tightens the edit.** Dead air goes, filler words optionally too; every cut is placed in the quietest instant near the word edge, frame-aligned, with click-free audio joins. Loud lines get a punch-in zoom anchored on the face — every cut logged in `cut_plan.json`.
- 💬 **Eight caption styles.** Karaoke, Hormozi, neon, beast and friends, burned in with libass.
- 🧩 **Any platform, your brand.** 9:16, 4:5, 1:1 or 16:9 from the same source, with an optional headline, progress bar, corner logo and a music bed that ducks under the voice.
- 🚀 **Renders in parallel, in sync.** A streaming decode → compose → encode pipeline, clips side by side on NVENC or VideoToolbox (libx264 fallback). A/V sync is exact by construction and loudness lands on −14 LUFS with a −1 dBTP ceiling.
- 🔒 **Stays local.** Your video never leaves the disk; only clip scoring optionally calls your chosen AI provider or Jev (Laya never leaves the machine).

```mermaid
flowchart LR
    V(["Video"]) --> A["Extract audio"] --> T["Transcribe"] --> P["Pick clips"] --> F["Track speaker"] --> R["Render"] --> O(["Clips"])
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
digiclip input.mp4 --focus "pricing, AI agents" # steer picks toward a topic
digiclip ./videos/ --out-dir clips/             # a whole folder, one video after another
```

Everything lands in `<input-name>-digiclip/`: `clip-01-9x16.mp4` with its `.ass`/`.srt` captions and upload kit, plus `transcript.json/.srt` and `clips.json`. `digiclip --help` lists every knob.

### Make it yours

```bash
digiclip input.mp4 --aspect 1:1                 # also 4:5 (feed) and 16:9 (YouTube)
digiclip input.mp4 --headline                   # clip title pinned on top (or --headline "Your text")
digiclip input.mp4 --caption-anim words         # caption motion: pop (default), words, none
digiclip input.mp4 --progress-bar               # watch-time bar along the bottom (or --progress-bar "#00E5FF")
digiclip input.mp4 --logo logo.png --logo-pos br   # corner logo: tl, tr (default), bl, br
digiclip input.mp4 --music bed.mp3 --music-db -12  # looped music bed, ducked under speech
```

- **Aspect.** The tracker, camera, captions and blur fill all work on the chosen canvas, and files are named after it (`clip-01-1x1.mp4`). Captions move to the lower third on non-vertical canvases, so they stay off faces.
- **Headline.** A white title card shown for the whole clip, top centre, below the platform's top bar: at most two balanced lines, one accented word, a quick pop-in. Bare `--headline` uses each clip's title (the AI picker writes a 3–7 word headline); offline, a short self-contained sentence from the clip's first seconds, or no headline when nothing reads well on its own.
- **Caption motion.** `pop` (default): each line pops in and fades out, keywords bump as they're spoken, lines never flash for a split second or run across a sentence end. `words`: the same, with words appearing one by one as they're spoken. `none`: static lines, as before.
- **Logo.** A PNG or JPEG, sized to one box whatever its shape (square mark or wide wordmark) and slightly translucent. The headline and captions keep clear of its corner.
- **Music.** Normalised to `--music-db` below the speech (default −16), looped to length, faded in and out, and ducked by roughly 6–9 dB while someone talks. The mix still lands on −14 LUFS.
- **Focus.** The LLM is told the topic, the offline scorer boosts sentences that mention it, and every candidate that covers it ranks first. When nothing matches, the best clips overall are used.
- **Batch.** Pass several files or a folder. Each video gets its own `<name>-digiclip/` folder, under `--out-dir` when given. If one video fails, the rest still run and the run exits with an error that lists the failures.

### 🤖 Let Claude drive it (MCP)

`digiclip --serve` also runs an [MCP](https://modelcontextprotocol.io) server, so Claude Desktop, Claude Code, Cursor or any MCP app can use DigiClip the way you do: start jobs from files or links with any preset or option, wait for them, read transcripts, retitle, re-cut or add clips, look at a clip's poster frame, grab the posting kit, change settings and manage models. 21 tools, and every call shows up live in the app.

- **Stdio:** point the app at `digiclip --mcp`. It's a small bridge that forwards to the running engine and opens the DigiClip app if nothing's listening. The engine also keeps a copy at `<data>/mcp/digiclip-mcp(.exe)`, so updates never fight a running bridge.
- **HTTP:** `http://127.0.0.1:47420/mcp` (streamable HTTP, JSON replies) with `Authorization: Bearer <token>`. Loopback only, browser origins other than localhost are refused.
- `<data>/mcp/server.json` has the live port and token. In the app, the **AI apps** page adds DigiClip to Claude Desktop, Claude Code or Cursor in one click, and has the on/off switch, port and token.
- Your API keys are write-only: tools can set them but never read them back.

## ⚙️ Tune it

| Variable / flag | Default | What it does |
|---|---|---|
| `--ai-provider` | `openrouter` | Clip AI provider: `openrouter`, `openai`, `anthropic`, `gemini`, `ollama_cloud`, `ollama` (this PC), `lm_studio`, `groq`, `mistral`, `deepseek`, `xai`, `together`, `fireworks`, `cerebras` or `custom` (any OpenAI-compatible address). Serve setting: `ai_provider`. |
| `--ai-base-url` | provider's own | Address for `ollama`, `lm_studio` and `custom`. |
| `OPENROUTER_API_KEY` / `--openrouter-key` (`--ai-key`) | — | Key for the provider (its own variable is read too: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OLLAMA_API_KEY`, `GROQ_API_KEY`…). Local providers need none. Without a ready provider the offline scorer runs. |
| `OPENROUTER_MODEL` / `--openrouter-model` (`--ai-model`) | provider's default (OpenRouter: `nvidia/nemotron-3-ultra-550b-a55b:free`) | Scoring model. The app fetches each provider's model list itself. |
| `--decider` | `auto` | System One judge: `jev`, `laya`, `off`, or `auto` (Jev when a key is set, else Laya when downloaded and the talk is English). Serve setting: `decider`. |
| `JEV_API_KEY` / `--jev-key` | — | Key for TypeSafe's Jev. `--jev-model` picks the model (default `jev-latest`). |
| `--model` | `base.en` | Whisper model: `tiny.en`, `base.en`, `large-v3-turbo(-q5_0)`, `large-v3`. |
| `--lang` | `en` | Spoken language (`es`, `de`, `ja`…) or `auto` to detect it. Needs a multilingual model; `.en` models always transcribe English. Serve setting: `stt_lang`. |
| `--gpu` | `true` | GPU everything: NVENC / VideoToolbox renders, DirectML tracking, Vulkan STT. |
| `--style` | `karaoke` | `tiktok` · `karaoke` · `hormozi` · `minimal` · `beast` · `neon` · `highlight` · `ghost` |
| `DIGICLIP_RENDER_JOBS` | auto | How many clips render at once. |
| `DIGICLIP_FFMPEG` | — | Use a specific ffmpeg. |
| `DIGICLIP_PRIORITY` | gentle | `normal` turns off gentle mode (below-normal CPU and low disk-I/O priority for the engine and its ffmpeg/tracker children, so the PC stays responsive during renders). |

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

Powered by whisper.cpp · ffmpeg · ONNX Runtime · OpenRouter · Jev & Laya (System One) · axum.
Drives the [DigiClip](https://github.com/n1ssyyy/DigiClip) desktop app.
