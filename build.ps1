# build.ps1 — Windows build helper for Cortex.
#
# Linux and macOS: plain `cargo build --release` is all you need.
# Windows: plain cargo works with the default MSVC toolchain (Visual Studio Build
# Tools). Without Visual Studio, install a portable MinGW-w64 (WinLibs UCRT) and
# this script uses the GNU toolchain with it:
#   - MinGW is looked up in $env:CORTEX_MINGW, ~\mingw64-portable\mingw64\bin,
#     C:\mingw-rust\mingw64\bin;
#   - rustup toolchain stable-x86_64-pc-windows-gnu (installed on first use).
#
# Usage: .\build.ps1                 debug build
#        .\build.ps1 release         release build (LTO)
#        .\build.ps1 test            cargo test
#        .\build.ps1 run -- index .  build + run
#        .\build.ps1 <cargo args>    anything else is passed to cargo
param([Parameter(ValueFromRemainingArguments=$true)] $Args)

$candidates = @($env:CORTEX_MINGW, "$env:USERPROFILE\mingw64-portable\mingw64\bin", "C:\mingw-rust\mingw64\bin") | Where-Object { $_ }
$mingw = $candidates | Where-Object { Test-Path "$_\gcc.exe" } | Select-Object -First 1
if ($mingw) {
  $env:PATH = "$mingw;$env:PATH"
  if (-not $env:RUSTUP_TOOLCHAIN) { $env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu" }
  Write-Host "[build] GNU toolchain + MinGW ($mingw)"
} else {
  Write-Host "[build] MinGW not found: using the default Rust toolchain (MSVC on Windows)"
}

if ($Args.Count -eq 0) { cargo build }
elseif ($Args[0] -eq "release") { cargo build --release }
elseif ($Args[0] -eq "run") { cargo run @($Args[1..($Args.Count-1)]) }
elseif ($Args[0] -eq "test") { cargo test }
else { cargo @Args }
exit $LASTEXITCODE
