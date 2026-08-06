# Nextbase CLI (Rust)

Rust rewrite of the Nextbase CLI. `nextbase` is the umbrella command with two tools:

- **Wisper** — hold-to-record dictation, paste, polish, spell fix.
- **Meeting Agent** — record a meeting, transcribe it with speaker labels, get notes.

```bash
nextbase                     # what this installs, where to start
nextbase wisper <command>    # namespaced
nextbase meeting <command>
wisper <command>             # same thing, direct
nbmeet <command>
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

## Install

One command. No git, no Rust toolchain, no SoX.

### macOS and Linux

```bash
curl -fsSL https://raw.githubusercontent.com/dix105/nextbase-cli-rust/main/install.sh | bash
```

### Windows

```powershell
iwr -useb https://raw.githubusercontent.com/dix105/nextbase-cli-rust/main/install.ps1 | iex
```

The installer downloads a prebuilt binary for your platform, stops any listener
still holding the old one, puts `wisper`, `nextbase` and `nbmeet` on your `PATH`, and
prints what to do next. If no prebuilt binary exists for your platform it falls back to
building from source, which needs [Rust](https://rustup.rs) but still not git.

It never overwrites a previous TypeScript install: that `wisper` is renamed to
`wisper-ts` and stays callable, so rolling back is one command.

Knobs, if you need them:

```bash
WISPER_BIN_DIR=/usr/local/bin bash install.sh   # where to install
WISPER_VERSION=v0.1.0        bash install.sh   # pin a version
```

### From source

```bash
cargo install --git https://github.com/dix105/nextbase-cli-rust nextbase-cli --locked
```

If a previous TypeScript build is installed, note that it lives in `~/.local/bin`,
which comes *before* `~/.cargo/bin` on a default macOS setup — so plain `wisper`
would keep running the old build. Check with `command -v wisper`.

### First run

```bash
wisper setup     # model, API key, shortcut, preferences
wisper doctor    # permissions, microphone, shortcuts, provider, listener
wisper listen    # start the background listener
```

On macOS, global shortcuts need Accessibility permission. `wisper setup`, `wisper
listen` and `wisper doctor` ask macOS for it directly — the system dialog names the
binary and adds it to the Accessibility list, so all that is left is the switch;
there is no path to copy or file picker to navigate. Nothing can award the grant
programmatically, so this is as direct as macOS allows.

Run from a terminal that already has Accessibility, permission is inherited and
nothing is asked — so `wisper doctor` can look green while a LaunchAgent listener
still cannot register shortcuts. The grant has to be against the `wisper` binary
itself (toggle it under Privacy & Security > Accessibility; not only Terminal).
Because the grant is tied to that exact binary, it has to be given again after an
update replaces it. The listener now stays up and waits for the grant instead of
exiting into a KeepAlive restart loop. Until releases are signed, macOS also
quarantines a downloaded binary; the installer clears that for you.

### Updating

```bash
wisper update           # install the latest release over this one
wisper update --check   # only report whether one exists
```

No `curl` re-run needed. All three binaries are replaced together — they share the
core, so a new `wisper` beside an old `nbmeet` is not a state worth allowing. It
downloads the release binaries, refuses to install anything that does not run, stops
the listener for the swap (Windows will not replace a running executable at all), and
restarts it afterwards — through the login launcher if one owns it, so launchd cannot
respawn the listener from a half-written binary.

The listener also checks periodically and writes a line to `wisper logs` when a
release is out; `wisper autoupdate on|off|status` controls that. It never installs
by itself on purpose: while releases are unsigned, replacing the binary voids the
macOS Accessibility grant, and doing that unattended would leave the shortcut
silently dead.

### Uninstall

```bash
wisper autostart off
rm -f ~/.local/bin/wisper ~/.local/bin/nextbase ~/.local/bin/nbmeet
rm -rf ~/.wisper-cli        # also removes config, keys, and history
rm -rf ~/.nextbase          # meeting recordings, transcripts, and notes
```

## Meeting Agent

```bash
nbmeet setup     # paste a Sarvam key (transcription) and a Groq key (summaries)
nbmeet doctor    # microphone, system audio, permissions, keys
nbmeet start     # record
nbmeet stop      # stop, transcribe, summarise
nbmeet audio <f> # transcribe a file or URL you already have
nbmeet open      # dashboard, with start and stop buttons
```

### Audio you already have

```bash
nbmeet audio ~/Downloads/standup.m4a
nbmeet audio https://example.com/recording.mp3
```

The **original bytes** go to Sarvam — it accepts mp3, m4a, flac, ogg, opus and webm
natively, so re-encoding before upload would only add a conversion step to blame for a
bad transcript. The sample gate needs a WAV to cut three minutes from, so for other
formats `ffmpeg` makes a copy *for that alone*; without `ffmpeg` the gate is skipped and
says so rather than silently transcoding.

The dashboard has the same field, and takes either a local path or a URL.

Four files land in `~/.nextbase/meetings/<id>/`: `meeting-note.md`,
`full-diarized-transcript.md`, `full-transcript.txt` and `processing-metadata.json`.
The two transcript files are written **before** the summary is attempted, so a crash
while Groq is working cannot cost you a transcription Sarvam has already been paid for.
Config and API keys are shared with Wisper, so a Sarvam key saved by either tool works
in both.

### More than one Sarvam key

Batch bills per job, and a key that runs out of credit part-way through would otherwise
strand the recording.

```bash
nbmeet key sarvam --add        # save a standby key
nbmeet key                     # how many are saved, and in what order
nbmeet key sarvam --remove 1   # drop one
```

Keys are tried in order and only moved past when the key itself is the problem — an
empty balance, or one that is rejected outright. A rate limit is waited out on the same
key first, honouring `Retry-After` up to two minutes. Anything else fails immediately,
because every key would fail it the same way.

Nothing here can interrupt a recording: the network is only touched after `stop`. But a
job belongs to the account that created it, so rotating past the upload step means a new
job and the **same audio uploaded again** — announced when it happens. If every key is
spent, the recording is kept and `nbmeet process` picks it up; the model is never
downgraded behind your back to get a transcript out.

### It records both sides

A meeting recorded from the microphone alone transcribes half a conversation — the
other participants are coming out of your speakers, not sitting in the room. So the
microphone and the system output are captured together and mixed into one 16 kHz mono
WAV (two hours is ~115 MB rather than ~690 MB at 48 kHz, and Sarvam takes 16 kHz PCM
natively).

- **macOS** uses ScreenCaptureKit, which needs Screen Recording permission — `nbmeet
  doctor` asks for it. Requires macOS 13+.
- **Windows** uses WASAPI loopback on the default playback device.
- **Linux** uses the PulseAudio/PipeWire monitor source.

`nbmeet doctor` probes each source separately and says whether it actually *heard*
anything, because a meeting recorded with a silently broken system source looks
completely fine until the far side is missing from the transcript. If a source stayed
silent for a whole recording, the note says so.

### The sample gate

After recording, a three-minute window — chosen by RMS energy, so it does not land on
the silence and "can you hear me" that opens every recording — is transcribed twice, in
Sarvam's `transcribe` and `codemix` modes, as two concurrent jobs. Both are shown with
what was measured: elapsed time, timestamp coverage, diarized segment and speaker-label
counts, and the provider's language detection. You pick one, or reject.

This exists because a transcript that reads well can still corrupt names, numbers and
commitments, and for Gujarati/Hindi/English speech the two modes differ exactly there.
Nothing reports an accuracy percentage, because nothing here can measure one — language
detection says which language was heard, not whether the words are right.

`nbmeet gate off` skips it and transcribes immediately in the last approved mode.

### Modes

```bash
nbmeet mode              # list, current one marked
nbmeet mode translate    # English notes from a Gujarati meeting, no second step
```

| Mode | Output |
|---|---|
| `transcribe` | the spoken language, as spoken |
| `codemix` | Indic speech in Latin script, English terms kept |
| `translate` | English, whatever was spoken |
| `verbatim` | filler words and false starts kept |
| `translit` | Indic speech transliterated to Latin script |

`transcribe` and `codemix` are the pair the sample gate weighs against each other, because
for code-mixed speech that is a genuine judgement call. Pinning any of the other three is
an instruction rather than a question, so the gate checks one sample instead of comparing
two — and says so when you pick one. All five belong to `saaras:v3`; `saarika:v2.5` takes
no mode, and `mode` is then omitted from the request rather than sent and rejected.

### What it will not do

Speaker labels stay generic (`SPEAKER_00`) and are never presented as people. An action
item only carries an owner when the transcript assigned it outright: an owner paired
with anything weaker is dropped and the task marked `suggested`, so an inference never
reads as a commitment somebody made. Every note states that the audio remains the
source of truth.

A failure never costs the recording. Anything that goes wrong after `stop` rolls back
to "recorded" and tells you to run `nbmeet process`; a Groq failure still delivers the
transcript without a summary.

And a failure never costs the same money twice. The moment the words come back they are
written to disk — as the two transcript files, and as a `resume-transcript.json` record
next to them. `nbmeet process` finds that record and goes straight to the summary, so a
run interrupted between transcription and notes is finished for free rather than
re-uploading audio Sarvam has already been paid to transcribe. The record is deleted once
the four deliverables exist, so it is only ever on disk while a meeting is half-finished;
`nbmeet status` and `nbmeet history` both say when one is waiting. To throw it away and
transcribe from the audio again — a new job, and a new bill:

```bash
nbmeet process --fresh
```

## Layout

```
crates/nextbase-core     config, storage, log, shortcuts, providers, audio,
                         hotkeys, paste, autostart, process state, updater,
                         Sarvam Batch, WAV slicing, meeting capture
