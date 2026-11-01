# Rebuilds and restarts the receiver, keeping the exe's path so the firewall rule and shortcuts stay good.
$exe = Join-Path $PSScriptRoot 'target\release\AirPlayReceiver.exe'
$old = "$exe.old"
Remove-Item $old -ErrorAction SilentlyContinue
if (Test-Path $exe) { Move-Item $exe $old }   # Windows lets a running exe be renamed, not replaced
cargo build --release --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) {
    Move-Item $old $exe
    throw 'Build failed; the old version is still in place.'
}
Get-Process AirPlayReceiver -ErrorAction SilentlyContinue | Stop-Process -Force   # closing the window only hides it
Start-Sleep -Milliseconds 300
Remove-Item $old -ErrorAction SilentlyContinue
Start-Process $exe
