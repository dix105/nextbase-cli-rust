# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo test --all                            # unit tests, the main gate
cargo clippy --all-targets -- -D warnings   # CI treats warnings as errors
cargo fmt --all --check
cargo build --release                       # target/release/{wisper,nextbase,nbmeet}
```

There is no integration harness. Verification is those three plus running the built
binaries — most behaviour here is device, permission and process handling that a unit
test cannot reach.

## Three binaries, two tools, three crates

- `nextbase-core` — anything platform-shaped or shared: config, paths, logging,
  shortcuts, hotkeys, paste, autostart, process state, the updater, the four
  transcription providers, Sarvam Batch, WAV slicing, and meeting capture.
- `nextbase-meeting` — meeting *policy*: state machine, recorder worker, sample gate,
  Groq analysis, deliverables. Separate so Wisper's build does not carry it.
- `nextbase-cli` — clap surfaces and the binaries `wisper`, `nextbase`, `nbmeet`.

`nextbase` dispatches `Tool::Wisper` / `Tool::Meeting`; the other two binaries are
direct entry points into the same code.

## State lives outside the repo

- `~/.wisper-cli/` — `config.json` (**shared by both tools, including API keys**),
  `history.json`, `wisper.log`, `listener.pid`
- `~/.nextbase/` — `active-meeting.json` and `meetings/<id>/` holding `audio.wav` and
  the deliverables. Separate because these are files a person opens.

`Config` carries `#[serde(flatten)] extra`, so writing config never drops a key this
build does not know about. Always go through `config::load` / `config::update`.

## Things that will bite you

**Two processes, two lifecycles.** `wisper listen` detach-spawns `_listen`; `nbmeet
start` detach-spawns `_record <id>`. Both go through `autostart::spawn_detached_with`,
which applies `setsid` on unix and `DETACHED_PROCESS` on Windows — without the latter
the child inherits the terminal's console and dies when the window closes.

**Meeting stop is a state-file transition, not a signal.** The recorder polls
`active-meeting.json` for `stopping`. Do not "simplify" this to a kill: on Windows
`process_state` uses `TerminateProcess`, which gives the recorder no chance to finalize
its WAV, and every Windows recording would end up with a zero-length header.

**The listener reads config once at startup.** Any command that changes
listener-relevant config must `stopListener()` then restart, or at minimum print the
`wisper restart` hint. Existing commands do; keep new ones consistent.

**macOS needs a Swift toolchain.** `screencapturekit` compiles a Swift bridge.
`nextbase-core/build.rs` finds the compatibility archives (they are *not* where the
dependency looks when only Command Line Tools are installed), and both
`nextbase-cli/build.rs` and `.cargo/config.toml` add an rpath to `/usr/lib/swift` —
without it every binary that links core aborts before `main`.

**Windows code cannot be compiled here.** `rustls` pulls in `ring`, whose build script
needs a Windows C toolchain. The `windows-latest` CI job is the only check the hotkey,
paste, autostart and WASAPI loopback paths get. Expect a round trip; keep changes to
those files small and reviewable.

**`master` is not the release channel; tags are.** `release.yml` builds on `v*`, and
`wisper update` downloads the *bare* per-platform binaries it publishes (the archives
are for the installers). Releases are created as drafts and must be published before
`update` can see them. All three binaries are replaced together — `updater::BINARIES`.

## Sarvam Batch limits worth remembering

2 hours per file, 20 files per job, chunk-level timestamps only. `mode` is a per-job
parameter, which is why the sample gate runs two jobs rather than one. The **output
JSON schema is undocumented**, so `sarvam_batch::parse_output` accepts several field
spellings and returns empty data rather than erroring — losing an already-paid-for job
to a schema change would be worse.

## Conventions

- Errors are user-facing and action-oriented (`"... Run: nbmeet setup"`); both `main`
  functions print `error.message` only.
- Platform workarounds carry a short "why" comment naming the failure mode they
  defend against. Keep them when editing those paths.
- Meeting output must never overstate what is known: speaker labels stay generic and
  are never presented as people, an action item only carries an owner when the
  transcript assigned it outright, and nothing reports an accuracy percentage —
  language detection is not accuracy. Tests pin all three.
