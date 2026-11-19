<#
.SYNOPSIS
  DroidPath Zero-Friction 1-Click Installer for Windows (zero Node.js required).
.EXAMPLE
  irm https://raw.githubusercontent.com/V0val0x/DroidPath/main/scripts/install.ps1 | iex
  .\scripts\install.ps1 -Deploy -ConfigureMcp all
  .\scripts\install.ps1 -FromLocal . -ConfigureMcp cursor
#>
[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$FromLocal = "",
    [switch]$Deploy,
    [ValidateSet("", "cursor", "claude", "all")]
    [string]$ConfigureMcp = "",
    [ValidateRange(1, 65535)]
    [int]$DaemonPort = 27189,
    [string]$Serial = ""
)

$ErrorActionPreference = "Stop"

$Repo = "V0val0x/DroidPath"
$InstallRoot = if ($env:DROIDPATH_HOME) { $env:DROIDPATH_HOME } else { Join-Path $env:USERPROFILE ".droidpath" }
$BinDir = Join-Path $InstallRoot "bin"
$ShareDir = Join-Path $InstallRoot "share"
New-Item -ItemType Directory -Force -Path $BinDir, $ShareDir | Out-Null

$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
$Target = if ($Arch -eq "arm64") { "aarch64-pc-windows-msvc.exe" } else { "x86_64-pc-windows-msvc.exe" }

$McpDest = Join-Path $BinDir "droidpath-mcp.exe"
$JarDest = Join-Path $ShareDir "android-daemon.jar"

if ($FromLocal -ne "") {
    $LocalBin = Join-Path $FromLocal "target\release\droidpath-mcp.exe"
    if (-not (Test-Path $LocalBin)) {
        $LocalBin = Join-Path $FromLocal "target\debug\droidpath-mcp.exe"
    }
    $LocalJar = Join-Path $FromLocal "android-daemon\build\dist\android-daemon.jar"
    if (-not (Test-Path $LocalJar)) {
        $LocalJar = Join-Path $FromLocal "packages\droidpath-cli\assets\android-daemon.jar"
    }
    if (-not (Test-Path $LocalBin)) { throw "Local droidpath-mcp.exe not found in $FromLocal\target" }
    if (-not (Test-Path $LocalJar)) { throw "Local android-daemon.jar not found in $FromLocal" }
    Copy-Item -Force $LocalBin $McpDest
    Copy-Item -Force $LocalJar $JarDest
} else {
    $BaseUrl = if ($Version -eq "latest") {
        "https://github.com/$Repo/releases/latest/download"
    } else {
        "https://github.com/$Repo/releases/download/$Version"
    }
    Write-Host "Downloading droidpath-mcp ($Target)..."
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/droidpath-mcp-$Target" -OutFile $McpDest
    } catch {
        if ($Arch -eq "arm64") {
            Write-Host "ARM64 binary unavailable, falling back to x86_64-pc-windows-msvc.exe..." -ForegroundColor Yellow
            Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/droidpath-mcp-x86_64-pc-windows-msvc.exe" -OutFile $McpDest
        } else {
            throw
        }
    }
    Write-Host "Downloading android-daemon.jar..."
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/android-daemon.jar" -OutFile $JarDest
}

Write-Host "Installed droidpath-mcp.exe -> $McpDest" -ForegroundColor Green
Write-Host "Installed android-daemon.jar -> $JarDest" -ForegroundColor Green

# Idempotently add $BinDir to User PATH
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($UserPath -split ";" | Where-Object { $_ -eq $BinDir })) {
    $NewPath = if ([string]::IsNullOrWhiteSpace($UserPath)) { $BinDir } else { "$UserPath;$BinDir" }
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host "Added $BinDir to User PATH (restart terminal to take effect)." -ForegroundColor Cyan
}

function Set-McpClientConfig {
    param([string]$ConfigPath)
    $Dir = Split-Path -Parent $ConfigPath
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null

    $Doc = [ordered]@{}
    if (Test-Path $ConfigPath) {
        $Raw = [System.IO.File]::ReadAllText($ConfigPath, [System.Text.Encoding]::UTF8).TrimStart([char]0xFEFF).Trim()
        if ($Raw.Length -gt 0) {
            $Parsed = $Raw | ConvertFrom-Json
            if ($null -eq $Parsed -or ($Parsed -is [System.Collections.IList]) -or -not ($Parsed -is [System.Management.Automation.PSCustomObject])) {
                throw "Refusing to overwrite non-object JSON in $ConfigPath"
            }
            foreach ($prop in $Parsed.PSObject.Properties) {
                $Doc[$prop.Name] = $prop.Value
            }
        }
    }

    $Servers = [ordered]@{}
    if ($Doc.Contains("mcpServers") -and $null -ne $Doc["mcpServers"]) {
        $ExistingServers = $Doc["mcpServers"]
        if (($ExistingServers -is [System.Collections.IList]) -or -not ($ExistingServers -is [System.Management.Automation.PSCustomObject])) {
            throw "Invalid 'mcpServers' object in $ConfigPath"
        }
        foreach ($prop in $ExistingServers.PSObject.Properties) {
            $Servers[$prop.Name] = $prop.Value
        }
    }

    $ArgsList = @("--stdio", "--daemon-jar", $JarDest)
    if ($DaemonPort -ne 27189) {
        $ArgsList += @("--daemon-port", "$DaemonPort")
    }
    $Servers["droidpath"] = [ordered]@{
        command = $McpDest
        args    = $ArgsList
    }
    $Doc["mcpServers"] = $Servers

    $TmpPath = "$ConfigPath.$PID.tmp"
    $JsonText = ($Doc | ConvertTo-Json -Depth 10) + "`n"
    $Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($TmpPath, $JsonText, $Utf8NoBom)
    Move-Item -Force $TmpPath $ConfigPath
    Write-Host "Configured MCP client -> $ConfigPath" -ForegroundColor Green
}

if ($ConfigureMcp -eq "" -and $env:DROIDPATH_CONFIGURE_MCP) {
    $ConfigureMcp = $env:DROIDPATH_CONFIGURE_MCP
}
if (-not $Deploy -and $env:DROIDPATH_DEPLOY -eq "1") {
    $Deploy = [switch]::Present
}

if ($ConfigureMcp -eq "cursor" -or $ConfigureMcp -eq "all") {
    Set-McpClientConfig (Join-Path $env:USERPROFILE ".cursor\mcp.json")
}
if ($ConfigureMcp -eq "claude" -or $ConfigureMcp -eq "all") {
    $AppDataDir = if ($env:APPDATA) { $env:APPDATA } else { Join-Path $env:USERPROFILE "AppData\Roaming" }
    $ClaudeDir = Join-Path $AppDataDir "Claude"
    Set-McpClientConfig (Join-Path $ClaudeDir "claude_desktop_config.json")
}

if ($Deploy) {
    $DeployArgs = @("deploy", "--daemon-port", "$DaemonPort", "--daemon-jar", $JarDest)
    if ($Serial -ne "") {
        $DeployArgs += @("--serial", $Serial)
    }
    & $McpDest @DeployArgs
}
