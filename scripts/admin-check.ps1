# Checks the administrator-only parts of Strela on this machine: reading $MFT straight from NTFS volumes and following
# the USN change journal. Everything is read-only except one scratch folder it creates (and removes) on the first volume.
# One UAC prompt; the output is written to the log file and printed.
#
#   powershell -ExecutionPolicy Bypass -File scripts\admin-check.ps1 [-Roots C:\,E:\]
param(
    [string[]]$Roots = @(),
    [string]$Log = ''
)
$repo = Split-Path -Parent $PSScriptRoot
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    $log = Join-Path $env:TEMP 'strela-admin-check.log'
    if (Test-Path $log) { [IO.File]::Delete($log) }
    $rootArg = if ($Roots.Count) { "-Roots " + (($Roots | ForEach-Object { "'$_'" }) -join ',') } else { '' }
    $args = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" $rootArg -Log `"$log`""
    Write-Host 'Запрашиваю права администратора (UAC)…'
    try { Start-Process powershell -Verb RunAs -ArgumentList $args -Wait } catch { Write-Host "Отказ или ошибка: $($_.Exception.Message)"; exit 2 }
    if (Test-Path $log) { Get-Content $log } else { Write-Host 'лог не создан' }
    exit 0
}

$ErrorActionPreference = 'Continue'
function Say($m) { Write-Host $m; if ($Log) { Add-Content -LiteralPath $Log -Value $m -Encoding UTF8 } }
$cli = Join-Path $repo 'target\release\strela-cli.exe'
$exe = Join-Path $repo 'target\release\Strela.exe'
if (-not $Roots.Count) { $Roots = @(Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -match '^[A-Z]:\\$' -and $_.Used -gt 0 } | ForEach-Object { $_.Root }) }
Say "Strela admin check, $(Get-Date -Format 'yyyy-MM-dd HH:mm'), тома: $($Roots -join ' ')"

# --- 1. MFT scan, one volume at a time
$scratchRoot = $null
foreach ($r in $Roots) {
    $idx = Join-Path $env:TEMP 'strela-admin-mft.bin'
    $o = & $cli index --root $r --mft --index $idx 2>&1 | Out-String
    $line = ($o -split "`n" | Where-Object { $_ -match 'записей' } | Select-Object -First 1)
    Say ("MFT  " + $line.Trim())
    if ($line -match 'MFT\+USN' -and -not $scratchRoot -and $r -eq [IO.Path]::GetPathRoot($repo)) { $scratchRoot = $r }
    $s = & $cli search "ext:exe notepad" --root $r --mft --index $idx --time -n 3 2>&1 | Out-String
    $m = ($s -split "`n" | Where-Object { $_ -match 'совпадений' } | Select-Object -First 1)
    if ($m) { Say ("     поиск по индексу: " + $m.Trim()) }
    [IO.File]::Delete($idx)
}

# --- 2. USN journal: the index must follow a create / rename / delete
if ($scratchRoot) {
    $pipe = '\\.\pipe\StrelaAdminCheck'
    $data = Join-Path $env:TEMP 'strela-admin-usn.bin'
    $stop = [IO.Path]::ChangeExtension($data, 'stop')
    if (Test-Path $stop) { [IO.File]::Delete($stop) }
    $svc = Start-Process $exe -ArgumentList '--service-console', '--pipe', $pipe, '--data', $data, '--root', $scratchRoot -PassThru -WindowStyle Hidden
    # ready = the first scan has finished and the index already knows a file we know exists
    $probe = 'admin-check.ps1'
    $repoRoot = [IO.Path]::GetPathRoot($repo)
    for ($i = 0; $i -lt 600; $i++) {
        Start-Sleep -Milliseconds 500
        $st = & $cli search $probe --pipe $pipe --no-fuzzy 2>&1 | Out-String
        if ($st -match [regex]::Escape($probe)) { break }
    }
    Start-Sleep -Seconds 3
    $dir = Join-Path $scratchRoot 'strela-usn-check'
    New-Item -ItemType Directory -Force $dir | Out-Null
    $name = 'usn-проверка-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
    function Wait-Found($q, $want, $secs = 20) {
        $t = [Diagnostics.Stopwatch]::StartNew()
        while ($t.Elapsed.TotalSeconds -lt $secs) {
            $o = & $cli search $q --pipe $pipe --no-fuzzy 2>&1 | Out-String
            if (($o -match [regex]::Escape($q)) -eq $want) { return $t.Elapsed.TotalSeconds }
            Start-Sleep -Milliseconds 100
        }
        return -1
    }
    Set-Content (Join-Path $dir "$name.txt") 'x'
    Say ("USN создание:   найден через {0:N2} с" -f (Wait-Found "$name.txt" $true))
    Rename-Item (Join-Path $dir "$name.txt") "$name-renamed.txt"
    Say ("USN переименование: новое имя найдено через {0:N2} с" -f (Wait-Found "$name-renamed.txt" $true))
    Say ("     старое имя исчезло через {0:N2} с" -f (Wait-Found "$name.txt" $false))
    [IO.File]::Delete((Join-Path $dir "$name-renamed.txt"))
    Say ("USN удаление:   исчез через {0:N2} с" -f (Wait-Found "$name-renamed.txt" $false))
    New-Item $stop -ItemType File | Out-Null
    Start-Sleep 4
    if (-not $svc.HasExited) { $svc | Stop-Process -Force }
    [IO.Directory]::Delete($dir, $true)
    foreach ($f in $data, $stop) { if (Test-Path $f) { [IO.File]::Delete($f) } }
} else {
    Say 'USN: нет подходящего тома NTFS для проверки'
}
Say 'готово'
