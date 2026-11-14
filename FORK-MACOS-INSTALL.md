# voxtype on macOS — fork install and restore guide

This fork (`zoos1/voxtype`, branch `macos`) tracks upstream
[peteonrails/voxtype](https://github.com/peteonrails/voxtype) and adds:

| Addition | Where |
|---|---|
| Source build with **Parakeet TDT 0.6B v3 (int8)** + Metal, about 0.3 s decode for a 12 s utterance on Apple Silicon | `scripts/macos-local-install.ts` |
| **Native macOS on-screen display**: small centered see-through smoke panel, white JetBrains Mono; dot-waveform or voice-driven spectrogram that appears when you speak, "Transcribing…", then the transcript | `src/bin/voxtype_osd_macos.rs` |
| `voxtype-osd` launcher execs the macOS overlay | `src/bin/voxtype_osd.rs` |
| **LaunchAgent service**: starts at login, launchd restarts the daemon if it dies | `scripts/macos-service.ts` |
| Daemon publishes the delivered text to `$runtime/last_text` (mode 0600) for the overlay | `src/osd/session.rs`, `src/daemon.rs` |

The upstream macOS release binary ships without the `parakeet` feature, so
Parakeet needs this source build. Upstream's own DMG/cask guide is
`docs/INSTALL_MACOS.md`.

## 1. Prerequisites

```sh
xcode-select --install          # Apple command line tools (once)
brew install rust cmake         # cargo/rustc and cmake (whisper.cpp needs cmake)
curl -fsSL https://bun.sh/install | bash   # bun runs the install script
```

## 2. Clone

```sh
git clone https://github.com/zoos1/voxtype.git ~/code/voxtype
cd ~/code/voxtype
git switch macos
git remote add upstream https://github.com/peteonrails/voxtype.git
```

## 3. Build and install

```sh
bun scripts/macos-local-install.ts
```

This builds `--release --features gpu-metal,parakeet` (the first build takes
about 10 minutes), installs `voxtype`, `voxtype-osd` and `voxtype-osd-macos`
into `~/.local/opt/voxtype-<version>/`, symlinks them into `$(brew --prefix)/bin`,
downloads `parakeet-tdt-0.6b-v3-int8` (about 640 MB) and sets in the config:

```toml
engine = "parakeet"
[parakeet]
model = "parakeet-tdt-0.6b-v3-int8"
[osd]
enabled = true
```

Flags: `--no-build` reinstalls the existing build; `--no-config` leaves the
config alone.

If the Homebrew cask of voxtype is installed, remove it first
(`brew uninstall --cask voxtype`), or a later `brew upgrade` puts the old
binary back over the symlink.

## 4. Config

The config lives at `~/Library/Application Support/voxtype/config.toml` and
models live in `~/Library/Application Support/voxtype/models/`. Keep a copy of
your `config.toml` somewhere private (it holds personal word replacements);
it is deliberately not in this repo. Useful settings:

```toml
[whisper]
flash_attention = true        # if you switch back to Whisper

[text]
replacements = { "vox type" = "voxtype" }   # fix names the model mishears
```

## 5. Run the daemon as a service (starts at login, restarts if it dies)

```sh
bun scripts/macos-service.ts install
```

This copies the `voxtype` binary into `/Applications/Voxtype.app` (bundle id
`io.voxtype.daemon`, ad-hoc signed) and loads the LaunchAgent
`~/Library/LaunchAgents/io.voxtype.daemon.plist` with `RunAtLoad` and
`KeepAlive`: launchd starts the daemon at login and restarts it within 5 s
whenever it exits or crashes. Logs: `~/Library/Logs/voxtype/daemon.out.log`.

Grant **Voxtype** in System Settings > Privacy & Security: **Microphone**
(allow the prompt), **Accessibility** (to type text) and, if the FN hotkey
does not respond, **Input Monitoring**. The daemon starts the overlay itself;
its log shows `OSD child started`.

```sh
bun scripts/macos-service.ts status      # state, pid, run count
bun scripts/macos-service.ts refresh     # after rebuilding voxtype
bun scripts/macos-service.ts uninstall   # stop and remove the LaunchAgent
```

Do not also run `voxtype daemon` in a terminal or add Voxtype to Login
Items: the second instance fails on the lock. Replacing the main binary
changes its ad-hoc signature, so `refresh` resets and re-asks for the three
permissions only when the binary actually changed. Rebuilding just the
overlay never touches them (the overlay binaries live outside the bundle).

Without the service: run `voxtype daemon` from a terminal that holds the
three permissions (macOS then grants them to the terminal).

## 6. Verify

```sh
voxtype --version                         # voxtype 1.2.1 (or newer)
voxtype info variants | grep Features     # Features: parakeet, gpu-metal
voxtype-osd-macos --demo "Overlay check"  # overlay appears centered for 4 s
voxtype status                            # idle
```

Hold FN, speak, release. The overlay shows the level strip and timer, then
"Transcribing…", then your text, which is typed at the cursor.

## 7. Updating

Local changes: commit on `macos`, push to `origin`, re-run the install script.

```sh
git push origin macos
bun scripts/macos-local-install.ts
```

Moving to a new upstream release:

```sh
git fetch upstream --tags
git rebase --onto vX.Y.Z v1.2.1 macos   # replay the fork's commits onto the new tag
bun scripts/macos-local-install.ts
git push --force-with-lease origin macos
```

## 8. Rollback

- Engine: `voxtype config set engine whisper`, then restart the daemon.

- Binary: re-point the symlinks at an older install, for example
  `ln -sfn ~/.local/opt/voxtype-1.2.0/voxtype "$(brew --prefix)/bin/voxtype"`,
  or reinstall the upstream cask.

- Overlay: `voxtype config set osd.enabled false`.

## Overlay notes

- Reads only the daemon's runtime files in `$XDG_RUNTIME_DIR/voxtype` (or
  `/tmp/voxtype`): `state`, `recording.json`, `outcome.json`, `last_text` and
  the `audio.sock` level stream. It can crash or restart without affecting
  dictation; the daemon's supervisor restarts it.

