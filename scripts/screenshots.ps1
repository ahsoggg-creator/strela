# Makes the README screenshots on invented data: a throw-away demo tree in %TEMP%, a separate data folder
# (STRELA_DATA_DIR), Strela driven through its own command line and keyboard. Nothing real is shown or touched.
#
#   cargo build --release
#   powershell -ExecutionPolicy Bypass -File scripts\screenshots.ps1
param(
    [string]$Out = 'docs\screenshots',
    [string]$DemoRoot = 'F:\Demo',          # where the invented tree is made (a short neutral path: it shows up in the screenshots)
    [string]$DataRoot = 'F:\Demo-data',
    [string]$BackupDrive = 'E'               # a slower drive for the transfer scene, so the progress is visible
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo
$exe = Join-Path $repo 'target\release\Strela.exe'
$cli = Join-Path $repo 'target\release\strela-cli.exe'
$demo = $DemoRoot
$data = $DataRoot
$backup = "${BackupDrive}:\Резерв-демо"
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path

if (Get-Process Strela -ErrorAction SilentlyContinue) { throw 'Strela is running: close it first (the demo needs the only instance).' }

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public class Win {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

function Find-Window {
    $ids = (Get-Process -Name Strela -ErrorAction SilentlyContinue).Id
    $script:found = [IntPtr]::Zero
    [Win]::EnumWindows({ param($h, $l)
        $p = 0; [Win]::GetWindowThreadProcessId($h, [ref]$p) | Out-Null
        if ($ids -contains $p -and [Win]::IsWindowVisible($h)) {
            $sb = New-Object System.Text.StringBuilder 256; [Win]::GetWindowText($h, $sb, 256) | Out-Null
            if ($sb.ToString() -like 'Strela*') { $script:found = $h }
        }
        return $true }, [IntPtr]::Zero) | Out-Null
    return $script:found
}

# Captures only the visible frame of the Strela window (not the invisible resize border, which would show whatever is behind it).
function Shot([string]$Name) {
    Start-Sleep -Milliseconds 700
    $h = Find-Window
    if ($h -eq [IntPtr]::Zero) { throw "no Strela window for $Name" }
    $r = New-Object Win+RECT
    [Win]::DwmGetWindowAttribute($h, 9, [ref]$r, [Runtime.InteropServices.Marshal]::SizeOf($r)) | Out-Null   # DWMWA_EXTENDED_FRAME_BOUNDS
    $w = $r.R - $r.L; $hgt = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap $w, $hgt
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size $w, $hgt))
    $bmp.Save((Join-Path $Out "$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Host "  $Name.png ($w x $hgt)"
}

function Keys([string]$k, [int]$wait = 600) {
    $h = Find-Window
    if ($h -ne [IntPtr]::Zero) { [Win]::SetForegroundWindow($h) | Out-Null }
    Start-Sleep -Milliseconds 300
    [System.Windows.Forms.SendKeys]::SendWait($k)
    Start-Sleep -Milliseconds $wait
}

function Cmd([string[]]$a) { & $exe @a; Start-Sleep -Milliseconds 900 }

# ------------------------------------------------------------------------------------------------ demo data
if (Test-Path $demo) { [IO.Directory]::Delete($demo, $true) }
if (Test-Path $data) { [IO.Directory]::Delete($data, $true) }
New-Item -ItemType Directory -Force $demo, $data | Out-Null
function Sparse([string]$rel, [int]$mb) { & $cli bench gen-sparse (Join-Path $demo $rel) --mb $mb }
function Text([string]$rel, [string]$body) {
    $p = Join-Path $demo $rel
    New-Item -ItemType Directory -Force (Split-Path $p) | Out-Null
    [IO.File]::WriteAllText($p, $body, (New-Object Text.UTF8Encoding($false)))
}
Sparse 'Документы\Отчёт за 3 квартал 2025.xlsx' 2
Sparse 'Документы\Договор аренды.docx' 1
Sparse 'Документы\Презентация проекта 2025.pptx' 14
Sparse 'Документы\Смета ремонта 2025.pdf' 3
Sparse 'Видео\Концерт.mkv' 2150
Sparse 'Видео\Заставка.mp4' 48
Sparse 'Видео\Свадьба 2024.mp4' 4300
foreach ($i in 4021..4030) { Sparse "Фото\Отпуск 2025\IMG_$i.jpg" (4 + ($i % 5)) }
foreach ($i in 1..9) { Sparse ("Музыка\Альбом\{0:00} Трек.flac" -f $i) (28 + $i) }
Sparse 'Игры\Космос\data.pak' 6600
Sparse 'Игры\Космос\space.exe' 42
Sparse 'Установщики\setup-editor.exe' 180
Sparse 'Установщики\drivers-pack.zip' 940
Text 'Проекты\demo\src\main.rs' "fn main() {`n    // TODO: прочитать настройки из файла`n    let items = load();`n    println!(`"{}`", items.len());`n}`n"
Text 'Проекты\demo\src\lib.rs' "pub fn load() -> Vec<String> {`n    // TODO: кэшировать результат`n    vec![]`n}`n`n// TODO: покрыть тестами`n"
Text 'Проекты\demo\README.md' "# Демо`n`nTODO: описание проекта.`n"
Text 'Проекты\demo\Cargo.toml' "[package]`nname = `"demo`"`nversion = `"0.1.0`"`n"
# the one real big file: the transfer scene needs a genuine copy to show speed
& $cli bench gen-big (Join-Path $demo 'Видео\Отпуск 2025.mp4') --mb 3500 | Out-Null

$settings = @{ theme = 'Dark'; accent = @(45, 160, 235); autostart = $false; use_service = $false; index_roots = @($demo); close_to_tray = $false }
$env:STRELA_DATA_DIR = $data
function Write-Settings($s) { [IO.File]::WriteAllText((Join-Path $data 'settings.json'), ($s | ConvertTo-Json -Compress), (New-Object Text.UTF8Encoding($false))) }
Write-Settings $settings

try {
    # ---------------------------------------------------------------------------------------- dark theme
    Write-Host 'dark theme'
    $app = Start-Process $exe -PassThru
    Start-Sleep 5
    Keys '2025' 1500
    Shot '01-search'

    Cmd @('--cmd', 'copy', '--', (Join-Path $demo 'Видео\Отпуск 2025.mp4'))
    Start-Sleep 1
    Cmd @('--cmd', 'paste', '--', $backup)
    Keys '^2' 200
    Start-Sleep -Milliseconds 2300
    Shot '02-transfer'
    Start-Sleep 8   # let it finish; the queue keeps the finished card

    Cmd @('--cmd', 'largest-here', '--', $demo)
    Start-Sleep 2
    Shot '03-cleanup'

    Cmd @('--cmd', 'find-content-here', '--', $demo)
    Start-Sleep 1
    Keys 'TODO' 300
    Keys '{ENTER}' 2500
    Shot '04-content'

    Keys '^5' 800
    Shot '05-settings'

    Cmd @('--cmd', 'fast-delete', '--', (Join-Path $demo 'Установщики'))
    Start-Sleep 2
    Shot '06-delete'
    Keys '{ESC}' 500

    Cmd @('--cmd', 'spotlight')
    Start-Sleep 1
    Keys 'отпуск' 1800
    Shot '07-spotlight'
    Keys '{ESC}' 500

    & $exe --cmd quit
    Start-Sleep 5

    # ---------------------------------------------------------------------------------------- light theme
    Write-Host 'light theme'
    $settings.theme = 'Light'; Write-Settings $settings
    $app = Start-Process $exe -PassThru
    Start-Sleep 5
    Keys '2025' 1500
    Shot '08-light'
}
finally {
    if (Get-Process Strela -ErrorAction SilentlyContinue) { & $exe --cmd quit; Start-Sleep 5; Get-Process Strela -ErrorAction SilentlyContinue | Stop-Process -Force }
    Remove-Item Env:\STRELA_DATA_DIR -ErrorAction SilentlyContinue
    if (Test-Path $backup) { [IO.Directory]::Delete($backup, $true) }
    if (Test-Path $demo) { [IO.Directory]::Delete($demo, $true) }
    if (Test-Path $data) { [IO.Directory]::Delete($data, $true) }
}
Write-Host "screenshots in $Out" -ForegroundColor Green
