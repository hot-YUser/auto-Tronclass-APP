# auto-tronclass-rollcall-answer

Cross-platform TronClass client (rollcall + LLM-assisted answering). Rust core (all domain
logic) + .NET MAUI dumb view, joined by one in-process FFI seam. See `docs/` for the spec and
`AGENTS.md` for the ground rules. Licensed AGPL-3.0-or-later.

## Status: walking skeleton (build-order step 0)

One button → the Rust core does a real async login round-trip → the UI shows the result. Its
only job is to prove the three riskiest things once, on the real architecture:

1. **async over FFI** — C# `await`/Task ↔ Rust tokio future
2. **event callback (reverse channel)** — the core pushes unsolicited events + a heartbeat up to the UI
3. **the platform keeps the process alive** — desktop naturally; Android via a foreground service

All three are runtime-proven (see below), on .NET **MAUI 11** preview. No domain features yet
(rollcall, answering, QR, providers, vault, login feature-routing are later slices). The skeleton
logs into an in-repo **fake** TronClass, so it needs zero credentials and names no school.

## Layout

- `core/` — Rust `cdylib` (`tronclass_core`). FFI surface in `src/lib.rs` (`core_init/send/free`
  + one event callback), heartbeat + login in `src/engine.rs`/`src/login.rs`, wire schema in
  `src/protocol.rs`, the fake server in `src/fake.rs`. `build.rs` runs csbindgen →
  `ui/Interop/NativeMethods.g.cs`.
- `ui/` — .NET MAUI app (net11.0 android + windows). `Interop/Core.cs` is the whole C# side of
  the seam; `MainPage` is the dumb view; `Platforms/Android/CoreForegroundService.cs` is the
  keepalive service.
- `smoke/` — headless C# console that P/Invokes the core against the fake server (CI-able proof
  of the marshalling without a GUI).
- `build-core.ps1` — builds the native core (Windows dll / both Android ABIs).
- `package-windows.ps1` — Windows distribution: portable exe and/or signed MSIX.

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

# 3. Windows app — portable
pwsh ./build-core.ps1                        # builds tronclass_core.dll
dotnet build ui/Ui.csproj -f net11.0-windows10.0.19041.0
#   run it, keep the fake server up, press Login: ticks stream, state → logging_in, result shows,
#   the button never blocks.
```

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
pwsh ./build-core.ps1 -Head android          # cargo-ndk → core/jniLibs/{x86_64,arm64-v8a}/libtronclass_core.so
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

Verified on an API-36 x86_64 emulator: the heartbeat keeps ticking (1/s) while backgrounded **and**
under forced deep Doze, same pid throughout — the foreground service holds the process. Use `10.0.2.2`
as the base_url host from an emulator to reach the fake server on your machine.

> Honesty ceiling: emulator + forced Doze proves the *mechanism*. Real all-day survival on a
> physical device under OEM battery-killers is not proven here — and API 35+ caps `dataSync`
> foreground services to ~6h/day, a real limit to face when slice 7 adds all-day background.
