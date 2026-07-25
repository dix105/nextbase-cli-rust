# Nextbase CLI (Rust)

Rust rewrite of the Nextbase CLI. `nextbase` is the umbrella command; **Wisper**
(hold-to-record dictation) is its first and currently only tool. NoteBot is out of
scope here.

```bash
nextbase                    # what this installs, where to start
nextbase wisper <command>   # namespaced
wisper <command>            # same thing, direct
```

## What the rewrite removed

The TypeScript build shelled out to SoX for recording, `swift` for macOS hotkeys,
and PowerShell/VBScript on Windows. All of it is gone — capture, event taps, and
keystroke injection happen in process.

Measured on this machine:

| | TypeScript | Rust |
|---|---|---|
| Listener start (3 shortcuts registered) | ~2.0s (compiles `mac-hotkey.swift` 3×) | **0.077s** |
| `wisper status` (warm) | 144ms | **11ms** |
| Install prerequisites | Node, npm, SoX, then `npm install` + `tsc` on the user's machine | one binary |
| Processes while listening | 1 node + 3 swift | 1 |

## Layout

```
crates/nextbase-core   config, storage, log, shortcuts, providers, audio,
                       hotkeys, paste, autostart, process state, updater
crates/nextbase-cli    clap surface, setup wizard, listener, dashboard, binaries
```

## On-disk compatibility

This build reads and writes the **same** `~/.wisper-cli/` files as the TypeScript
CLI — `config.json`, `history.json`, `wisper.log` — so both can run side by side
during the port and existing users keep their keys, shortcuts, and history.

Unknown config fields are carried through untouched (`Config::extra`), so writing
config from this build never drops a setting the TypeScript CLI still owns. There is
a test for it.

Rules worth keeping:

- **`wisper.log` stays plain text.** Start-up verification greps it for literal
  markers like `Shortcut registered:`; ANSI colour would break that. Style terminal
  output at the call site (`ui.rs`), never in `log.rs`.
- **Shortcuts are validated before they are saved.** A key the platform cannot
  register used to reach config and then throw at every listener start, which
  autostart turned into a silent 10-second restart loop.
- **One listener per machine.** The newest listener sweeps the others, matching the
  TypeScript listener's command line too, so the two builds cannot double-register.
- **The dashboard binds 127.0.0.1 only.** It exposes transcript history.

## Providers

All four are supported through their direct endpoints: Groq Whisper, ElevenLabs
Scribe, Sarvam, and the Nextbase Codex gateway.

Sarvam's Batch job API — upload, poll, download, speaker diarization — is **not**
here on purpose. It exists to handle hour-long meeting recordings, which is
NoteBot's job. Wisper records hold-to-talk clips of a few seconds. If a long file
is passed to `wisper transcribe` on Sarvam and a Groq key is present, it falls back
to Groq Whisper; otherwise it says so and points at `wisper provider`. Only
duration-related errors trigger that fallback, so an auth failure still surfaces as
an auth failure.

## Terminal UX

- `inquire` for the setup wizard — sequential prompts that stay in scrollback, and
  degrade cleanly when stdin is not a TTY (setup runs from installers and over SSH).
- `indicatif` spinners for network waits like key verification and transcription.
- `owo-colors` through `anstream`, so colour disappears when piped or `NO_COLOR` is set.
- API keys are entered masked, and a key is only saved once it verifies. The
  TypeScript setup printed the verification failure and saved the key anyway.
- `ratatui` is still not a dependency. It is worth adding for the two genuinely live
  moments — an inline viewport for live shortcut capture and mic level metering —
  plus a possible full-screen `wisper dash`. Keep it on the crossterm backend that
  `inquire` already uses.

## Phases

| Phase | Scope | State |
|---|---|---|
| 0 | Workspace, clap surface, config/storage/log, shortcut logic, setup wizard | **done** |
| 1 | All four providers, `transcribe`, `polish`/`spell` rewriting | **done** |
| 2 | `cpal` capture + `hound` WAV, level metering, `mic`, `mic --auto`, `record` | **done** |
| 3 | macOS CGEventTap hotkeys, paste, the listener loop | **done, verified on macOS** |
| 4 | Autostart (launchd/logon task/systemd), detached start, single instance | **done** |
| 5 | Embedded dashboard with search, copy, delete | **done** |
| 6 | Windows hotkeys, paste, autostart | **written, unverified** |
| 7 | CI and release workflows | **written, needs secrets and a first tag** |

## What still needs a human

1. **Grant Accessibility permission to the new binary.** macOS ties it to binary
   identity, so this build starts untrusted even though the old CLI was allowed.
   `wisper doctor` reports it. Every existing user will hit this on migration.
2. **Disable the TypeScript LaunchAgent before enabling autostart.** Both builds
   would register the same shortcuts and one press would fire twice. `wisper
   autostart on` refuses while `com.wisper.cli` exists:
   `launchctl bootout gui/$(id -u)/com.wisper.cli`
3. **Verify Windows.** The hotkey, paste, and autostart code cannot be compiled from
   macOS: `rustls` pulls in `ring`, whose build script needs a Windows C toolchain.
   The CI job on `windows-latest` is the only check it gets.
4. **Add signing secrets.** `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`,
   `MACOS_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`.
   Unsigned binaries hit Gatekeeper, and re-signing with a different identity
   invalidates the Accessibility grant from step 1.
5. **Live provider smoke test.** Nothing here has made a real API call yet.

## Known gaps

- `wisper autoupdate check --apply` reports what is available but does not replace
  the binary; that lands with the first published release.
- Recording uses the device's native sample rate (48 kHz here) rather than SoX's
  fixed 16 kHz. Providers resample server side, so this only costs upload size —
  roughly 3× — and a `rubato` resampler would remove it.
- Linux has autostart but no hotkeys or paste, same as the TypeScript build.
- Live shortcut capture still asks you to type the combo; the inline TUI for it is
  the main reason to add `ratatui`.

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets   # CI runs with -D warnings
cargo fmt --all --check
```

Test against a sandboxed config instead of your own — `HOME` is honoured
everywhere, exactly as Node's `os.homedir()` does:

```bash
HOME=/tmp/wisper-sandbox cargo run --bin wisper -- status
```

`wisper doctor` is the fastest way to see permission, device, shortcut, provider,
and listener state at once. `wisper record 3` tells a permission problem apart from
a device problem.
