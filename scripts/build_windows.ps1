# ──────────────────────────────────────────────────────────────────────
# OneAI Windows build script
#
# Builds oneai.dll (cdylib) for x86_64-pc-windows-msvc and stages it for the
# Windows app (platforms/windows), which P/Invokes it from C#.
#
# Run on a Windows machine with Visual Studio (MSVC) + the rust target:
#   rustup target add x86_64-pc-windows-msvc
#   pwsh ./scripts/build_windows.ps1            # release
#   pwsh ./scripts/build_windows.ps1 -DebugBuild
#
# NOTE: uniffi-bindgen 0.32 has NO C# generator (only kotlin/swift/python/
# ruby). The C# binding is therefore NOT auto-generated here. The app uses a
# hand-rolled `extern "C"` JSON facade (crates/oneai-uniffi/src/c_facade.rs)
# P/Invoked from C#. See bindings/csharp/README.md + platforms/windows/README.
# This script only builds the native oneai.dll that the C# app consumes.
# ──────────────────────────────────────────────────────────────────────
[CmdletBinding()]
param(
  # NOTE: do NOT name this `Debug` — `[CmdletBinding()]` already provides an
  # implicit `-Debug` common-parameter switch, so re-declaring `param([switch]$Debug)`
  # throws "ParameterNameAlreadyExistsForCommand". Use a distinct name.
  [switch]$DebugBuild
)

$ErrorActionPreference = "Stop"
$Root    = Resolve-Path (Join-Path $PSScriptRoot "..")
$WinDir  = Join-Path $Root "platforms/windows"
$Triple  = "x86_64-pc-windows-msvc"
# NOTE: do NOT name this `$Profile` — PowerShell variables are case-insensitive
# and `$PROFILE` is a read-only automatic variable (the profile-script path).
# Assigning to it throws "Cannot overwrite variable Profile because it is
# read-only or a constant", which (with $ErrorActionPreference=Stop) aborts
# the script before cargo even runs. Hence the explicit `$BuildProfile` name.
$BuildProfile = if ($DebugBuild) { "debug" } else { "release" }

# --- prerequisites -----------------------------------------------------
$ErrorActionPreference = "Stop"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  Throw "cargo not found on PATH. Install Rust (https://rustup.rs) and ensure the MSVC toolchain (Visual Studio with the 'Desktop development with C++' workload) is installed."
}
$installed = & rustup target list --installed 2>$null
if (-not ($installed -contains $Triple)) {
  Throw "Rust target $Triple is not installed. Run: rustup target add $Triple"
}

# --- build -------------------------------------------------------------
Write-Host "-- Building oneai.dll for $Triple [$BuildProfile]"
cargo build "--$BuildProfile" -p oneai-uniffi --target $Triple
if ($LASTEXITCODE -ne 0) { Throw "cargo build failed (exit $LASTEXITCODE)" }

# --- stage -------------------------------------------------------------
# On MSVC a cdylib named `oneai` emits oneai.dll (+ oneai.dll.lib import lib).
$Src = Join-Path $Root "target/$Triple/$BuildProfile"
$Dll = Join-Path $Src "oneai.dll"
if (-not (Test-Path -LiteralPath $Dll)) {
  $Lib = Join-Path $Src "oneai.dll.lib"   # import lib, present if any export
  if (-not (Test-Path -LiteralPath $Lib)) {
    Throw "oneai.dll not found at $Src — the build produced no artifact. Check cargo output above."
  }
  # Import lib exists but the dll itself doesn't — this shouldn't happen for a
  # cdylib, but guard the copy below rather than failing with a confusing msg.
  Throw "oneai.dll missing (only the import lib $Lib was produced). Re-run the build."
}

$Out = Join-Path $WinDir "native"
New-Item -ItemType Directory -Force -Path $Out | Out-Null
# Stage as oneai_native.dll — NOT oneai.dll. The crate emits oneai.dll, but the
# C# app's managed assembly is also OneAI.dll; on case-insensitive NTFS those
# collide in the output dir and the managed one clobbers the native, breaking
# every P/Invoke (the OneAiNative DllName is "oneai_native" to match).
$Dst = Join-Path $Out "oneai_native.dll"
Copy-Item -Force -LiteralPath $Dll $Dst
Write-Host "-- Staged oneai.dll -> $Dst (renamed to avoid NTFS case collision with OneAI.dll)"

Write-Host ""
Write-Host "-- Done. Open platforms/windows/OneAI.sln in Visual Studio to build the app,"
Write-Host "   or:  dotnet build platforms\windows\OneAI.sln -c Debug"
