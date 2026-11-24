%bcond_without check

%global __cargo_common_opts %{?_smp_mflags} -Z avoid-dev-deps --locked

Name:           wayland-voice-dictation
Version:        0.8.2
Release:        1%{?dist}
Obsoletes:      hyprland-voice-dictation < 0.6.0
Provides:       hyprland-voice-dictation = %{version}-%{release}
Summary:        Offline voice dictation for Wayland desktops with Parakeet speech recognition
License:        MIT OR Apache-2.0
URL:            https://github.com/MasonRhodesDev/wayland-voice-dictation
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  systemd-rpm-macros
BuildRequires:  pkg-config
BuildRequires:  clang-devel
BuildRequires:  pkgconfig(libonnxruntime)
BuildRequires:  pipewire-devel
BuildRequires:  alsa-lib-devel
BuildRequires:  fontconfig-devel
BuildRequires:  freetype-devel
BuildRequires:  libxkbcommon-devel
BuildRequires:  wayland-devel
Requires:       wtype
Requires:       pipewire
Requires:       onnxruntime
Recommends:     playerctl

%description
Offline voice dictation daemon for Hyprland (and other Wayland compositors)
using NVIDIA Parakeet TDT speech recognition via ONNX Runtime. Press a key
to start recording, press again to transcribe and type the result into the
focused window with wtype. Ships a systemd user service and a standalone
model download script. The ~1.6 GB Parakeet model is NOT part of this
package; download it after install with `voice-dictation download-model`.

%prep
%autosetup -p1 -a1
%cargo_prep -v vendor
cfg=.cargo/config.toml
[ -f "$cfg" ] || cfg=.cargo/config
cat vendor-git-sources.toml >> "$cfg"

%build
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
%cargo_install
install -Dpm0644 dist/voice-dictation.service %{buildroot}%{_userunitdir}/voice-dictation.service
install -Dpm0755 scripts/download-parakeet-model.sh %{buildroot}%{_datadir}/%{name}/download-parakeet-model.sh

%if %{with check}
%check
%cargo_test
%endif

%post
%systemd_user_post voice-dictation.service

%preun
%systemd_user_preun voice-dictation.service

%postun
%systemd_user_postun_with_restart voice-dictation.service

%files
%license LICENSE-MIT LICENSE-APACHE LICENSE.dependencies
%doc README.md
%{_bindir}/voice-dictation
%{_userunitdir}/voice-dictation.service
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/download-parakeet-model.sh

%changelog
* Tue Nov 24 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.8.2-1
- A dictation shorter than 25 ms no longer panics the Parakeet engine, and a
  panic in any local model surfaces as an error instead of leaving dictation
  dead until the daemon restarts.

* Mon Nov 23 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.8.1-1
- download-model and download-parakeet-model.sh fetch vocab.txt, which
  parakeet-rs needs to load the model. Fresh installs failed at daemon start
  with "vocab.txt not found".
- Idle inhibit goes through keepawake and systemd readiness through
  sd-notify, so systemd-devel is no longer a build dependency.
- macOS permission checks come from rust-platform-kit 0.2.
- url 2.5.9 for RUSTSEC-2026-0592.

* Thu Nov 19 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.8.0-1
- macOS support behind a per-OS platform layer. Linux keeps its D-Bus
  interface, CLI and service unchanged.
- Logs default to WARN, and the systemd unit no longer forces RUST_LOG=info.
- Restart Daemon no longer races the overlay's exit, so systemd reliably
  restarts it.
- confirm queues behind a pending start, so a quick push-to-talk tap no
  longer leaves the daemon recording.
- cpal falls back to the device's native format and resamples when it cannot
  open mono 16 kHz, and builds its streams paused.
- download-model writes each file to a .part file first, so an interrupted
  download is retried instead of skipped.
- Correction learning matches AT-SPI events to the focused window by pid, so
  it learns on Hyprland, and no longer learns from a cleared, sent or
  replaced dictation.
