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

# JSON escapes backslashes, so sys.executable prints like "C:\\...\\python.exe".
# Normalize both sides to forward slashes before comparing.
$rootNorm = ($Root -replace '\\\\','/') -replace '\\','/'
$outNorm  = ($out  -replace '\\\\','/') -replace '\\','/'
if ($outNorm -notmatch [regex]::Escape('"executable": "' + $rootNorm)) {
    Write-Error "FAIL: executable path is not inside the bundle ($rootNorm)"
    exit 1
}
if ($out -notmatch '"numpy":') {
    Write-Error "FAIL: numpy did not import in the bundled env"
    exit 1
}
Write-Output "PASS: bundled python + numpy verified at $Root"
