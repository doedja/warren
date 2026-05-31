<#
warren node installer for Windows. Run from an ELEVATED PowerShell (the startup
service is registered as SYSTEM).

  # install the binary only:
  irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1 | iex

  # install + register as a startup service:
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) `
      -Hub HOST:7000 -Token TOKEN -Tls -HubFingerprint FP

  # uninstall:
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) -Uninstall
#>
param(
  [string]$Hub,
  [string]$Token,
  [string]$HubFingerprint,
  [string]$Name,
  [switch]$Tls,
  [switch]$Insecure,
  [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'
$repo = 'doedja/warren'
$dir = Join-Path $env:LOCALAPPDATA 'warren'
$exe = Join-Path $dir 'warren.exe'

if ($Uninstall) {
  if (Test-Path $exe) { & $exe node uninstall }
  Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
  Write-Host 'warren: uninstalled.'
  return
}

$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64') {
  Write-Error "warren: only x86_64 Windows binary is published (got $arch). Build from source: cargo install --git https://github.com/$repo warren"
  return
}
$url = "https://github.com/$repo/releases/latest/download/warren-windows-x86_64.zip"

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$zip = Join-Path $env:TEMP 'warren.zip'
Write-Host "warren: downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
Expand-Archive -Path $zip -DestinationPath $dir -Force
Remove-Item $zip -ErrorAction SilentlyContinue
$found = Get-ChildItem -Path $dir -Recurse -Filter warren.exe | Select-Object -First 1
if (-not $found) { Write-Error 'warren: warren.exe not found in archive.'; return }
if ($found.FullName -ne $exe) { Copy-Item $found.FullName $exe -Force }
Write-Host "warren: installed at $exe"

if ($Hub) {
  $a = @('node', 'install', '--hub', $Hub)
  if ($Token) { $a += @('--token', $Token) }
  if ($Tls) { $a += '--tls' }
  if ($HubFingerprint) { $a += @('--hub-fingerprint', $HubFingerprint) }
  if ($Insecure) { $a += '--insecure' }
  if ($Name) { $a += @('--name', $Name) }
  & $exe @a
} else {
  Write-Host "Next: `"$exe`" node run --hub HOST:7000 --token TOKEN [-Tls -HubFingerprint FP]"
  Write-Host "(or add $dir to PATH)"
}
