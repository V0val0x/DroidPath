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
    [ValidateSet("", "cursor", "claude", "gemini", "antigravity", "all")]
    [string]$ConfigureMcp = "",
    [ValidateRange(1, 65535)]
    [int]$DaemonPort = 27189,
    [string]$Serial = "",
    [switch]$AllowUnverified,
    [string]$ReleaseBaseUrl = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Ensure TLS 1.2+ on Windows PowerShell 5.1 (GitHub Releases rejects TLS 1.0/1.1)
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # Ignore on platforms/runtimes where ServicePointManager is obsolete or unavailable
}

$Repo = "V0val0x/DroidPath"
$InstallRoot = if ($env:DROIDPATH_HOME) { $env:DROIDPATH_HOME } else { Join-Path $env:USERPROFILE ".droidpath" }
$BinDir = Join-Path $InstallRoot "bin"
$ShareDir = Join-Path $InstallRoot "share"
New-Item -ItemType Directory -Force -Path $BinDir, $ShareDir | Out-Null

$Arch = ""
try {
    $Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
} catch {
    $Arch = ""
}
if ([string]::IsNullOrWhiteSpace($Arch)) {
    $ProcArch = if ($env:PROCESSOR_ARCHITECTURE) { $env:PROCESSOR_ARCHITECTURE.ToLowerInvariant() } else { "amd64" }
    $Arch = if ($ProcArch -eq "arm64") { "arm64" } else { "x64" }
}
$Target = if ($Arch -eq "arm64") { "aarch64-pc-windows-msvc.exe" } else { "x86_64-pc-windows-msvc.exe" }

$McpDest = Join-Path $BinDir "droidpath-mcp.exe"
$JarDest = Join-Path $ShareDir "android-daemon.jar"

function Parse-ChecksumsManifest {
    param([object]$ManifestContent)
    $ManifestText = if ($ManifestContent -is [byte[]]) {
        [System.Text.Encoding]::UTF8.GetString($ManifestContent)
    } else {
        [string]$ManifestContent
    }
    $ManifestText = $ManifestText.TrimStart([char]0xFEFF)
    $Map = @{}
    foreach ($rawLine in ($ManifestText -split "`r?`n")) {
        $line = $rawLine.Trim()
        if ($line.Length -eq 0 -or $line.StartsWith("#")) { continue }
        if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
            $hash = $Matches[1].ToLowerInvariant()
            $name = $Matches[2].Trim() -replace '^\./', ''
            if ($name.Length -gt 0) {
                $Map[$name] = $hash
            }
        }
    }
    return $Map
}

function Resolve-PinnedReleaseBaseUrl {
    param([string]$InitialBaseUrl)
    $InitialUri = [Uri]("$InitialBaseUrl/checksums.txt")
    $InitialOrigin = $InitialUri.GetLeftPart([System.UriPartial]::Authority)
    $CurrentUri = $InitialUri
    $PinnedBase = $InitialBaseUrl

    for ($hop = 0; $hop -lt 5; $hop++) {
        $Location = $null
        try {
            $Req = [System.Net.HttpWebRequest]::Create($CurrentUri)
            $Req.Method = "HEAD"
            $Req.AllowAutoRedirect = $false
            $Req.UserAgent = "DroidPath-Installer"
            $Req.Timeout = 15000
            try {
                $Resp = $Req.GetResponse()
                try {
                    $StatusCode = [int]$Resp.StatusCode
                    if ($StatusCode -ge 300 -and $StatusCode -lt 400) {
                        $Location = $Resp.Headers["Location"]
                    }
                } finally {
                    $Resp.Close()
                }
            } catch [System.Net.WebException] {
                $WebResp = $_.Exception.Response
                if ($null -ne $WebResp) {
                    try {
                        $StatusCode = [int]$WebResp.StatusCode
                        if ($StatusCode -ge 300 -and $StatusCode -lt 400) {
                            $Location = $WebResp.Headers["Location"]
                        }
                    } finally {
                        $WebResp.Close()
                    }
                }
            }
        } catch {
            break
        }

        if ([string]::IsNullOrWhiteSpace($Location)) {
            break
        }
        $NextUri = New-Object System.Uri($CurrentUri, $Location)
        $NextOrigin = $NextUri.GetLeftPart([System.UriPartial]::Authority)
        $NextPath = $NextUri.AbsolutePath
        if ($NextOrigin -eq $InitialOrigin -and $NextPath.EndsWith("/checksums.txt")) {
            $PinnedBase = ($NextOrigin + $NextPath.Substring(0, $NextPath.Length - "/checksums.txt".Length)).TrimEnd("/")
            $CurrentUri = $NextUri
            continue
        }
        # Stop following once redirected off-origin (e.g. GitHub S3 release-assets CDN)
        break
    }
    return $PinnedBase
}

