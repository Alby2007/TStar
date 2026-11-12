#Requires -Version 5.1
<#
.SYNOPSIS
    Windows dogfood: install the shipped app and prove lifecycle
    containment -- clean close, hard kill, restart -- leaves no survivors.

.DESCRIPTION
    Full mode (local):
        powershell -File scripts\dogfood-windows.ps1 [-Version next]
    scaffolds a fresh app via `npm create pylair@<Version>` in a temp dir,
    npm-installs, uv-syncs, `npx pylair build`, silent-installs the NSIS
    bundle, runs the three lifecycle tests, then uninstalls and cleans up.

    Tests-only mode (CI -- app already installed by an earlier step):
        dogfood-windows.ps1 -ExePath C:\...\app-under-test.exe
    skips scaffold/build/install entirely. -AppDir <dir> instead reuses an
    existing scaffolded+built app but still does the install step.

    The Python runtime spawns eagerly at plugin setup (the supervisor
    actor's run_loop spawns the child immediately), so no UI interaction
    or env hook is needed to bring the runtime up -- the poll for the
    python child IS the positive control: if it never appears the test
    fails as invalid. Documented next to PYLAIR_SELF_TEST_WINDOW_CLOSE in
    the app template's lib.rs.

    Prerequisites for full mode: node/npm, uv, cargo/rust, and a Windows
    desktop session (the tests open a real window).
#>
[CmdletBinding()]
param(
    [string]$Version = 'next',
    [string]$WorkDir,
    [string]$AppName,
    [string]$AppDir,
    [string]$ExePath,
    [string]$InstallDir,
    [switch]$Keep,
    [int]$SpawnTimeoutSec = 90,
    [int]$ExitTimeoutSec = 30,
    [int]$SurvivorTimeoutSec = 15
)

$ErrorActionPreference = 'Stop'
$script:results = New-Object System.Collections.ArrayList

function Add-Result([string]$Test, [string]$Result, [string]$Detail) {
    [void]$script:results.Add([pscustomobject]@{
        Test = $Test; Result = $Result; Detail = $Detail
    })
    Write-Output ("[{0}] {1} -- {2}" -f $Result, $Test, $Detail)
}

function Wait-Until([scriptblock]$Cond, [int]$TimeoutSec, [int]$IntervalMs = 250) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (& $Cond) { return $true }
        Start-Sleep -Milliseconds $IntervalMs
    }
    return $false
}

# Every lifecycle check matches by executable path under the install
# dir -- never by command line or window title.
function Get-InstallDirProcs([string]$Dir) {
    Get-Process -ErrorAction SilentlyContinue | Where-Object {
        try {
            $_.Path -and $_.Path.StartsWith($Dir, [StringComparison]::OrdinalIgnoreCase)
        } catch { $false }
    }
}

function Get-PythonChild([string]$Dir) {
    Get-InstallDirProcs $Dir | Where-Object { $_.Name -like 'python*' }
}

function Invoke-Native([string]$Desc, [scriptblock]$Cmd) {
    & $Cmd
    if ($LASTEXITCODE -ne 0) { throw "$Desc failed with exit code $LASTEXITCODE" }
}

