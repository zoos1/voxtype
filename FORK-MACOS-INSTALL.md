# voxtype on macOS — fork install and restore guide

This fork (`zoos1/voxtype`, branch `macos`) tracks upstream
[peteonrails/voxtype](https://github.com/peteonrails/voxtype) and adds:

| Addition | Where |
|---|---|
| Source build with **Parakeet TDT 0.6B v3 (int8)** + Metal, about 0.3 s decode for a 12 s utterance on Apple Silicon | `scripts/macos-local-install.ts` |
| **Native macOS on-screen display**: centered, smoke-glass background, white text; live level strip while recording, "Transcribing…", then the transcript | `src/bin/voxtype_osd_macos.rs` |
| `voxtype-osd` launcher execs the macOS overlay | `src/bin/voxtype_osd.rs` |
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

## 5. Run the daemon and grant permissions

Run the daemon from a terminal app that has the permissions. macOS grants
them to the terminal, not to the binary:

```sh
voxtype daemon
```

In System Settings > Privacy & Security, give that terminal **Microphone**,
**Accessibility** (to type text) and **Input Monitoring** (for the FN hotkey).
The daemon starts the overlay itself; its log shows `OSD child started`.

Alternative: `voxtype setup app-bundle` creates `/Applications/Voxtype.app`
with Login Items, which holds its own permissions (not yet tested with this
fork's build).

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

- `voxtype-osd-macos --demo "<text>" --snapshot out.png` renders the panel to
  a PNG. The snapshot does not capture the live blur; it shows the flat
  fallback color.