- ammonia 4.2.5 for RUSTSEC-2026-0533.

* Tue Nov 10 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.7.1-1
- The overlay survives a monitor being removed. It destroyed its wl_surface
  before the layer surface, which Hyprland answers with a fatal "invalid
  object" protocol error, so every undock killed the daemon.
- A removed monitor no longer releases the pointer for the overlays that
  remain.
- Hyprland's FALLBACK placeholder output is not expected to carry an overlay.

* Fri Oct 09 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.7.0-1
- Engines declare their post-processing stages; [pipeline] overrides them per engine.
- New openai:gpt-live-transcribe realtime engine with live preview and batch fallback.
- gpt-transcribe keyword, prompt and language hints from the user dictionary.
- Optional llm_correction stage on Amazon Bedrock.
- transcribe-file command for comparing engines and stage lists.
- Obsolete the old hyprland-voice-dictation package name.

* Sat Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.6.2-1
- Republish so the wayland-voice-dictation COPR project is created with current chroots.

* Sat Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.6.1-1
- Resolve hypr-ipc 0.1.1 so the old hypr-paths crate leaves the dependency graph.

* Sat Aug 22 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.6.0-1
- Rename package from hyprland-voice-dictation (desktop-commons ADR 0005).
- Depend on xdg-paths and logind-session (renamed crates).

* Thu Aug 20 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.5.5-1
- Use Rustls for HTTP and dynamically load system ONNX Runtime, removing native OpenSSL build dependencies

* Thu Aug 20 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.5.4-1
- Admit the pinned shared Slint runtime and update h2 for RUSTSEC-2026-0258

* Thu Aug 20 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.5.3-1
- Freeze the Slint overlay while hidden and wake it from real events

* Sun Aug 16 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.5.2-1
- Pin hypr-paths, hypr-logind, and hypr-ipc to crates.io 0.1.0.

* Fri Jul 24 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.5.1-1
- Fuzzy vocabulary post-correction: snap transcribed words onto the user
  dictionary to fix vendor/product names and split words (e.g. "aws agent
  tools" -> "aws-agent-tools", "hyperland" -> "hyprland"). Engine-agnostic;
  toggle with enable_fuzzy_vocab.

* Tue Jul 21 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.5.0-1
- Event-emitting StreamingEngine contract + LocalEngineDriver (streaming
  partials and correct finalize); daemon loop rewired to consume engine events
- Opt-in OpenAI transcription engine: set model = "openai:whisper-1" with
  OPENAI_API_KEY (batch transcription, no streaming partials)
- Minimal (style2) overlay now renders live transcription text; UI split into
  reusable components

* Tue Jul 14 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.4.1-1
- BuildRequires pkgconfig(libonnxruntime): COPR builders have no network for
  ort-sys' prebuilt-binary fallback

* Sat Jul 04 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.4.0-1
- Correction/dictionary UX: dict/subst/corrections CLI, learned-correction
  store with hot-reload, wezterm-native correction backend, IME probe
- Reject shell-execution as corrections; hot-reload corrections.json on CLI edits
- Real AT-SPI2 e2e test; fix AT-SPI listener dying on first stream error

* Fri Jul 03 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.3.4-1
- Disable makepkg LTO (onig C objects vs ld.lld)

* Fri Jul 03 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.3.3-1
- pipewire-rs 0.10 migration (builds against pipewire 1.6)

* Fri Jul 03 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.3.2-1
- Fix first-run CI gates (see git log)

* Fri Jul 03 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.3.1-1
- Standardized packaging release: shared CI, arch-repo + COPR pipeline

* Thu Jul 02 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.3.0-1
- Standardized packaging: PKGBUILD + RPM spec share the dist/ payload,
  systemd user unit uses the packaged /usr/bin/voice-dictation path
- First COPR-buildable release (vendored cargo sources incl. git deps)
