# Verify the bundled python payload runs with no system Python involved.
# Usage: verify-bundled.ps1 -Root <dir-containing-python,site-packages,backend>
param([Parameter(Mandatory=$true)][string]$Root)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path $Root).Path

$py = Join-Path $Root "python\python.exe"
if (-not (Test-Path $py)) {
    Write-Error "no bundled interpreter found at $py"
    exit 1
}

$env:PYTHONPATH = Join-Path $Root "site-packages"
$env:PYTHONNOUSERSITE = "1"
$out = & $py (Join-Path $Root "backend\app.py") --self-test 2>&1 | Out-String
Write-Output $out

$normalized = $Root -replace '\\','/'
if ($out -notmatch [regex]::Escape('"executable": "') -or ($out -notmatch [regex]::Escape($Root) -and $out -notmatch [regex]::Escape($normalized))) {
    Write-Error "FAIL: executable path is not inside the bundle ($Root)"
    exit 1
}
if ($out -notmatch '"numpy":') {
    Write-Error "FAIL: numpy did not import in the bundled env"
    exit 1
}
Write-Output "PASS: bundled python + numpy verified at $Root"
