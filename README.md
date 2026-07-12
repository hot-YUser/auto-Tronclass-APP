# auto-tronclass-rollcall-answer

Cross-platform TronClass client (rollcall + LLM-assisted answering). Rust core (all domain
logic) + .NET MAUI dumb view, joined by one in-process FFI seam. See `docs/` for the spec and
`AGENTS.md` for the ground rules. Licensed AGPL-3.0-or-later.

## Status: slice 4 — core feature-complete (Phase A done)

Slices 0–3 are done (FFI seam on .NET **MAUI 11** preview; vault; registry; accounts; real login;
multi-account monitoring; four rollcall types; the whole auto-answer subsystem + LLM). Slice 4 is the
last stop of Phase A — filling the miscellaneous gaps that make the core feature-complete, still
**headless / command-driven** (UI paused until a later phase):

- **settings system** — every tuning knob is typed in `Settings`, runtime-patchable via `UpdateConfig`,
  persisted: `llm_max_tokens` (`0` → safe 16384), the radar strategy chain, number brute-force
  concurrency/cooldown, poll + quiz-detect cadences, `log_level`, and operating hours.
- **operating hours** (`Operating::is_open`, pure) — the monitor only polls inside enabled per-weekday
  windows (empty schedule = always-on); local time = UTC + a fixed `tz_offset_minutes` (default +8),
  zero new deps.
- **captcha login** (docs 30) — a captcha page is detected, its image shipped as a `CaptchaChallenge`
  event, and the user's typed answer (`SubmitCaptcha`) completes the login. **No OCR.** SSO / email-SPA
  pages route to the browser-cookie fallback (`ImportCookies`).
- **single redaction pass** (docs 90 §4) — every event crosses the seam through one audited
  `redaction::emit`; secrets (password/cookie/LLM key) never reach a log/event. Leveled logging
  (normal/debug).
- **vault unlock layer** (docs 10) — a `KeyStore` trait + in-memory stub; the vault can unlock from a
  stored key (`UnlockWithKeystore`) as well as the master password. Real Keychain/Keystore/DPAPI → Phase B.

### Slice 3 — auto-answer subsystem + LLM

- **pure decision layer** (`quiz.rs`) — per subject: a server-leaked answer → replay, else → LLM;
  group types flatten, `paragraph_desc` skips, fill/cloze/short send **verbatim (HTML included)**,
  matching uses member-validation. (docs 31 scoring gotchas.)
- **answer flow** (docs 20 flow A) — prepare (don't send) → **per-account conflict** check (never
  overwrite an existing answer; resolve via `SetAnswer{account_id,…}`) → core-owned 15 s countdown
  with submit-now / hold / discard → submit → resubmit-for-correct. LLM failure never submits blank.
- **per-source contracts** — exam / vote / courseware-quiz / classroom-exam (per-subject full
  wrapper) / homework / questionnaire, submit bodies exactly per docs 31.
- **LLM client** (`llm.rs`) — NVIDIA NIM + minimax default, reasoning always on, explicit
  `max_tokens`, reasoning streamed as `ReasoningChunk`, key from the vault. In a **merged** activity
  the LLM runs **once** (shared); each account keeps/overrides its own answers and submits for itself.

Quiz detection is a per-account × per-course fan-out. The monitor **actor never awaits network**
(prepare / LLM / submit all spawned). Development runs against an in-repo **fake** TronClass + a fake
LLM endpoint; no real NIM calls in tests.

## Layout

- `core/` — Rust `cdylib` (`tronclass_core`). FFI in `src/lib.rs` (`core_init/send/free` + one
  callback); state machine + dispatch in `src/engine.rs`; `src/providers.rs` (registry/endpoints),
  `src/config.rs` (metadata + typed settings + operating hours), `src/secrets.rs` (vault),
  `src/keystore.rs` (unlock layer), `src/redaction.rs` (single audited event/log chokepoint),
  `src/login.rs` (feature detection + captcha), `src/protocol.rs` (commands), `src/fake.rs` (fake
  server). `build.rs` runs csbindgen → `core/generated/NativeMethods.g.cs` (handed to the UI repo).
- **The UI is a separate sibling repo** — `../auto-tronclass-rollcall-answer-UI` (the .NET MAUI app).
  It consumes this core ONLY as a prebuilt binary (its `native/`); its `sync-core.ps1` pulls the
  dll/.so + FFI bindings from here. **No UI source lives in this repo** — the core is a black box.
- `smoke/` — headless C# console that drives the account+login flow over P/Invoke (CI-able, no GUI).
- `build-core.ps1` — builds the native core (Windows dll / all four Android ABIs).

