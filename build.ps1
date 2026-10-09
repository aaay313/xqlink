# Build xqlink (Chinese-chess assistant) on Windows WITHOUT administrator rights.
#
# Why GNU toolchain: installing MSVC Build Tools requires admin, which is not
# available on this machine, so we use x86_64-pc-windows-gnu + w64devkit instead.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File D:\workbuuudy\chessboard\build.ps1
#
# Prerequisites (already provisioned on this machine):
#   1. Rust via rustup, user-level, host = x86_64-pc-windows-gnu
#        C:\Users\14746\.rustup   and   C:\Users\14746\.cargo\bin
#   2. w64devkit (gcc / windres / ld)
#        D:\workbuuudy\tools\w64devkit\w64devkit\bin
#   3. frontend deps installed:  cd chessboard; npm install --ignore-scripts
#      plus the two platform packages:
#        npm i --no-save @esbuild/win32-x64@0.25.12 @rollup/rollup-win32-x64-msvc
#
# IMPORTANT: run this in PowerShell, NOT Git Bash.
#   In Git Bash rustc/cargo silently produce no output and no artifacts on this box.

$ErrorActionPreference = "Stop"

$repo     = "D:\workbuuudy\chessboard"
$mingw    = "D:\workbuuudy\tools\w64devkit\w64devkit\bin"
$toolchain= "C:\Users\14746\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin"
$cargobin = "C:\Users\14746\.cargo\bin"

$env:Path = "$mingw;$toolchain;$cargobin;" + $env:Path
$env:CARGO_BUILD_JOBS = "6"

Set-Location $repo

Write-Host "=== 1/3 toolchain ===" -ForegroundColor Cyan
& rustc --version
& gcc --version | Select-Object -First 1
"windres: " + (Get-Command windres -ErrorAction SilentlyContinue).Source

Write-Host "=== 2/3 frontend (vue-tsc + vite) -> dist/ ===" -ForegroundColor Cyan
& npm run build
if ($LASTEXITCODE -ne 0) { throw "frontend build failed" }

Write-Host "=== 3/3 cargo release build (slow: lto=true, codegen-units=1) ===" -ForegroundColor Cyan
& "$repo\node_modules\.bin\tauri.cmd" build --config server/tauri.windows.cpu.build.json --no-bundle
if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }

$exe = "$repo\server\target\release\xqlink.exe"
if (Test-Path $exe) {
    $f = Get-Item $exe
    Write-Host ("OK  {0}  {1:N0} bytes" -f $f.FullName, $f.Length) -ForegroundColor Green
} else {
    throw "xqlink.exe not found"
}