function Stop-InstallDirProcs([string]$Dir) {
    Get-InstallDirProcs $Dir | ForEach-Object {
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
}

function Wait-NoSurvivors([string]$Dir, [int]$TimeoutSec) {
    Wait-Until { @(Get-InstallDirProcs $Dir).Count -eq 0 } $TimeoutSec
}

function Find-InstalledExe([string]$Name) {
    $roots = @($env:LOCALAPPDATA, $env:ProgramFiles, ${env:ProgramFiles(x86)}) |
        Where-Object { $_ -and (Test-Path $_) }
    Get-ChildItem $roots -Recurse -Filter "$Name.exe" -Depth 4 `
        -ErrorAction SilentlyContinue | Select-Object -First 1
}

function Close-And-Wait($Proc, [int]$TimeoutSec) {
    # CloseMainWindow sends WM_CLOSE to the app's main window. Retry it --
    # the window handle can lag the process by a beat.
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $Proc.Refresh()
        if ($Proc.HasExited) { return $true }
        try { [void]$Proc.CloseMainWindow() } catch {}
        Start-Sleep -Milliseconds 500
    }
    $Proc.Refresh()
    return $Proc.HasExited
}

function Start-App([string]$Exe, [string]$Tag, [string]$LogDir) {
    $out = Join-Path $LogDir "$Tag.stdout.log"
    $err = Join-Path $LogDir "$Tag.stderr.log"
    Remove-Item $out, $err -Force -ErrorAction SilentlyContinue
    # Raw ProcessStartInfo, not Start-Process -Redirect*: the cmdlet's
    # redirect path leaves Process.ExitCode EMPTY on Windows PowerShell
    # 5.1 (measured). ReadToEndAsync drains the pipes on .NET threads so
    # the child never blocks — a PS scriptblock on a Task thread has no
    # runspace and throws on pwsh 7 (found by CI).
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    return @{ Proc = $p; Err = $err; Out = $out
              OutTask = $p.StandardOutput.ReadToEndAsync()
              ErrTask = $p.StandardError.ReadToEndAsync() }
}

# -- phase resolution ----------------------------------------------------

$ownInstall = $false          # did THIS script install the app?
$workDirCreated = $false
$logDir = $env:TEMP

try {
    if (-not $ExePath) {
        # -- 1. scaffold ----------------------------------------------
        if (-not $AppDir) {
            if (-not $WorkDir) {
                $WorkDir = Join-Path $env:TEMP ("pylair-dogfood-" +
                    [guid]::NewGuid().ToString('N').Substring(0, 8))
            }
            if (-not $AppName) {
                $AppName = 'dogfood-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
            }
            New-Item -ItemType Directory -Force $WorkDir | Out-Null
            $workDirCreated = $true
            Write-Output "scaffolding $AppName (pylair@$Version) in $WorkDir"
            Push-Location $WorkDir
            try {
                Invoke-Native 'npm create' {
                    npm --yes create "pylair@$Version" -- $AppName | Out-Host }
            } finally { Pop-Location }
            $AppDir = Join-Path $WorkDir $AppName
            if (-not (Test-Path $AppDir)) { throw "scaffold produced no $AppDir" }

            # -- 2. deps + build --------------------------------------
            Push-Location $AppDir
            try {
                Invoke-Native 'npm install' { npm install | Out-Host }
                Invoke-Native 'uv sync' { uv sync | Out-Host }
                Invoke-Native 'pylair build' { npx pylair build | Out-Host }
            } finally { Pop-Location }
        } else {
            if (-not $AppName) { $AppName = Split-Path $AppDir -Leaf }
            $WorkDir = Split-Path $AppDir -Parent
        }

        # -- 3. silent NSIS install -----------------------------------
        if (-not $AppName) { throw 'AppName unresolved -- pass -AppName or -AppDir' }
        $installer = Get-ChildItem `
            (Join-Path $AppDir 'src-tauri\target\release\bundle\nsis') `
            -Filter '*-setup.exe' -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if (-not $installer) {
            throw "no NSIS *-setup.exe under $AppDir\src-tauri\target\release\bundle\nsis"
        }
        $inst = Start-Process -FilePath $installer.FullName -ArgumentList '/S' `
            -Wait -PassThru
        if ($inst.ExitCode -ne 0) { throw "NSIS installer exit $($inst.ExitCode)" }
        $ownInstall = $true

        $exe = $null
        $found = Wait-Until {
            $script:exe = Find-InstalledExe $AppName; $null -ne $script:exe
        } 30 1000
        if (-not $found) { throw "installed $AppName.exe not found" }
        $ExePath = $exe.FullName
    }

    if (-not (Test-Path $ExePath)) { throw "ExePath not found: $ExePath" }
    if (-not $InstallDir) { $InstallDir = Split-Path $ExePath -Parent }
    # Path-scoped kills make a wrong InstallDir dangerous — refuse roots
    # and system dirs outright.
    $forbidden = @(
        [IO.Path]::GetPathRoot($InstallDir),
        $env:WINDIR, $env:ProgramFiles, ${env:ProgramFiles(x86)},
        $env:LOCALAPPDATA
    ) | Where-Object { $_ }
    $resolved = (Resolve-Path $InstallDir -ErrorAction SilentlyContinue)
    if (-not $resolved) { throw "InstallDir does not exist: $InstallDir" }
    $InstallDir = $resolved.ProviderPath
    foreach ($f in $forbidden) {
        if ($InstallDir -ieq $f) { throw "refusing unsafe InstallDir: $InstallDir" }
    }
    Write-Output "app: $ExePath`ninstall dir: $InstallDir"
    $logDir = if ($WorkDir) { $WorkDir } else { $env:TEMP }

    # Fresh start: nothing under the install dir may already be running.
    Stop-InstallDirProcs $InstallDir
    [void](Wait-NoSurvivors $InstallDir $SurvivorTimeoutSec)

    # -- TEST A: clean close ------------------------------------------
    try {
        $a = Start-App $ExePath 'testA' $logDir
        $childUp = Wait-Until {
            @(Get-PythonChild $InstallDir).Count -gt 0 } $SpawnTimeoutSec
        if (-not $childUp) {
            Stop-Process -Id $a.Proc.Id -Force -ErrorAction SilentlyContinue
            Add-Result 'A clean-close' 'FAIL' `
                "invalid: no python child under install dir within ${SpawnTimeoutSec}s"
        } else {
            $closed = Close-And-Wait $a.Proc $ExitTimeoutSec
            $code = try { $a.Proc.ExitCode } catch { 'unreadable' }
            Stop-InstallDirProcs $InstallDir
            $clean = Wait-NoSurvivors $InstallDir $SurvivorTimeoutSec
            # ErrTask completes once every handle holder is dead — read
            # it only after the survivors poll so it can't block.
            $stderr = $a.ErrTask.GetAwaiter().GetResult()
            [IO.File]::WriteAllText($a.Err, $stderr)
            $panicked = $stderr -match 'panicked'
            if ($closed -and $code -eq 0 -and -not $panicked -and $clean) {
                Add-Result 'A clean-close' 'PASS' "exit 0, no panic, no survivors"
            } else {
                Add-Result 'A clean-close' 'FAIL' `
                    "closed=$closed exit=$code panicked=$panicked noSurvivors=$clean"
            }
        }
    } catch { Add-Result 'A clean-close' 'FAIL' "harness: $_" }

    # -- TEST B: hard kill --------------------------------------------
    try {
        $b = Start-App $ExePath 'testB' $logDir
        $childUp = Wait-Until {
            @(Get-PythonChild $InstallDir).Count -gt 0 } $SpawnTimeoutSec
        if (-not $childUp) {
            Stop-Process -Id $b.Proc.Id -Force -ErrorAction SilentlyContinue
            Add-Result 'B hard-kill' 'FAIL' `
                "invalid: no python child under install dir within ${SpawnTimeoutSec}s"
        } else {
            Stop-Process -Id $b.Proc.Id -Force
            $gone = Wait-NoSurvivors $InstallDir $SurvivorTimeoutSec
            if ($gone) {
                Add-Result 'B hard-kill' 'PASS' `
                    "job containment: nothing under install dir survived"
            } else {
                $left = (Get-InstallDirProcs $InstallDir |
                    ForEach-Object { "$($_.Name)($($_.Id))" }) -join ', '
                Add-Result 'B hard-kill' 'FAIL' "survivors after kill: $left"
            }
        }
    } catch { Add-Result 'B hard-kill' 'FAIL' "harness: $_" }

    # -- TEST C: restart after kill -----------------------------------
    try {
        $c = Start-App $ExePath 'testC' $logDir
        $childUp = Wait-Until {
            @(Get-PythonChild $InstallDir).Count -gt 0 } $SpawnTimeoutSec
        if (-not $childUp) {
            Stop-Process -Id $c.Proc.Id -Force -ErrorAction SilentlyContinue
            Add-Result 'C restart' 'FAIL' `
                "no python child within ${SpawnTimeoutSec}s -- stale state after kill?"
        } else {
            $closed = Close-And-Wait $c.Proc $ExitTimeoutSec
            $code = try { $c.Proc.ExitCode } catch { 'unreadable' }
            Stop-InstallDirProcs $InstallDir
            $clean = Wait-NoSurvivors $InstallDir $SurvivorTimeoutSec
            if ($closed -and $code -eq 0 -and $clean) {
                Add-Result 'C restart' 'PASS' "restarted clean, closed clean"
            } else {
                Add-Result 'C restart' 'FAIL' `
                    "closed=$closed exit=$code noSurvivors=$clean"
            }
        }
    } catch { Add-Result 'C restart' 'FAIL' "harness: $_" }
} catch {
    Add-Result 'setup' 'FAIL' "$_"
} finally {
    # Always clean up: kill leftovers, uninstall, remove the work dir.
    if ($InstallDir) {
        Stop-InstallDirProcs $InstallDir
        [void](Wait-NoSurvivors $InstallDir 10)
        if (Test-Path $InstallDir) {
            $uninst = Get-ChildItem $InstallDir -Filter '*ninstall*.exe' `
                -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($uninst) {
                Write-Output "uninstalling via $($uninst.Name)"
                # NSIS uninstall re-execs from %TEMP% -- -Wait returns early;
                # poll for the dir to actually disappear.
                $u = Start-Process -FilePath $uninst.FullName `
                    -ArgumentList '/S' -Wait -PassThru
                if ($u.ExitCode -ne 0) {
                    Write-Warning "uninstaller exit $($u.ExitCode)"
                }
                [void](Wait-Until { -not (Test-Path $InstallDir) } 30)
                if (Test-Path $InstallDir) {
                    Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
                }
                if (Test-Path $InstallDir) {
                    Write-Warning "install dir still present after uninstall: $InstallDir"
                }
            }
        }
    }
    if ($WorkDir -and $workDirCreated -and -not $Keep) {
        Remove-Item $WorkDir -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $WorkDir) { Write-Warning "work dir left behind: $WorkDir" }
    }
}

Write-Output ''
Write-Output '-- dogfood results --'
$script:results | Format-Table Test, Result, Detail -AutoSize | Out-String | Write-Output
$failed = @($script:results | Where-Object { $_.Result -ne 'PASS' }).Count
if ($script:results.Count -eq 0) { Write-Error 'no tests ran'; exit 1 }
if ($failed -gt 0) { exit 1 }
Write-Output 'all dogfood tests passed'
exit 0
