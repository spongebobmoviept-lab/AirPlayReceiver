# Builds the exe for publishing: standalone, with this PC's folder names stripped out of it.
# The result is target\publish\release\AirPlayReceiver.exe.
$registry = (Get-ChildItem "$HOME\.cargo\registry\src\*" -Directory | Select-Object -First 1).FullName
$env:RUSTFLAGS = "-C target-feature=+crt-static --remap-path-prefix=$registry=crates --remap-path-prefix=$PSScriptRoot=. --remap-path-prefix=$HOME\.rustup=rustup"
$env:CARGO_TARGET_DIR = Join-Path $PSScriptRoot 'target\publish'
cargo build --release --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml')
Remove-Item Env:RUSTFLAGS, Env:CARGO_TARGET_DIR
