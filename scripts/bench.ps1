# Strela benchmark: Strela against Explorer's copy engine, robocopy and CopyFile2 on generated data.
#
#   powershell -ExecutionPolicy Bypass -File scripts\bench.ps1            # full run (5 GiB file, 100 000 files)
#   powershell -ExecutionPolicy Bypass -File scripts\bench.ps1 -Quick     # small sizes, to check the harness
#
# Everything happens in folders created by this script ($Root* below) and is deleted afterwards. Results are written
# to docs\BENCHMARKS.md together with a description of this machine, so the numbers can be judged in context.
param(
    [int]$BigMB = 5120,
    [int]$SmallCount = 100000,
    [int]$MoveSmall = 10000,
    [int]$Runs = 3,
    [string]$NvmeA = 'F:\strela-bench',                       # NVMe disk 1
    [string]$NvmeB = (Join-Path $env:TEMP 'strela-bench-dst'), # NVMe disk 2 (system drive)
    [string]$Sata = 'E:\strela-bench-sata',                    # SATA SSD
    [switch]$Quick,
    [switch]$SkipSata,
    [string[]]$Only = @('big', 'small', 'move', 'delete', 'search'),
    [string[]]$SearchRoots = @(),   # default: every local drive
    [int]$Cooldown = -1             # seconds of rest before every timed run (default 25, 0 with -Quick)
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if ($Quick) { $BigMB = 600; $SmallCount = 5000; $MoveSmall = 1000; $Runs = 2; if ($Cooldown -lt 0) { $Cooldown = 0 } }
if ($Cooldown -lt 0) { $Cooldown = 25 }

# strela-cli runs from %TEMP%: Windows Defender treats processes from different folders very differently (see
# docs\BENCHMARKS.md), and every Strela-hosted tool below runs from the same place.
$binDir = Join-Path $env:TEMP 'strela-bench-bin'
New-Item -ItemType Directory -Force $binDir | Out-Null
Copy-Item (Join-Path $repo 'target\release\strela-cli.exe') $binDir -Force
$cli = Join-Path $binDir 'strela-cli.exe'

function Remove-Tree($p) {
    if (Test-Path -LiteralPath $p) { cmd /c "rd /s /q `"$p`"" 2>$null | Out-Null }
}

# Returns @{ Returned = seconds until the tool exited; Durable = seconds until the data was also flushed to the disk }.
# Buffered writers (Explorer, robocopy) return as soon as the data sits in RAM; unbuffered ones (the Strela pipeline,
# robocopy /J) return when it is on the device. FlushFileBuffers on the result makes the two comparable.
function Measure-Tool([scriptblock]$Action, [string]$FlushPath) {
    [GC]::Collect()
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $Action *> $null
    $returned = $sw.Elapsed.TotalSeconds
    if ($FlushPath -and (Test-Path -LiteralPath $FlushPath)) { & $cli bench flush $FlushPath *> $null }
    $sw.Stop()
    return @{ Returned = $returned; Durable = $sw.Elapsed.TotalSeconds }
}

function Median($xs) {
    $s = @($xs | Sort-Object)
    if ($s.Count -eq 0) { return [double]::NaN }
    if ($s.Count % 2) { return $s[($s.Count - 1) / 2] }
    return ($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2
}

function Count-Tree($p) {
    $line = (& $cli bench count $p) | Select-String 'COUNT'
    if ($line -match 'files=(\d+) dirs=(\d+) bytes=(\d+)') { return @{ Files = [int64]$Matches[1]; Bytes = [int64]$Matches[3] } }
    throw "cannot count $p"
}

function Fmt([double]$s) { if ([double]::IsNaN($s)) { 'n/a' } else { ('{0:N2} с' -f $s) } }

$results = [ordered]@{}   # scenario -> list of rows

function Run-Scenario {
    param([string]$Name, [string]$Note, [hashtable]$Tools, [scriptblock]$Prepare, [scriptblock]$Cleanup, [scriptblock]$Verify, [double]$Megabytes, [string]$FlushPath = '')
    Write-Host "=== $Name" -ForegroundColor Cyan
    $times = @{}; $returned = @{}
    foreach ($t in $Tools.Keys) { $times[$t] = New-Object System.Collections.Generic.List[double]; $returned[$t] = New-Object System.Collections.Generic.List[double] }
    $names = @($Tools.Keys)
    for ($i = 1; $i -le $Runs; $i++) {
        # the order rotates from run to run, so no tool always runs right after a heavy writer
        for ($k = 0; $k -lt $names.Count; $k++) {
            $t = $names[($k + $i - 1) % $names.Count]
            & $Prepare
            if ($Cooldown -gt 0) { Start-Sleep -Seconds $Cooldown }   # lets the SSD's SLC cache drain after the preparation writes
            $m = Measure-Tool $Tools[$t] $FlushPath
            $ok = if ($Verify) { & $Verify } else { $true }
            if ($ok) { $times[$t].Add($m.Durable); $returned[$t].Add($m.Returned) } else { Write-Warning "${t}: проверка результата не прошла, замер отброшен" }
            Write-Host ("  run {0} {1,-34} {2}  (возврат {3})" -f $i, $t, $(if ($ok) { Fmt $m.Durable } else { 'ОШИБКА' }), (Fmt $m.Returned))
            & $Cleanup
        }
    }
    $rows = foreach ($t in $Tools.Keys) {
        $v = $times[$t]
        [pscustomobject]@{
            Tool = $t; Median = (Median $v); Min = ($v | Measure-Object -Minimum).Minimum; Max = ($v | Measure-Object -Maximum).Maximum
            ReturnedMedian = (Median $returned[$t])
            N = $v.Count; Speed = $(if ($Megabytes -gt 0 -and $v.Count) { $Megabytes / (Median $v) } else { 0 })
        }
    }
    $script:results[$Name] = @{ Note = $Note; Rows = @($rows); MB = $Megabytes; Flushed = [bool]$FlushPath }
}

# ------------------------------------------------------------------------------------------------ machine
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1)
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
$os = (Get-CimInstance Win32_OperatingSystem)
$disks = Get-PhysicalDisk | ForEach-Object { "{0} ({1}, {2}, {3:N0} ГБ)" -f $_.FriendlyName, $_.MediaType, $_.BusType, ($_.Size / 1GB) }
$rt = try { (Get-MpComputerStatus).RealTimeProtectionEnabled } catch { 'неизвестно' }

# ------------------------------------------------------------------------------------------------ 1. one big file
function Big-Scenario([string]$SrcRoot, [string]$DstRoot, [string]$Title) {
    $srcDir = Join-Path $SrcRoot 'big'
    $dstDir = Join-Path $DstRoot 'out'
    $srcFile = Join-Path $srcDir 'big.bin'
    $prepare = {
        Remove-Tree $dstDir
        New-Item -ItemType Directory -Force $dstDir | Out-Null
        New-Item -ItemType Directory -Force $srcDir | Out-Null
        # a fresh file per run, written without the cache: every tool reads the source cold
        & $cli bench gen-big $srcFile --mb $BigMB *> $null
    }
    $verify = { (Get-Item (Join-Path $dstDir 'big.bin')).Length -eq ([int64]$BigMB * 1MB) }
    $tools = [ordered]@{
        'Проводник (IFileOperation)'      = { & $cli bench shell-copy $srcFile $dstDir }
        'robocopy (1 поток, с буфером)'   = { robocopy $srcDir $dstDir big.bin /R:0 /W:0 /NFL /NDL /NJH /NJS /NP }
        'robocopy /J (без буфера)'        = { robocopy $srcDir $dstDir big.bin /J /R:0 /W:0 /NFL /NDL /NJH /NJS /NP }
        'robocopy /MT:16'                 = { robocopy $srcDir $dstDir big.bin /MT:16 /R:0 /W:0 /NFL /NDL /NJH /NJS /NP }
        'CopyFile2 (один вызов)'          = { & $cli bench cf2 $srcFile (Join-Path $dstDir 'big.bin') }
        'Strela'                          = { & $cli copy $srcFile $dstDir --quiet }
        'Strela + проверка xxh3'          = { & $cli copy $srcFile $dstDir --quiet --verify xxh3 }
    }
    Run-Scenario -Name $Title -Note "Один файл $BigMB МБ, источник записан без кэша и читается «холодным»." -Tools $tools -Prepare $prepare `
        -Cleanup { Remove-Tree $srcDir; Remove-Tree $dstDir } -Verify $verify -Megabytes $BigMB -FlushPath (Join-Path $dstDir 'big.bin')
}

# ------------------------------------------------------------------------------------------------ 2. many small files
function Small-Scenario([string]$SrcRoot, [string]$DstRoot, [string]$Title) {
    $tmpl = Join-Path $SrcRoot 'small-template'
    $dstDir = Join-Path $DstRoot 'outsmall'
    Remove-Tree $tmpl
    Write-Host "  генерирую $SmallCount файлов…"
    & $cli bench gen-small $tmpl --count $SmallCount --dirs ([math]::Max(10, [int]($SmallCount / 100))) *> $null
    $ref = Count-Tree $tmpl
    $mb = [math]::Round($ref.Bytes / 1MB)
    $prepare = {
        Remove-Tree $dstDir
        New-Item -ItemType Directory -Force $dstDir | Out-Null
    }
    $verify = { $c = Count-Tree $dstDir; ($c.Files -eq $ref.Files) -and ($c.Bytes -eq $ref.Bytes) }
    $src = $tmpl
    $tools = [ordered]@{
        'Проводник (IFileOperation)'        = { & $cli bench shell-copy $src $dstDir }
        'robocopy (1 поток)'                = { robocopy $src (Join-Path $dstDir 'small-template') /E /R:0 /W:0 /NFL /NDL /NJH /NJS /NP }
        'robocopy /MT:16'                   = { robocopy $src (Join-Path $dstDir 'small-template') /E /MT:16 /R:0 /W:0 /NFL /NDL /NJH /NJS /NP }
        'Strela (автовыбор способа)'        = { & $cli copy $src $dstDir --quiet }
        'Strela (только CopyFile2)'         = { & $cli copy $src $dstDir --quiet --engine copyfile2 }
        'Strela + проверка xxh3'            = { & $cli copy $src $dstDir --quiet --verify xxh3 }
    }
    Run-Scenario -Name $Title -Note "$($ref.Files) файлов, $mb МБ. Источник в кэше системы (холодный кэш без прав администратора не сбросить), поэтому читается одинаково для всех." `
        -Tools $tools -Prepare $prepare -Cleanup { Remove-Tree $dstDir } -Verify $verify -Megabytes $mb
    Remove-Tree $tmpl
}

# ------------------------------------------------------------------------------------------------ 3. move inside a volume
function Move-Scenario([string]$Root) {
    $payload = Join-Path $Root 'mv\payload'
    $target = Join-Path $Root 'mv\target'
    Remove-Tree (Join-Path $Root 'mv')
    New-Item -ItemType Directory -Force $payload, $target | Out-Null
    & $cli bench gen-big (Join-Path $payload 'big.bin') --mb $BigMB *> $null
    & $cli bench gen-small (Join-Path $payload 'small') --count $MoveSmall --dirs 100 *> $null
    $ref = Count-Tree $payload
    $verify = { $c = Count-Tree (Join-Path $target 'payload'); ($c.Files -eq $ref.Files) -and ($c.Bytes -eq $ref.Bytes) }
    $back = {
        $moved = Join-Path $target 'payload'
        if (Test-Path $moved) {
            if (Test-Path $payload) { Remove-Tree $payload }   # an empty leftover of the tool, never the data itself
            cmd /c "move `"$moved`" `"$payload`"" *> $null
        }
        if (-not (Test-Path $payload)) { throw 'payload lost' }
    }
    $tools = [ordered]@{
        'Проводник (IFileOperation)' = { & $cli bench shell-move $payload $target }
        'cmd move'                   = { cmd /c "move `"$payload`" `"$(Join-Path $target 'payload')`"" }
        'PowerShell Move-Item'       = { Move-Item -LiteralPath $payload -Destination (Join-Path $target 'payload') }
        'robocopy /MOVE'             = { robocopy $payload (Join-Path $target 'payload') /E /MOVE /R:0 /W:0 /NFL /NDL /NJH /NJS /NP }
        'Strela'                     = { & $cli move $payload $target --quiet }
    }
    # `back` restores the payload after every run (not timed). robocopy /MOVE may leave an empty source behind.
    Run-Scenario -Name "Перемещение внутри диска ($([math]::Round($ref.Bytes / 1MB)) МБ, $($ref.Files) файлов)" `
        -Note 'Тот же том: правильный ответ — переименование, не копирование.' -Tools $tools `
        -Prepare { New-Item -ItemType Directory -Force $payload, $target | Out-Null } -Cleanup $back -Verify $verify -Megabytes 0
    Remove-Tree (Join-Path $Root 'mv')
}

# ------------------------------------------------------------------------------------------------ 4. delete a tree
function Delete-Scenario([string]$Root) {
    $tmpl = Join-Path $Root 'del-template'
    $work = Join-Path $Root 'del-work'
    $empty = Join-Path $Root 'empty'
    Remove-Tree $tmpl; Remove-Tree $work
    New-Item -ItemType Directory -Force $empty | Out-Null
    Write-Host "  генерирую $SmallCount файлов…"
    & $cli bench gen-small $tmpl --count $SmallCount --dirs ([math]::Max(10, [int]($SmallCount / 100))) *> $null
    $ref = Count-Tree $tmpl
    $prepare = {
        Remove-Tree $work
        New-Item -ItemType Directory -Force $work | Out-Null
        & $cli copy $tmpl $work --quiet --engine copyfile2 *> $null
        $c = Count-Tree (Join-Path $work 'del-template')
        if ($c.Files -ne $ref.Files) { throw 'fixture copy failed' }
    }
    $victim = Join-Path $work 'del-template'
    $verify = { -not (Test-Path -LiteralPath $victim) }
    $tools = [ordered]@{
        'Проводник (IFileOperation)'        = { & $cli bench shell-delete $victim }
        'cmd rd /s /q'                      = { cmd /c "rd /s /q `"$victim`"" }
        'PowerShell Remove-Item -Recurse'   = { Remove-Item -LiteralPath $victim -Recurse -Force }
        'robocopy /MIR (пустая папка, MT:16)' = { robocopy $empty $victim /MIR /MT:16 /R:0 /W:0 /NFL /NDL /NJH /NJS /NP; cmd /c "rd /s /q `"$victim`"" }
        'Strela'                            = { & $cli delete $victim -y }
    }
    Run-Scenario -Name "Удаление дерева ($($ref.Files) файлов, $([math]::Round($ref.Bytes / 1MB)) МБ)" `
        -Note 'Безвозвратное удаление; каждое дерево создаётся заново перед замером.' -Tools $tools -Prepare $prepare -Cleanup { Remove-Tree $work } -Verify $verify -Megabytes 0
    Remove-Tree $tmpl; Remove-Tree $empty; Remove-Tree $work
}

# ------------------------------------------------------------------------------------------------ 5. search
function Search-Scenario {
    $ErrorActionPreference = 'Continue'   # strela-cli prints timings to stderr; that is not an error
    Write-Host "=== Поиск" -ForegroundColor Cyan
    $ix = Join-Path $env:TEMP 'strela-bench-index.bin'
    $roots = if ($SearchRoots.Count) { $SearchRoots } else { @(Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -match '^[A-Z]:\\$' -and $_.Used -gt 0 } | ForEach-Object { $_.Root }) }
    $rootArgs = @(); foreach ($r in $roots) { $rootArgs += @('--root', $r) }
    $out = & $cli index @rootArgs --index $ix 2>&1
    $scan = ($out | Select-String 'всего ([\d.,]+) с').Matches.Groups[1].Value
    $entries = ($out | ForEach-Object { if ($_ -match '\s(\d+) записей') { [int64]$Matches[1] } } | Measure-Object -Sum).Sum
    $queries = @('witcher', 'ext:mp4', 'size:>1GB', 'modified:<1d', 'cargo toml')
    $rows = foreach ($q in $queries) {
        $ms = @()
        for ($i = 0; $i -lt 5; $i++) {
            $o = & $cli search $q @rootArgs --index $ix --time -n 20 2>&1 | Out-String
            if ($o -match 'за ([\d.,]+) мс') { $ms += [double]($Matches[1] -replace ',', '.') }
        }
        $hits = if ($o -match '(\d+) совпадений') { $Matches[1] } else { '?' }
        [pscustomobject]@{ Query = $q; Hits = $hits; Median = (Median $ms) }
    }
    $script:searchRes = @{ Scan = $scan; Entries = $entries; Roots = ($roots -join ' '); Rows = @($rows) }
    Remove-Item $ix -ErrorAction SilentlyContinue
}

# ------------------------------------------------------------------------------------------------ run
New-Item -ItemType Directory -Force $NvmeA, $NvmeB | Out-Null
try {
    if ($Only -contains 'big') {
        Big-Scenario $NvmeA $NvmeB "Один файл $BigMB МБ: NVMe → NVMe"
        if (-not $SkipSata) {
            New-Item -ItemType Directory -Force $Sata | Out-Null
            Big-Scenario $Sata $NvmeA "Один файл $BigMB МБ: SATA SSD → NVMe"
        }
    }
    if ($Only -contains 'small') { Small-Scenario $NvmeA $NvmeB "$SmallCount мелких файлов: NVMe → NVMe" }
    if ($Only -contains 'move') { Move-Scenario $NvmeA }
    if ($Only -contains 'delete') { Delete-Scenario $NvmeA }
    if ($Only -contains 'search') { Search-Scenario }
}
finally {
    Remove-Tree $NvmeA; Remove-Tree $NvmeB; if (-not $SkipSata) { Remove-Tree $Sata }
}

# ------------------------------------------------------------------------------------------------ report
$md = New-Object System.Text.StringBuilder
[void]$md.AppendLine('# Скорость: измерения')
[void]$md.AppendLine('')
[void]$md.AppendLine("Все цифры получены скриптом ``scripts\bench.ps1`` на этой машине, $(Get-Date -Format 'yyyy-MM-dd'). Никаких чужих или оценочных чисел здесь нет.")
[void]$md.AppendLine('')
[void]$md.AppendLine('## Машина')
[void]$md.AppendLine('')
[void]$md.AppendLine("- Процессор: $($cpu.Name.Trim()) ($($cpu.NumberOfCores) ядер / $($cpu.NumberOfLogicalProcessors) потоков)")
[void]$md.AppendLine("- Память: $ram ГБ")
[void]$md.AppendLine("- ОС: $($os.Caption) $($os.Version)")
[void]$md.AppendLine("- Диски: " + ($disks -join '; '))
[void]$md.AppendLine("- Антивирус (Защитник Windows), защита в реальном времени: **$rt** — она влияет на все замеры, особенно на создание мелких файлов")
[void]$md.AppendLine("- Права: обычный пользователь (без администратора), поэтому индекс строился обходом папок, а не чтением MFT")
[void]$md.AppendLine('')
[void]$md.AppendLine('## Методика')
[void]$md.AppendLine('')
[void]$md.AppendLine("- Каждый инструмент запускается $Runs раза, в таблице — медиана (и разброс). Запуски идут по кругу, порядок инструментов сдвигается от раза к разу, перед каждым замером пауза $Cooldown с: SSD без DRAM-кэша сильно зависит от своего состояния (кэш SLC), и без пауз порядок запуска решал бы больше, чем сам инструмент.")
[void]$md.AppendLine('- Для больших файлов указано два времени: когда инструмент вернул управление и когда данные ещё и сброшены на диск (`FlushFileBuffers`). Копирование «с буфером» (Проводник, robocopy) возвращается, как только данные попали в оперативную память; копирование без буфера (Strela, `robocopy /J`) — когда они на устройстве. Честное сравнение — по второму столбцу.')
[void]$md.AppendLine('- Время — стенные часы всего процесса, включая его запуск.')
[void]$md.AppendLine('- После каждого запуска проверяется результат (размер файла / число файлов и байт); запуск, не прошедший проверку, отбрасывается.')
[void]$md.AppendLine('- «Проводник (IFileOperation)» — тот же движок копирования, который использует Проводник; запускается без окна прогресса (с окном он медленнее). Он живёт в `strela-cli.exe bench shell-*`, который запущен из той же папки, что и Strela.')
[void]$md.AppendLine('- Большой файл создаётся заново перед каждым запуском и записывается без кэша, так что все читают его «холодным».')
[void]$md.AppendLine('- Папки данных удаляются после замеров.')
[void]$md.AppendLine('')
foreach ($name in $results.Keys) {
    $r = $results[$name]
    [void]$md.AppendLine("## $name")
    [void]$md.AppendLine('')
    [void]$md.AppendLine($r.Note)
    [void]$md.AppendLine('')
    if ($r.Flushed) { [void]$md.AppendLine('| Инструмент | Вернул управление | С записью на диск (медиана) | Мин–макс | Скорость |'); [void]$md.AppendLine('| --- | ---: | ---: | ---: | ---: |') }
    elseif ($r.MB -gt 0) { [void]$md.AppendLine('| Инструмент | Медиана | Мин–макс | Скорость |'); [void]$md.AppendLine('| --- | ---: | ---: | ---: |') }
    else { [void]$md.AppendLine('| Инструмент | Медиана | Мин–макс |'); [void]$md.AppendLine('| --- | ---: | ---: |') }
    $best = ($r.Rows | Where-Object { $_.N -gt 0 } | Sort-Object Median | Select-Object -First 1).Tool
    foreach ($row in $r.Rows) {
        $tool = if ($row.Tool -eq $best) { "**$($row.Tool)**" } else { $row.Tool }
        $range = if ($row.N) { "{0:N2}–{1:N2} с" -f $row.Min, $row.Max } else { 'n/a' }
        if ($r.Flushed) { [void]$md.AppendLine("| $tool | $(Fmt $row.ReturnedMedian) | $(Fmt $row.Median) | $range | $(if ($row.Speed) { '{0:N0} МБ/с' -f $row.Speed } else { 'n/a' }) |") }
        elseif ($r.MB -gt 0) { [void]$md.AppendLine("| $tool | $(Fmt $row.Median) | $range | $(if ($row.Speed) { '{0:N0} МБ/с' -f $row.Speed } else { 'n/a' }) |") }
        else { [void]$md.AppendLine("| $tool | $(Fmt $row.Median) | $range |") }
    }
    [void]$md.AppendLine('')
}
$s = $script:searchRes
if ($s) {
[void]$md.AppendLine('## Поиск по имени')
[void]$md.AppendLine('')
[void]$md.AppendLine("Индекс обходом папок ($($s.Roots)): **$($s.Entries) записей за $($s.Scan) с** (без прав администратора; чтение MFT быстрее, но на этой машине не измерялось). Время запроса — внутри программы, по сохранённому индексу, медиана из 5 запусков; 20 результатов с ранжированием, нечёткое совпадение включено.")
[void]$md.AppendLine('')
[void]$md.AppendLine('| Запрос | Найдено | Время |'); [void]$md.AppendLine('| --- | ---: | ---: |')
foreach ($q in $s.Rows) { [void]$md.AppendLine("| ``$($q.Query)`` | $($q.Hits) | $('{0:N2}' -f $q.Median) мс |") }
[void]$md.AppendLine('')
}
$outFile = Join-Path $repo 'docs\BENCHMARKS.md'
[IO.File]::WriteAllText($outFile, $md.ToString(), (New-Object Text.UTF8Encoding($false)))
$results | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $repo 'docs\bench-results.json') -Encoding utf8
Write-Host "готово: $outFile" -ForegroundColor Green
