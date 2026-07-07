#!/usr/bin/env pwsh
# Builds the Rust core and places artifacts where Ui.csproj expects them.
#   ./build-core.ps1                 → Windows: core/target/release/tronclass_core.dll
#   ./build-core.ps1 -Head android   → Android: core/jniLibs/arm64-v8a/libtronclass_core.so
#
# Android prerequisites (one-time):
#   rustup target add aarch64-linux-android
#   cargo install cargo-ndk
#   an installed Android NDK (sdkmanager "ndk;27.2.12479018"), ANDROID_NDK_HOME pointing at it
param(
    [ValidateSet("windows", "android")] [string]$Head = "windows"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$core = Join-Path $root "core"

if ($Head -eq "windows") {
    cargo build --manifest-path "$core/Cargo.toml" --release
    Write-Host "windows: core/target/release/tronclass_core.dll ready"
}
else {
    Push-Location $core
    try {
        # Point cargo-ndk at the NDK if the env var isn't already set (newest installed NDK).
        if (-not $env:ANDROID_NDK_HOME) {
            $ndkBase = Join-Path $env:LOCALAPPDATA "Android\sdk\ndk"
            $ndk = Get-ChildItem $ndkBase -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
            if ($ndk) { $env:ANDROID_NDK_HOME = $ndk.FullName }
        }
        # Both ABIs: x86_64 (emulator) + arm64-v8a (device). cargo-ndk writes
        # jniLibs/<abi>/libtronclass_core.so — the layout AndroidNativeLibrary wants.
        cargo ndk -t x86_64 -t arm64-v8a -o jniLibs build --release
        Write-Host "android: core/jniLibs/{x86_64,arm64-v8a}/libtronclass_core.so ready"
    }
    finally { Pop-Location }
}