## Prereqs

Rust; **.NET 11 preview SDK** + `dotnet workload install maui`. For Android also: an NDK, the
emulator + an x86_64 system image, `cargo install cargo-ndk`, and
`rustup target add x86_64-linux-android aarch64-linux-android`.

## Build & run

```sh
# 1. Offline seam test — proves all three risks + the negative case, no UI, no network
cargo test --manifest-path core/Cargo.toml

# 2. C# marshalling smoke — run the fake server, then the console harness
cargo run --manifest-path core/Cargo.toml --features fakeserver --bin fake_tronclass   # terminal A
dotnet run --project smoke -- http://127.0.0.1:8779                                      # terminal B → SEAM SMOKE PASS

# 3. The app UI lives in the sibling repo ../auto-tronclass-rollcall-answer-UI:
pwsh ./build-core.ps1                        # builds tronclass_core.dll here
#   then, in ../auto-tronclass-rollcall-answer-UI:
#     ./sync-core.ps1                         # pull the fresh dll/.so + bindings
#     dotnet build ui/Ui.csproj -f net11.0-windows10.0.19041.0
```

**Using it (the slice-1 flow):** launch → *Create vault* (set a master password) → *Add account*
(pick a school or type a `base_url`, e.g. the fake server `http://127.0.0.1:8779`, + username/password)
→ *Open dashboard* → *Login active account*. Point `base_url` at your own school and enter your own
credentials to log into it for real — they go into the vault, never the repo.
(Unpackaged Windows launch needs the WindowsAppSDK; build with `-p:WindowsAppSDKSelfContained=true`
if the runtime isn't installed machine-wide.)

### Windows distribution — portable and MSIX

```powershell
./package-windows.ps1 -Mode portable   # unpackaged exe → ui/bin/Release/.../win-x64/publish/
./package-windows.ps1 -Mode msix       # signed .msix → ui/AppPackages/  (+ tronclass-dev.cer)
./package-windows.ps1 -Mode both
```

The MSIX is signed with a self-signed dev cert (`CN=TronClass Dev`, created automatically; its
Subject must match the appxmanifest `Publisher`). **Before installing, trust the cert once** (needs
elevation):

```powershell
Import-Certificate -FilePath ui/AppPackages/tronclass-dev.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople
```

Then double-click `Ui_1.0.0.0_x64.msix`. Without the trust step the install fails with
`0x800B0109` (untrusted root). The portable build needs no install or cert.
(Future: an `.appinstaller` pointing at a GitHub URL gives auto-update — slice 7, `self_update`.)

### Android — runtime-proven keepalive

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
pwsh ./build-core.ps1 -Head android          # cargo-ndk → core/jniLibs/{arm64-v8a,armeabi-v7a,x86_64,x86}/libtronclass_core.so
# .NET 11 preview's android workload compiles against API 37, which isn't in the default SDK channel:
dotnet build ui/Ui.csproj -t:InstallAndroidDependencies -f net11.0-android -p:AcceptAndroidSDKLicenses=true
# Build a directly-installable APK (Debug uses fast-deployment, so embed assemblies for adb install):
dotnet build ui/Ui.csproj -f net11.0-android -c Debug -p:EmbedAssembliesIntoApk=true
adb install -r -g ui/bin/Debug/net11.0-android/com.tronclass.skeleton-Signed.apk
adb shell monkey -p com.tronclass.skeleton -c android.intent.category.LAUNCHER 1
adb shell input keyevent KEYCODE_HOME                # background it
adb shell dumpsys battery unplug && adb shell dumpsys deviceidle force-idle   # force Doze
adb logcat -s tronclass                              # heartbeat ticks keep coming → process held alive
```

The APK packages **all four ABIs** (arm64-v8a / armeabi-v7a / x86_64 / x86). .NET 11 preview's
CoreCLR-on-Android only ships 64-bit runtime packs, so the csproj sets `UseMonoRuntime=true` to
include the 32-bit ABIs; revisit at GA. Verified on an API-36 x86_64 emulator: the heartbeat keeps
ticking (1/s) while backgrounded **and** under forced deep Doze, same pid throughout — the foreground
service holds the process. Use `10.0.2.2` as the base_url host from an emulator to reach the fake server.

> Honesty ceiling: emulator + forced Doze proves the *mechanism*. Real all-day survival on a
> physical device under OEM battery-killers is not proven here — and API 35+ caps `dataSync`
> foreground services to ~6h/day, a real limit to face when slice 7 adds all-day background.