function Get-FileSha256Hex {
    param([string]$FilePath)
    $Sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $Stream = [System.IO.File]::OpenRead($FilePath)
        try {
            $HashBytes = $Sha.ComputeHash($Stream)
            return ([System.BitConverter]::ToString($HashBytes) -replace '-', '').ToLowerInvariant()
        } finally {
            $Stream.Dispose()
        }
    } finally {
        $Sha.Dispose()
    }
}

function Assert-AssetSha256 {
    param(
        [string]$FilePath,
        [hashtable]$Checksums,
        [string]$AssetName
    )
    if (-not $Checksums.ContainsKey($AssetName)) {
        throw "Asset '$AssetName' is missing from checksums.txt. Refusing unverified install (pass -AllowUnverified or set DROIDPATH_ALLOW_UNVERIFIED=1 to override)."
    }
    $Expected = $Checksums[$AssetName].ToLowerInvariant()
    $Actual = Get-FileSha256Hex -FilePath $FilePath
    if ($Actual -ne $Expected) {
        throw "SHA-256 verification failed for '$AssetName': expected $Expected, got $Actual. Refusing to install tampered or corrupted artifact."
    }
    Write-Host ("Verified SHA-256 for {0} ({1}...)" -f $AssetName, $Actual.Substring(0, 16)) -ForegroundColor Green
}

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
    $EffectiveAllowUnverified = $AllowUnverified.IsPresent -or ($env:DROIDPATH_ALLOW_UNVERIFIED -eq "1")
    $BaseUrl = if ($ReleaseBaseUrl -ne "") {
        $ReleaseBaseUrl.TrimEnd("/")
    } elseif ($env:DROIDPATH_RELEASE_BASE_URL) {
        $env:DROIDPATH_RELEASE_BASE_URL.TrimEnd("/")
    } elseif ($Version -eq "latest") {
        "https://github.com/$Repo/releases/latest/download"
    } else {
        "https://github.com/$Repo/releases/download/$Version"
    }

    $StageDir = Join-Path ([System.IO.Path]::GetTempPath()) ("droidpath-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

    try {
        $Checksums = $null
        if (-not $EffectiveAllowUnverified) {
            # Pin /releases/latest/download -> /releases/download/<tag> BEFORE following to S3 CDN
            $BaseUrl = Resolve-PinnedReleaseBaseUrl -InitialBaseUrl $BaseUrl
            $ChecksumsUri = "$BaseUrl/checksums.txt"
            $StagedChecksums = Join-Path $StageDir "checksums.txt"
            Write-Host "Fetching SHA-256 checksums.txt..."
            try {
                Invoke-WebRequest -UseBasicParsing -Uri $ChecksumsUri -OutFile $StagedChecksums
            } catch {
                throw "Failed to fetch SHA-256 manifest ($ChecksumsUri). Refusing unverified download (pass -AllowUnverified or set DROIDPATH_ALLOW_UNVERIFIED=1 to override). $($_.Exception.Message)"
            }
            $ManifestRaw = [System.IO.File]::ReadAllText($StagedChecksums, [System.Text.Encoding]::UTF8)
            $Checksums = Parse-ChecksumsManifest $ManifestRaw
        } else {
            Write-Host "WARNING: SHA-256 verification bypassed (-AllowUnverified / DROIDPATH_ALLOW_UNVERIFIED=1)." -ForegroundColor Yellow
        }

        $McpAsset = "droidpath-mcp-$Target"
        $JarAsset = "android-daemon.jar"
        $StagedMcp = Join-Path $StageDir "droidpath-mcp.exe"
        $StagedJar = Join-Path $StageDir "android-daemon.jar"

        if ($Arch -eq "arm64" -and $null -ne $Checksums -and -not $Checksums.ContainsKey($McpAsset) -and $Checksums.ContainsKey("droidpath-mcp-x86_64-pc-windows-msvc.exe")) {
            $McpAsset = "droidpath-mcp-x86_64-pc-windows-msvc.exe"
            Write-Host "ARM64 binary not listed in checksums.txt; falling back to $McpAsset..." -ForegroundColor Yellow
        }

        Write-Host "Downloading $McpAsset..."
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$McpAsset" -OutFile $StagedMcp
        } catch {
            if ($Arch -eq "arm64" -and $McpAsset -ne "droidpath-mcp-x86_64-pc-windows-msvc.exe") {
                $McpAsset = "droidpath-mcp-x86_64-pc-windows-msvc.exe"
                Write-Host "ARM64 binary unavailable, falling back to $McpAsset..." -ForegroundColor Yellow
                Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$McpAsset" -OutFile $StagedMcp
            } else {
                throw
            }
        }

        Write-Host "Downloading $JarAsset..."
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$JarAsset" -OutFile $StagedJar

        if (-not $EffectiveAllowUnverified) {
            Assert-AssetSha256 -FilePath $StagedMcp -Checksums $Checksums -AssetName $McpAsset
            Assert-AssetSha256 -FilePath $StagedJar -Checksums $Checksums -AssetName $JarAsset
        }

        Move-Item -Force $StagedMcp $McpDest
        Move-Item -Force $StagedJar $JarDest
    } finally {
        if (Test-Path $StageDir) {
            Remove-Item -Recurse -Force $StageDir -ErrorAction SilentlyContinue
        }
    }
}

