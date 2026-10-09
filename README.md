# Wayland Voice Dictation

Offline voice dictation for Wayland desktops using NVIDIA Parakeet TDT speech recognition. Press a key to start recording, press again to transcribe and type the result into any focused window.

## Features

- **Offline, private** — all processing runs locally, no cloud API
- **NVIDIA Parakeet TDT 0.6b** — high-accuracy English speech recognition via ONNX Runtime
- **Silero VAD** — voice activity detection to trim silence automatically
- **Harper grammar checker** — optional light grammar correction on transcribed text
- **Slint overlay** — transparent HUD showing recording state and live transcription
- **System tray** — status icon with device selection and quick controls
- **D-Bus control** — clean interface for keybind integration
- **systemd daemon** — persistent background service with watchdog support
- **playerctl integration** — auto-pause/resume media during recording

## How it works

The whole pipeline runs on-device inside one daemon — no audio ever leaves the machine.

```mermaid
flowchart TD
    key["Hyprland keybind"] -->|"exec voice-dictation toggle"| cli["CLI client"]
    cli -->|"D-Bus com.voicedictation.Daemon"| engine

    subgraph engine["voice-dictation daemon"]
        cap["PipeWire / ALSA capture"] --> vad["Silero VAD"]
        vad --> asr["Parakeet TDT — ONNX Runtime, fully local"]
        asr --> post["Post-processing: Harper grammar, user dictionary, hotword substitution"]
        post --> inject["wtype injection into focused window"]
    end

    engine -->|"live transcription over unix socket"| hud["Slint layer-shell overlay HUD"]
    engine -->|"ksni StatusNotifierItem"| tray["System tray: status, device selection"]
    systemd["systemd Type=notify watchdog"] -->|"supervises, restarts on stall"| engine
```

## Requirements

- Wayland compositor (Hyprland, Sway, etc.)
- `wtype` — keyboard input injection
- PipeWire or ALSA audio
- ~1.6 GB disk space for the Parakeet model

Optional: `playerctl` for media pause/resume.

## Installation

### Arch Linux

