# Windows counterpart of watchdog-test.sh: `digiclip --serve` must survive
# its grandparent exiting (2.1.0 exited 2s after every normal launch) and
# must exit when its direct parent dies.
#
#   watchdog-test.ps1 -Engine <digiclip.exe>
param([Parameter(Mandatory)] [string] $Engine)
$ErrorActionPreference = 'Stop'
$Engine = (Resolve-Path $Engine).Path
$t = New-Item -ItemType Directory -Force (Join-Path ([IO.Path]::GetTempPath()) "digiclip-wd-$PID")
function Fail($msg) {
    Write-Host "::error::$msg"
    Get-Content (Join-Path $t 'engine.log') -ErrorAction SilentlyContinue
    Get-Process digiclip -ErrorAction SilentlyContinue | Stop-Process -Force
    exit 1
}

# inner: the engine's parent (stays alive, waits on the engine).
$inner = Join-Path $t 'inner.ps1'
@"
`$PID | Set-Content '$t\parent.pid'
& '$Engine' --serve --port 4997 --token t *> '$t\engine.log'
"@ | Set-Content $inner
# outer: starts inner and exits at once — the engine's grandparent is gone.
$outer = Join-Path $t 'outer.ps1'
"Start-Process pwsh -ArgumentList '-NoProfile','-File','$inner' -WindowStyle Hidden" | Set-Content $outer
# Not `Start-Process -Wait`: that waits for every descendant, i.e. the engine.
$outerProc = Start-Process pwsh -ArgumentList '-NoProfile', '-File', $outer -PassThru -WindowStyle Hidden
if (-not $outerProc.WaitForExit(60000)) { Fail 'outer launcher did not exit' }

$log = Join-Path $t 'engine.log'
for ($i = 0; $i -lt 100; $i++) {
    if ((Test-Path $log) -and (Select-String -Path $log -Pattern 'DIGICLIP_SERVE' -Quiet)) { break }
    Start-Sleep -Milliseconds 200
}
if (-not (Select-String -Path $log -Pattern 'DIGICLIP_SERVE' -Quiet)) { Fail 'engine never printed its serve banner' }
$engineProc = Get-Process digiclip | Select-Object -First 1
$parentPid = [int](Get-Content (Join-Path $t 'parent.pid'))

Start-Sleep -Seconds 5
if ($engineProc.HasExited) { Fail 'engine exited while its parent was alive' }
Write-Host 'ok: engine alive with its grandparent gone'
$info = Get-CimInstance Win32_Process -Filter "ProcessId=$($engineProc.Id)"
Write-Host "engine pid $($engineProc.Id), parent pid $($info.ParentProcessId) (launcher script pid $parentPid)"
Get-Content $log

Stop-Process -Id $parentPid -Force
if (-not $engineProc.WaitForExit(10000)) {
    $info = Get-CimInstance Win32_Process -Filter "ProcessId=$($engineProc.Id)"
    Write-Host "still running: parent pid $($info.ParentProcessId); parent alive: $([bool](Get-Process -Id $info.ParentProcessId -ErrorAction SilentlyContinue))"
    Fail 'engine outlived its parent'
}
Write-Host 'ok: engine exited after its parent died'