- Click-through, never takes focus, shows on all Spaces and over full-screen
  apps, and centers on the screen that has keyboard focus.

- Look: a small see-through smoke panel (55% opaque near-black), rounded
  corners, no border or shadow, white JetBrains Mono Light 12 pt (falls back
  to the system monospaced font).

- Recording visualizer, set in `config.toml` and read at each recording:

  ```toml
  [osd]
  macos_visualizer = "dots"      # white-dot waveform (default)
  # macos_visualizer = "spectrum" # purple > pink > yellow spectrogram
  ```

  `dots` uses the daemon's level stream. `spectrum` taps the default input
  device itself while recording (2048-point FFT, 24 log bands, 80 Hz to
  8 kHz, drawn as one continuous image): silence draws nothing, the
  loudest recent speech is yellow and quieter speech fades through pink to
  dark purple, and the lit rows follow your pitch. It falls back to dots if
  no input opens. `VOXTYPE_OSD_VIZ` overrides the config.

- Speech gate: while recording, the panel stays hidden until you start
  talking (about 14 dB over the room's noise floor for 130 ms), and fades
  after 1.2 s of quiet. The floor is the 10th percentile of the last 3 s, so
  it adapts to the room. Other loud sounds that last as long as a syllable
  (someone else talking, a TV) can still open it.

- Transcript: left-aligned, wrapped at 40 characters a line (phone-feed
  width), up to 10 lines. It stays up 1 s plus 0.25 s a word (adult silent
  reading is about 238 words a minute, Brysbaert 2019), from 1.5 s to 12 s.

- Demo and snapshots: `voxtype-osd-macos --demo "<text>"` or
  `--demo-rec dots|spectrum` shows the panel for 4 s with a test signal; add
  `--snapshot out.png` to render a PNG instead, and `--checker` to composite
  it over a checkerboard so the transparency is visible.
  `voxtype-osd-macos --probe-mic 10` prints what the spectrum hears (band
  levels, noise floor, gate state) twice a second, with no window.
