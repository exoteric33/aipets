# Builds aipets.exe with Rust (cargo). Needs the Rust toolchain: https://rustup.rs (default settings).
#   .\build.ps1                    compile (sprites come from pets\<id>\sprites\)
#   .\build.ps1 -OutputDirectory C:\Temp\aipets-build   isolated build, never stops or starts aipets
# A running aipets is stopped only after compilation succeeds and restarted after replacement.
param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$isolated = -not [string]::IsNullOrWhiteSpace($OutputDirectory)
if ($isolated) {
    $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    if ($OutputDirectory.TrimEnd('\') -eq $root.TrimEnd('\')) { throw 'OutputDirectory must differ from the source folder' }
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
} else { $OutputDirectory = $root }
$exe = Join-Path $OutputDirectory 'aipets.exe'

$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) {
    # rustup installs here; a shell opened before the installation does not know the path yet
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (-not (Test-Path $cargo)) { throw 'Rust was not found. Install it from https://rustup.rs (default settings), then run this again.' }
}
$candidate = Join-Path $OutputDirectory ('aipets-build-' + [guid]::NewGuid().ToString('N') + '.exe')
$running = @()
try {
    # cargo reports its progress on stderr: keep it from turning into a PowerShell error
    $ErrorActionPreference = 'Continue'
    & $cargo build --release --locked --manifest-path "$root\Cargo.toml" --target-dir "$root\target"
    $built = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($built -ne 0) { throw 'cargo build failed; the installed executable was not changed' }
    Copy-Item -LiteralPath "$root\target\release\aipets.exe" -Destination $candidate
    if (-not $isolated) {
        # Stop the installed app only after a successful compilation (also the pet processes of versions before 0.2).
        $running = @(Get-Process aipets -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
    # [NullString]::Value: PowerShell would pass $null as "" (invalid backup path)
    if ([IO.File]::Exists($exe)) { [IO.File]::Replace($candidate, $exe, [NullString]::Value) }
    else { [IO.File]::Move($candidate, $exe) }
    Write-Host "built $exe"
} finally {
    if (Test-Path -LiteralPath $candidate) { Remove-Item -LiteralPath $candidate }
    if ($running.Count -gt 0 -and (Test-Path -LiteralPath $exe)) {
        # Through explorer, so the tray does not inherit this shell's environment.
        Start-Process explorer.exe "`"$exe`"" -WindowStyle Hidden
        Write-Host 'aipets restarted'
    }
}