crates/nextbase-meeting  meeting state machine, recorder, sample gate,
                         Groq analysis, deliverables
crates/nextbase-cli      clap surface, setup wizards, listener, dashboard,
                         three binaries
```

`nextbase-core` holds anything platform-shaped or shared; meeting *policy* lives in
`nextbase-meeting` so Wisper's build does not carry it.

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
- `ratatui` drives the two genuinely live moments, both as an **inline viewport** so
  scrollback survives: press-to-capture for shortcuts and a mic level meter while
  recording. A full-screen `wisper dash` would be the next candidate.

  **Shortcut capture reads the CGEventTap, not stdin.** A terminal cannot see a bare
  modifier press unless it implements the kitty keyboard protocol, and Apple Terminal
  does not — which is why modifier-only combos like `Ctrl+Command` appeared to do
  nothing when capture read stdin. The tap sees them in any terminal. Each key shows
  up as it goes down (`Ctrl`, then `Ctrl + Command`), and releasing modifiers together
  without pressing a key captures the combo on its own. Key presses are swallowed
  during capture so a stray `Cmd+Q` cannot act on the focused app. Without
  Accessibility permission it falls back to reading stdin, and Esc always drops to
  typing.

  The meter is scaled to roughly -60..0 dB, because speech sits around 0.01-0.3 and a
  linear bar looks dead.

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
3. **Verify Windows.** The hotkey, paste, autostart and WASAPI loopback code cannot be
   compiled from macOS: `rustls` pulls in `ring`, whose build script needs a Windows C
   toolchain. The CI job on `windows-latest` is the only check any of it gets, and
   loopback capture has never actually been run.
4. **Add signing secrets.** `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`,
   `MACOS_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`.
   Unsigned binaries hit Gatekeeper, and re-signing with a different identity
   invalidates the Accessibility grant from step 1.
5. **Live provider smoke test.** Nothing here has made a real transcription API call
   yet — including the meeting sample gate and full run, which need a Sarvam key that
   is not on the development machine. Everything up to the upload is verified; the
   Batch lifecycle is covered only by fixtures.

## Known gaps

- `wisper update` cannot upgrade *to* a release built before v0.1.5, because the
  bare per-platform binaries it downloads were not published until then. Older
  installs need one more `curl`/`iwr` run to reach v0.1.5, after which `wisper
  update` works.
- Meeting Agent needs macOS 13+ for system audio: ScreenCaptureKit is the only way to
  capture it without making the user install a virtual audio device. Linking its Swift
  bridge also means the binaries carry an rpath to `/usr/lib/swift`.
- A meeting over 40 hours cannot be transcribed in one job (2 hours per file × 20
  files). It is refused with that explanation rather than submitted and failed.
- Recording uses the device's native sample rate (48 kHz here) rather than SoX's
  fixed 16 kHz. Providers resample server side, so this only costs upload size —
  roughly 3× — and a `rubato` resampler would remove it.
- Linux has autostart but no hotkeys or paste, same as the TypeScript build.
- The inline viewport needs a terminal that answers a cursor-position query. Where
  that fails, capture says so and falls back to typing — verified by driving it
  through a bare pty. The drawing itself is covered by `TestBackend` tests, but its
  on-screen layout is only confirmed by running it in a real terminal.
- Live capture of modifier-only combos is macOS-only, since it depends on the event
  tap. On Windows, type them: `wisper shortcut Ctrl+Win`.

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets   # CI runs with -D warnings
cargo fmt --all --check
```