Write-Host "Installed droidpath-mcp.exe -> $McpDest" -ForegroundColor Green
Write-Host "Installed android-daemon.jar -> $JarDest" -ForegroundColor Green

# Idempotently add $BinDir to User PATH
if ($env:DROIDPATH_SKIP_PATH_UPDATE -ne "1") {
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($UserPath -split ";" | Where-Object { $_ -eq $BinDir })) {
        $NewPath = if ([string]::IsNullOrWhiteSpace($UserPath)) { $BinDir } else { "$UserPath;$BinDir" }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "Added $BinDir to User PATH (restart terminal to take effect)." -ForegroundColor Cyan
    }
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
    $EnvCfg = $env:DROIDPATH_CONFIGURE_MCP.Trim().ToLowerInvariant()
    if ($EnvCfg -notin @("cursor", "claude", "gemini", "antigravity", "all")) {
        throw "Invalid DROIDPATH_CONFIGURE_MCP value '$EnvCfg' (expected cursor, claude, gemini, antigravity, or all)."
    }
    $ConfigureMcp = $EnvCfg
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
if ($ConfigureMcp -eq "gemini" -or $ConfigureMcp -eq "all") {
    Set-McpClientConfig (Join-Path $env:USERPROFILE ".gemini\settings.json")
}
if ($ConfigureMcp -eq "antigravity" -or $ConfigureMcp -eq "all") {
    Set-McpClientConfig (Join-Path $env:USERPROFILE ".gemini\antigravity\mcp_config.json")
}

if ($Deploy) {
    $DeployArgs = @("deploy", "--daemon-port", "$DaemonPort", "--daemon-jar", $JarDest)
    if ($Serial -ne "") {
        $DeployArgs += @("--serial", $Serial)
    }
    & $McpDest @DeployArgs
}
