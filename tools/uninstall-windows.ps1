# Removes everything tools\install-windows.ps1 added for the current user.
$ErrorActionPreference = 'SilentlyContinue'

$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Vellum'
$exe = Join-Path $installDir 'Vellum.exe'
Get-Process -Name 'Vellum' | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
Start-Sleep -Milliseconds 300

$classes = 'HKCU:\Software\Classes'
Remove-Item "$classes\Vellum.PDF" -Recurse -Force
Remove-ItemProperty "$classes\.pdf\OpenWithProgids" -Name 'Vellum.PDF' -Force
Remove-Item "$classes\Applications\Vellum.exe" -Recurse -Force
Remove-Item 'HKCU:\Software\Vellum' -Recurse -Force
Remove-ItemProperty 'HKCU:\Software\RegisteredApplications' -Name 'Vellum' -Force

Remove-Item (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Vellum.lnk') -Force
Remove-Item $installDir -Recurse -Force

Add-Type -Namespace Win32 -Name Shell -MemberDefinition @'
[DllImport("shell32.dll")] public static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);
'@
[Win32.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)

Write-Host 'Vellum has been uninstalled.'
