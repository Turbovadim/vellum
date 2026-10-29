# Installs Vellum for the current user (no admin rights needed):
#   - copies the release build to %LOCALAPPDATA%\Programs\Vellum
#   - registers it for .pdf files (Explorer "Open with", Settings > Default apps)
#   - adds a Start menu shortcut
# Undo with tools\uninstall-windows.ps1.
param([switch]$NoBuild)

$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $PSScriptRoot

if (-not $NoBuild) {
    Push-Location $project
    try { cargo build --release; if ($LASTEXITCODE -ne 0) { throw "cargo build failed" } }
    finally { Pop-Location }
}

$meta = cargo metadata --format-version 1 --no-deps --manifest-path "$project\Cargo.toml" | ConvertFrom-Json
$built = Join-Path $meta.target_directory 'release\vellum.exe'
if (-not (Test-Path $built)) { throw "Release build not found at $built" }

$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Vellum'
$exe = Join-Path $installDir 'Vellum.exe'
New-Item -ItemType Directory -Force $installDir | Out-Null

# Close running copies of the installed app so the file can be replaced.
Get-Process -Name 'Vellum' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe } |
    Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Copy-Item $built $exe -Force

function Set-Key([string]$path, [string]$default) {
    if (-not (Test-Path $path)) { New-Item -Path $path -Force | Out-Null }
    if ($PSBoundParameters.ContainsKey('default')) { Set-Item -Path $path -Value $default }
}
function Set-Value([string]$path, [string]$name, [string]$value) {
    if (-not (Test-Path $path)) { New-Item -Path $path -Force | Out-Null }
    New-ItemProperty -Path $path -Name $name -Value $value -PropertyType String -Force | Out-Null
}

$classes = 'HKCU:\Software\Classes'
$progId = 'Vellum.PDF'
$open = "`"$exe`" `"%1`""
$icon = "`"$exe`",0"

# The document type Vellum handles.
Set-Key "$classes\$progId" 'PDF Document'
Set-Key "$classes\$progId\DefaultIcon" $icon
Set-Key "$classes\$progId\shell\open\command" $open
Set-Value "$classes\$progId\shell\open" 'FriendlyAppName' 'Vellum'

# Offer Vellum in "Open with" for .pdf files.
Set-Value "$classes\.pdf\OpenWithProgids" $progId ''

# Describe the executable itself (name and icon in the "Open with" list).
$app = "$classes\Applications\Vellum.exe"
Set-Value $app 'FriendlyAppName' 'Vellum'
Set-Key "$app\DefaultIcon" $icon
Set-Key "$app\shell\open\command" $open
Set-Value "$app\SupportedTypes" '.pdf' ''

# Make it selectable in Settings > Apps > Default apps.
$caps = 'HKCU:\Software\Vellum\Capabilities'
Set-Value $caps 'ApplicationName' 'Vellum'
Set-Value $caps 'ApplicationDescription' 'A PDF viewer built with GPUI'
Set-Value $caps 'ApplicationIcon' $icon
Set-Value "$caps\FileAssociations" '.pdf' $progId
Set-Value 'HKCU:\Software\RegisteredApplications' 'Vellum' 'Software\Vellum\Capabilities'

# Start menu shortcut.
$lnk = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Vellum.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($lnk)
$shortcut.TargetPath = $exe
$shortcut.IconLocation = "$exe,0"
$shortcut.Description = 'View PDF documents'
$shortcut.Save()

# Tell Explorer that file associations changed.
Add-Type -Namespace Win32 -Name Shell -MemberDefinition @'
[DllImport("shell32.dll")] public static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);
'@
[Win32.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)

Write-Host "Installed Vellum to $exe"
Write-Host "Right-click any PDF > Open with > Vellum. To make it the default, pick 'Always' there,"
Write-Host "or choose Vellum for .pdf in Settings > Apps > Default apps."
