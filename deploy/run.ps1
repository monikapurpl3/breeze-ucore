# run.ps1 -- run the Breeze uCore deployer on Windows, and leave no program behind.
#
#   irm https://aspic.salataputarica.hr.eu.org/ucore-deploy/run.ps1 | iex
#
# What it does, and nothing more:
#   * downloads the deployer helper into a new folder in your temporary directory;
#   * checks it against SHA256SUMS, and runs nothing that does not match;
#   * runs it in this window: it opens its page in your browser, and keeps
#     going until you press Ctrl-C;
#   * deletes the folder again, whatever happened.
#
# The helper keeps your deployments, and the build tools it installs, in a
# folder of its own (%LOCALAPPDATA%\ucore-deploy), so they are there next
# time. Nothing else is installed: no PATH change, no profile, no registry.
#
# Plain ASCII: Windows PowerShell 5.1 reads a file without a BOM as ANSI.
#
# Copyright 2026 Monika. AGPL-3.0-or-later; see LICENSE.

# A child scope: under `irm | iex` this runs in the caller's own session,
# which must not be left holding its variables, or be closed by an `exit`.
& {
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Base = if ($env:UCORE_DEPLOY_BASE) { $env:UCORE_DEPLOY_BASE.TrimEnd('/') } else { 'https://aspic.salataputarica.hr.eu.org/ucore-deploy' }

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    Write-Host "ucore-deploy: there is no deployer for Windows on $($env:PROCESSOR_ARCHITECTURE) (Espressif's tools need x64)"
    $global:LASTEXITCODE = 1
    return
}
$Me = 'windows-x86_64'
# Windows PowerShell 5.1 may not offer TLS 1.2 by default.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

$Dir = Join-Path ([IO.Path]::GetTempPath()) ('ucore-deploy-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Dir | Out-Null
try {
    $Sums = Join-Path $Dir 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile $Sums
    # "HASH  ucore-deploy-VERSION-LABEL.exe": this machine's line. One
    # version is served at a time.
    $Pattern = '^([0-9a-f]{64})  (ucore-deploy-([0-9][0-9.]*)-' + [regex]::Escape($Me) + '\.exe)$'
    $Best = $null
    foreach ($Line in (Get-Content -LiteralPath $Sums)) {
        $M = [regex]::Match($Line.Trim(), $Pattern)
        if ($M.Success) { $Best = $M }
    }
    if ($null -eq $Best) { throw "$Base has no deployer for $Me" }
    $Exe = Join-Path $Dir 'ucore-deploy.exe'
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/$($Best.Groups[2].Value)" -OutFile $Exe
    $Got = (Get-FileHash -Algorithm SHA256 -LiteralPath $Exe).Hash.ToLowerInvariant()
    if ($Got -ne $Best.Groups[1].Value) { throw "$($Best.Groups[2].Value) does not match its SHA-256 in SHA256SUMS; nothing was run" }
    Write-Host "The uCore deployer $($Best.Groups[3].Value) for $Me, checked against its SHA-256. Ctrl-C stops it; the program is deleted then."
    # Its messages on stderr are its own, not errors: Windows PowerShell 5.1
    # would otherwise throw on the first one whenever stderr is redirected.
    $ErrorActionPreference = 'Continue'
    & $Exe @args
    $global:LASTEXITCODE = $LASTEXITCODE
} catch {
    Write-Host "ucore-deploy: $($_.Exception.Message)"
    $global:LASTEXITCODE = 1
} finally {
    Remove-Item -LiteralPath $Dir -Recurse -Force -ErrorAction SilentlyContinue
}
} @args
