<#
warren node installer for Windows. Run from an ELEVATED PowerShell (the startup
service is registered as SYSTEM).

  # install the binary only:
  irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1 | iex

  # install + register as a startup service (join code from the hub/dashboard):
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) `
      -Join warren1.aGVsbG8...

  # uninstall:
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) -Uninstall
#>
param(
  [string]$Join,
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
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
# Assets are version-stamped (warren-<tag>-windows-x86_64.zip); resolve the tag.
$tag = (Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -UseBasicParsing).tag_name
if (-not $tag) { Write-Error 'warren: could not resolve the latest release tag.'; return }
$url = "https://github.com/$repo/releases/download/$tag/warren-$tag-windows-x86_64.zip"
New-Item -ItemType Directory -Force -Path $dir | Out-Null

# If a node is already running, stop it first so its binary can be replaced
# (otherwise Expand-Archive hits "Access denied" on the locked warren.exe and
# the old binary keeps running). It is restarted below by `node install`.
schtasks /End /TN warren-node 2>$null | Out-Null
Get-Process warren -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

$zip = Join-Path $env:TEMP 'warren.zip'
Write-Host "warren: downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
Expand-Archive -Path $zip -DestinationPath $dir -Force
Remove-Item $zip -ErrorAction SilentlyContinue
$found = Get-ChildItem -Path $dir -Recurse -Filter warren.exe | Select-Object -First 1
if (-not $found) { Write-Error 'warren: warren.exe not found in archive.'; return }
if ($found.FullName -ne $exe) { Copy-Item $found.FullName $exe -Force }
Write-Host "warren: installed at $exe"

if ($Join) {
  $a = @('node', 'install', '--join', $Join)
  if ($Name) { $a += @('--name', $Name) }
  & $exe @a
} elseif ($Hub) {
  $a = @('node', 'install', '--hub', $Hub)
  if ($Token) { $a += @('--token', $Token) }
  if ($Tls) { $a += '--tls' }
  if ($HubFingerprint) { $a += @('--hub-fingerprint', $HubFingerprint) }
  if ($Insecure) { $a += '--insecure' }
  if ($Name) { $a += @('--name', $Name) }
  & $exe @a
} else {
  Write-Host "Next: `"$exe`" node run --join <code>   (or --hub HOST:7000 --token TOKEN)"
  Write-Host "(or add $dir to PATH)"
}
