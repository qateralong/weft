# Checks Weft on a real Windows machine and writes everything to windows-check.log next to this script.
# Run from an elevated PowerShell in the folder with weftd.exe, weft.exe, loom.exe and wintun.dll:
#   powershell -ExecutionPolicy Bypass -File .\windows-check.ps1
# Optionally join a real network and install the service:
#   powershell -ExecutionPolicy Bypass -File .\windows-check.ps1 -Link "weft://..." -Network lan -Password secret
#Requires -RunAsAdministrator
param(
    [string]$Link,
    [string]$Network = "lan",
    [string]$Password,
    [switch]$KeepService
)

$ErrorActionPreference = 'Continue'
$root = $PSScriptRoot
$log = Join-Path $root 'windows-check.log'
$work = Join-Path $env:TEMP "weft-check-$PID"
New-Item -ItemType Directory $work | Out-Null
Start-Transcript -Path $log -Force | Out-Null

function Section($title) { Write-Host "`n===== $title =====" }
function Weft($pipe) {
    $env:WEFT_SOCKET = $pipe
    $env:LANG = 'C'
    & (Join-Path $root 'weft.exe') @args 2>&1 | ForEach-Object { "$_" }
    Remove-Item Env:WEFT_SOCKET, Env:LANG -ErrorAction SilentlyContinue
}

Section 'System'
Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, OSArchitecture | Format-List
Get-ChildItem $root | Select-Object Name, Length | Format-Table

Section 'Local self-test'
@"
listen = "127.0.0.1:7443"
data_dir = '$work\loom'
"@ | Set-Content (Join-Path $work 'loom.toml')
$loomLink = (& (Join-Path $root 'loom.exe') --config "$work\loom.toml" link --host 127.0.0.1).Trim()
$processes = @(
    Start-Process (Join-Path $root 'loom.exe') -ArgumentList "--config `"$work\loom.toml`"" -PassThru -WindowStyle Hidden `
        -RedirectStandardError "$work\loom.log"
    Start-Process (Join-Path $root 'weftd.exe') -ArgumentList "--state-dir `"$work\a`" --socket \\.\pipe\weft-check-a --tun WeftCheck" `
        -PassThru -WindowStyle Hidden -RedirectStandardError "$work\a.log"
    Start-Process (Join-Path $root 'weftd.exe') -ArgumentList "--state-dir `"$work\b`" --socket \\.\pipe\weft-check-b --echo" `
        -PassThru -WindowStyle Hidden -RedirectStandardError "$work\b.log"
)
Start-Sleep 3
Weft '\\.\pipe\weft-check-a' up $loomLink --nickname alice
Weft '\\.\pipe\weft-check-b' up $loomLink --nickname bob
Weft '\\.\pipe\weft-check-a' create check --password secret
Weft '\\.\pipe\weft-check-b' join check --password secret
for ($i = 0; $i -lt 20; $i++) {
    if ((Weft '\\.\pipe\weft-check-a' status) -match 'bob .*online') { break }
    Start-Sleep 1
}
Weft '\\.\pipe\weft-check-a' status
$peer = ((Weft '\\.\pipe\weft-check-b' status) -match '^Address' -replace '^Address:\s*', '').Trim()
Write-Host "ping $peer"
ping -n 4 $peer
Start-Sleep 5
Get-NetAdapter -Name WeftCheck -ErrorAction SilentlyContinue | Format-List Name, InterfaceDescription, Status, MtuSize
Get-NetIPAddress -InterfaceAlias WeftCheck -AddressFamily IPv4 -ErrorAction SilentlyContinue | Format-Table IPAddress, PrefixLength
Get-NetConnectionProfile -InterfaceAlias WeftCheck -ErrorAction SilentlyContinue | Format-List InterfaceAlias, NetworkCategory
Get-NetIPInterface -InterfaceAlias WeftCheck -AddressFamily IPv4 -ErrorAction SilentlyContinue | Format-Table InterfaceAlias, InterfaceMetric
Get-NetRoute -InterfaceAlias WeftCheck -AddressFamily IPv4 -ErrorAction SilentlyContinue | Format-Table DestinationPrefix, RouteMetric
Get-NetFirewallRule -DisplayName 'Weft', 'Weft daemon' -ErrorAction SilentlyContinue | Format-Table DisplayName, Enabled, Direction, Action
$processes | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep 1
foreach ($name in 'loom', 'a', 'b') {
    Section "$name.log"
    Get-Content "$work\$name.log" -ErrorAction SilentlyContinue
}

if ($Link) {
    Section 'Service'
    & (Join-Path $root 'weftd.exe') service install 2>&1 | ForEach-Object { "$_" }
    Start-Sleep 3
    Get-Service weftd | Format-List Name, Status, StartType
    Weft '\\.\pipe\weftd' up $Link
    if ($Password) {
        Weft '\\.\pipe\weftd' join $Network --password $Password
    }
    for ($i = 0; $i -lt 30; $i++) {
        if ((Weft '\\.\pipe\weftd' status) -match 'online') { break }
        Start-Sleep 1
    }
    Section 'Status (English)'
    Weft '\\.\pipe\weftd' status
    Section 'Status (system language)'
    $env:WEFT_SOCKET = '\\.\pipe\weftd'
    & (Join-Path $root 'weft.exe') status
    Remove-Item Env:WEFT_SOCKET
    foreach ($line in (Weft '\\.\pipe\weftd' status)) {
        if ($line -match '^\s+\S+\s+(100\.\d+\.\d+\.\d+)\s+online') {
            Write-Host "ping $($Matches[1])"
            ping -n 4 $Matches[1]
        }
    }
    Section 'LAN discovery'
    Write-Host 'Sending broadcast and multicast discovery packets for 10 s.'
    Write-Host 'On the other machine run: python3 tools/lan-probe.py listen 15'
    $udp = New-Object System.Net.Sockets.UdpClient
    $udp.EnableBroadcast = $true
    $broadcast = [Text.Encoding]::ASCII.GetBytes('broadcast')
    $multicast = [Text.Encoding]::ASCII.GetBytes('multicast')
    for ($i = 0; $i -lt 20; $i++) {
        [void]$udp.Send($broadcast, $broadcast.Length, '255.255.255.255', 4445)
        [void]$udp.Send($multicast, $multicast.Length, '224.0.2.60', 4445)
        Start-Sleep -Milliseconds 500
    }
    $udp.Close()
    Get-NetIPInterface -InterfaceAlias Weft -AddressFamily IPv4 -ErrorAction SilentlyContinue | Format-Table InterfaceAlias, InterfaceMetric
    Get-NetRoute -InterfaceAlias Weft -AddressFamily IPv4 -ErrorAction SilentlyContinue | Format-Table DestinationPrefix, RouteMetric
    Section 'Service log'
    Get-Content "$env:ProgramData\Weft\weftd.log" -Tail 100 -ErrorAction SilentlyContinue
    if (-not $KeepService) {
        Weft '\\.\pipe\weftd' down
        & (Join-Path $root 'weftd.exe') service uninstall 2>&1 | ForEach-Object { "$_" }
    }
}

Stop-Transcript | Out-Null
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "`nDone. Send $log"