CI uses `stable`, which can be ahead of your local toolchain and therefore knows
lints yours does not — that is how the first CI run failed while local clippy was
clean. Run `rustup update` before pushing.

### CI cost

Windows runners are roughly 5x slower per step than macOS, so the shape of CI
matters there. Measured: the first green run took 15.7 min, of which
`cargo build --release` was 449s on its own and clippy/test ran serially for
another 350s. Restructuring — release build only on tags, clippy and test as
concurrent jobs with separate cache keys, no debug info, `fmt` on Linux — brought
it to 5.2 min.

The largest remaining lever is `ring`, pulled in by `rustls` for TLS. Its build
script needs a C toolchain, which is both slow on Windows and the reason the
Windows target cannot be checked from macOS. Switching `reqwest` to `native-tls`
would remove it and use SChannel on Windows and Security.framework on macOS —
which also means the OS trust store, so corporate TLS interception starts working.
The cost is that Linux builds would then need OpenSSL.

Test against a sandboxed config instead of your own — `HOME` is honoured
everywhere, exactly as Node's `os.homedir()` does:

```bash
HOME=/tmp/wisper-sandbox cargo run --bin wisper -- status
```

`wisper doctor` is the fastest way to see permission, device, shortcut, provider,
and listener state at once. `wisper record 3` tells a permission problem apart from
a device problem.