Add the [mason](https://github.com/MasonRhodesDev/arch-repo) pacman repository to `/etc/pacman.conf`:

```ini
[mason]
# Import the signing key first: https://github.com/MasonRhodesDev/arch-repo#use-it
SigLevel = Required DatabaseRequired
Server = https://masonrhodesdev.github.io/arch-repo/x86_64
```

Then install:

```bash
sudo pacman -Sy wayland-voice-dictation
```

### Fedora

```bash
sudo dnf copr enable solaris765/wayland-voice-dictation
sudo dnf install wayland-voice-dictation
```

Both packages install the binary to `/usr/bin/voice-dictation`, the systemd
user unit, and the standalone model download script under
`/usr/share/wayland-voice-dictation/`.

### From source (development)

```bash
git clone https://github.com/MasonRhodesDev/wayland-voice-dictation
cd wayland-voice-dictation

make install
```

`make install` builds the release binary, installs it to `~/.local/bin/voice-dictation`,
installs and reloads the systemd user service, restarts the daemon so it runs the binary
you just built, and removes any stale shadow copy (see warning below). Override the
locations if you need to:

```bash
make install BINDIR=/usr/local/bin UNITDIR=/etc/systemd/user
```

To remove everything (your model and config under `~/.config/voice-dictation` are kept):

```bash
make uninstall
```

> **Do not `cargo install --path .`** — that installs to `~/.cargo/bin`, which usually
> precedes `~/.local/bin` on `PATH`. The keybind would then run the `~/.cargo/bin` copy
> while the systemd daemon runs the `~/.local/bin` copy, and a stale shadow there makes
> the keybind silently misbehave. Use `make install`, which keeps a single binary in
> `~/.local/bin`. Run `make doctor` (or `voice-dictation diagnose`) any time the keybind
> seems dead — it reports duplicate binaries on `PATH` and client/daemon skew.
> The packaged installs above are immune to this: they own the single copy in
> `/usr/bin`. Don't mix a dev install with a packaged one.

## Download the Model

The Parakeet model (~1.6 GB) is not included and must be downloaded separately:

```bash
voice-dictation download-model
```

This downloads the model from HuggingFace to `~/.config/voice-dictation/models/parakeet/`. Files already present are skipped.

Alternatively, use the standalone shell script (requires `curl`):

```bash
bash scripts/download-parakeet-model.sh
# or, from a packaged install:
bash /usr/share/wayland-voice-dictation/download-parakeet-model.sh
```

## Setup

### Start the daemon

```bash
# Enable on login
systemctl --user enable --now voice-dictation

# Check status
systemctl --user status voice-dictation
journalctl --user -u voice-dictation -f
```

### Hyprland keybind

Add to `~/.config/hypr/hyprland.conf`:

```
bind = SUPER, V, exec, voice-dictation toggle
```

Press `Super+V` to start recording. Press again to confirm and type the transcription.

> If the keybind seems to do nothing, run `voice-dictation diagnose` (or `make doctor`).
> The most common causes are a duplicate binary on `PATH` (the bind runs a different one
> than the daemon) or a wedged daemon — `systemctl --user restart voice-dictation` clears
> the latter.

### Other compositors

Any Wayland compositor supporting `wtype` works. Map `voice-dictation toggle` to a key using your compositor's keybind system.

## CLI Usage

```
voice-dictation <COMMAND>

Commands:
  daemon              Start the dictation engine daemon
  start               Start a recording session
  stop                Cancel recording
  confirm             Finalize and type the transcription
  toggle              Start if idle, confirm if recording
  status              Show daemon and subsystem status
  config              Open the configuration TUI
  download-model      Download Parakeet model from HuggingFace
  list-audio-devices  List available audio input devices
  diagnose            Show diagnostics (model paths, audio, config)
  transcribe-file WAV Run a WAV through an engine and its stages (--model, --stages, --paced)
  debug list          List saved debug recordings
  debug play FILE     Play a debug recording
```

## Configuration

Run `voice-dictation config` to open the interactive configuration TUI, which covers all daemon settings:

![voice-dictation config TUI showing daemon settings: audio device, Parakeet model, grammar and capitalization toggles, correction learning](.github/screenshots/config-tui.png)

Config file: `~/.config/voice-dictation/config.toml`

```toml
# Audio device (leave empty for system default)
audio_device = ""

# Audio backend: "pipewire" or "alsa"
audio_backend = "pipewire"

# Grammar checking
grammar_check = true
```

Run `voice-dictation diagnose` to inspect the current configuration and model status.

### Engines

Set `model` in `[daemon]` to choose one engine:

| Model | Runs | Live preview | Needs |
|---|---|---|---|
| `parakeet:default` | Locally, offline | Yes | The downloaded model |
| `openai:gpt-live-transcribe` | OpenAI realtime WebSocket | Yes | `OPENAI_API_KEY` |
| `openai:gpt-transcribe` | OpenAI, one upload when you stop | No | `OPENAI_API_KEY` |

If the realtime stream fails, the realtime engine transcribes the recorded audio once with `gpt-transcribe`, so the utterance is not lost. The OpenAI engines send your user-dictionary words as keyword hints. The `[openai]` section sets the prompt, extra keywords, languages and the realtime delay.

### Post-processing stages

Text helpers are named stages: `acronyms`, `punctuation`, `word_substitution`, `fuzzy_vocab`, `grammar` and `llm_correction`. Each engine declares the stages it needs. Parakeet declares the first five. The OpenAI engines declare none, because their output is already punctuated and uses keyword hints for vocabulary.

The `[pipeline]` section overrides the declaration per engine. `default` keeps the declaration and `none` runs no stages. The `enable_*` switches in `[daemon]` still turn a stage off for every engine.

```toml
[pipeline]
parakeet = "acronyms,punctuation,word_substitution,fuzzy_vocab"   # local, without Harper
openai = "default"                                                # nothing
```

`grammar` and `llm_correction` run on the final text only, never on the live preview.

To compare engines or stage lists on one recording, convert it to 16 kHz mono and run:

```bash
voice-dictation transcribe-file clip.wav --model parakeet:default --stages none --paced
```

### LLM correction

The `llm_correction` stage sends the final text to a model on Amazon Bedrock. It applies spoken self-corrections such as "scratch that" and fixes glossary terms. Add it to a `[pipeline]` list and set the model:

```toml
[llm_correction]
model = "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123"
region = "us-west-2"
aws_profile = "my-sso-profile"   # read with `aws configure export-credentials`
timeout_ms = 3000
```

On a timeout, an expired credential, or a reply that is not an edit of the input, the stage keeps the original text and logs a warning.

## Troubleshooting

**Daemon not starting:**
```bash
journalctl --user -u voice-dictation -n 50
voice-dictation diagnose
```

**Model missing:**
```bash
voice-dictation download-model
```

**No audio input / wrong device:**
```bash
voice-dictation list-audio-devices
# Then set audio_device in config
voice-dictation config
```

**wtype not found:**
```bash
# Arch
sudo pacman -S wtype
# Fedora
sudo dnf install wtype
```

## Project Structure

```
src/main.rs                   CLI frontend and D-Bus client
dictation-engine/             Core library
  src/lib.rs                  Daemon entry point and state machine
  src/engine/                 Parakeet ONNX inference
  src/audio/                  PipeWire/ALSA capture
  src/vad.rs                  Silero VAD
  src/post_processing/        Grammar and text cleanup
dictation-types/              Shared types
slint-gui/                    Overlay HUD (Slint UI)
dist/
  voice-dictation.service     systemd user unit (packaged payload)
packaging/
  PKGBUILD                    Arch Linux package
  wayland-voice-dictation.spec  Fedora RPM spec (COPR)
  build-srpm.sh               SRPM builder (vendored cargo deps)
scripts/
  check-deps.sh               Dependency checker
  download-parakeet-model.sh  Standalone model downloader
  list-audio-devices.sh       List audio devices
config-schema.json            Config schema for the TUI
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) at your option.
