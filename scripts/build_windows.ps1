# ──────────────────────────────────────────────────────────────────────
# OneAI Windows build script
#
# Builds oneai.dll (cdylib) for x86_64-pc-windows-msvc and stages it for the
# Windows app (platforms/windows), which P/Invokes it from C#.
#
# Run on a Windows machine with Visual Studio (MSVC) + the rust target:
#   rustup target add x86_64-pc-windows-msvc
#   pwsh ./scripts/build_windows.ps1            # release
#   pwsh ./scripts/build_windows.ps1 -Debug
#
# NOTE: uniffi-bindgen 0.32 has NO C# generator (only kotlin/swift/python/
# ruby). The C# binding is therefore NOT auto-generated here. Phase 3 picks
# one of: (a) uniffi-bindgen-cs third-party generator, or (b) a hand-rolled
# `extern "C"` JSON facade P/Invoked from C#. See bindings/csharp/README.md.
# This script only builds the native oneai.dll that whichever route consumes.
# ──────────────────────────────────────────────────────────────────────
[CmdletBinding()]
param(
  [switch]$Debug
)

$ErrorActionPreference = "Stop"
$Root   = Resolve-Path (Join-Path $PSScriptRoot "..")
$WinDir = Join-Path $Root "platforms/windows"
$Triple = "x86_64-pc-windows-msvc"
$Profile = if ($Debug) { "debug" } else { "release" }

Write-Host "-- Building oneai.dll for $Triple [$Profile]"
cargo build "--$Profile" -p oneai-uniffi --target $Triple

$Src = Join-Path $Root "target/$Triple/$Profile"
$Dll = Join-Path $Src "oneai.dll"
if (-not (Test-Path $Dll)) {
  $Lib = Join-Path $Src "oneai.dll.lib"   # import lib, if any
  if (-not (Test-Path $Lib)) { Throw "oneai.dll not found at $Src" }
}

$Out = Join-Path $WinDir "native"
New-Item -ItemType Directory -Force -Path $Out | Out-Null
Copy-Item -Force (Join-Path $Src "oneai.dll") $Out
Write-Host "-- Staged oneai.dll -> $Out"

Write-Host ""
Write-Host "-- Done. Open platforms/windows/OneAI.sln in Visual Studio to build the app."
