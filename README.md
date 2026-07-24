# Nextbase CLI (Rust)

Rust rewrite of the Nextbase CLI. `nextbase` is the umbrella command; **Wisper**
(hold-to-record dictation) is its first and currently only tool. NoteBot is out of
scope here.

```bash
nextbase                    # what this installs, where to start
nextbase wisper <command>   # namespaced
wisper <command>            # same thing, direct
```

## Why the rewrite

The TypeScript build shells out to SoX for recording, `swift` for macOS hotkeys, and
PowerShell/VBScript on Windows. That costs a per-launch compile of the Swift helper
(~2s for three shortcuts), makes SoX an install prerequisite, and means the installer
runs `npm install` plus `tsc` on the user's machine. Native capture, native event
taps, and a single prebuilt binary remove all of it.

## Layout

```
crates/nextbase-core   config, storage, log, shortcut parsing, provider verification
crates/nextbase-cli    clap surface, setup wizard, the `nextbase` and `wisper` binaries
```

## On-disk compatibility

This build reads and writes the **same** `~/.wisper-cli/` files as the TypeScript
CLI — `config.json`, `history.json`, `wisper.log` — so both can run side by side
during the port and existing users keep their keys, shortcuts, and history.

Unknown config fields are carried through untouched (`Config::extra`), so writing
config from this build never drops a setting the TypeScript CLI still owns. There is
a test for it.

Two rules worth keeping:

- **`wisper.log` stays plain text.** The listener's start check greps it for literal
  markers like `Shortcut registered:`; ANSI colour would break that. Style terminal
  output at the call site (`ui.rs`), never in `log.rs`.
- **Shortcuts are validated before they are saved.** A key the platform cannot
  register used to reach config and then throw at every listener start — which
  autostart turned into a silent restart loop.

## Terminal UX

- `inquire` for the setup wizard — sequential prompts that stay in scrollback, and
  degrade cleanly when stdin is not a TTY (setup runs from installers and over SSH).
- `indicatif` spinners for network waits like key verification.
- `owo-colors` through `anstream`, so colour disappears when piped or `NO_COLOR` is set.
- `ratatui` is reserved for the two genuinely live moments — inline viewport for
  shortcut capture and mic level metering — plus a future full-screen `wisper dash`.
  Keep it on the crossterm backend that `inquire` already uses.

## Phases

| Phase | Scope | State |
|---|---|---|
| 0 | Workspace, clap surface, config/storage/log, shortcut logic + tests, setup wizard, `status`/`shortcuts`/`history`/`add`/`logs`/`shortcut`/`provider` | **done** |
| 1 | Providers via `reqwest`: `transcribe`, `polish`, `spell` text, Sarvam REST → Batch → chunk → Groq ladder | next |
| 2 | Audio: `cpal` capture + `hound` WAV, level metering, `mic` / `mic --auto` | |
| 3 | macOS hotkeys (CGEventTap, incl. modifier-only) + paste (`arboard` + CGEvent) → full `listen` | |
| 4 | Autostart (launchd) + single-instance process state | |
| 5 | Web dashboard (`axum`, HTML via `include_str!`) | |
| 6 | Windows: hotkeys, paste, device enumeration, autostart | |
| 7 | Releases: GH Actions matrix, signing + notarization, `self_update`, installers | |

Commands from later phases exist in the CLI surface and fail with the phase that
will bring them, rather than silently doing nothing.

## Carried over deliberately

Port behaviour, don't reinvent it. These encode real field fixes:

- the shortcut normalization tables (`Cmd`/`Command`/`Win`/`Window` → `META`, and
  `CommandOrControl` following the platform)
- the Sarvam routing ladder for long audio
- silent-recording and dead-microphone detection
- the platform-quirk comments in the TypeScript source

## Known migration cost

macOS Accessibility permission is bound to binary identity, so **every existing user
must re-grant it** when they move to this binary. Phase 3 needs a first-run
`AXIsProcessTrusted` check with a real prompt, not a silent failure. Signing and
notarization are required before shipping it the way `install.sh` ships today.

During migration a TypeScript listener and a Rust listener would both fire on one
keypress, so the Rust listener must also sweep `cli.js _listen` processes.

## Development

```bash
cargo build
cargo test
cargo run --bin wisper -- status
```

Test against a sandboxed config instead of your own:

```bash
HOME=/tmp/wisper-sandbox cargo run --bin wisper -- status
```
