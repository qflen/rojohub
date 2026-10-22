# Installs the RojoHub binary from GitHub Releases into %USERPROFILE%\.rojohub\bin, verifies
# its SHA-256, and adds that folder to the user's PATH.
#
#   irm https://github.com/qflen/rojohub/releases/latest/download/install.ps1 | iex
#
#   $env:ROJOHUB_VERSION = "0.4.0"   install that release instead of the latest
$ErrorActionPreference = "Stop"
$repo = if ($env:ROJOHUB_REPO) { $env:ROJOHUB_REPO } else { "qflen/rojohub" }
$version = if ($env:ROJOHUB_VERSION) { $env:ROJOHUB_VERSION } else { "latest" }
$base = if ($version -eq "latest") { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/v$version" }
$asset = "rojohub-windows-x64.exe"
$dir = if ($env:ROJOHUB_INSTALL_DIR) { $env:ROJOHUB_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".rojohub\bin" }
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$tmp = Join-Path $dir "rojohub.download"

Write-Host "Downloading $asset from $base"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $tmp
$sums = (Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS").Content
$want = ($sums -split "`n" | Where-Object { $_ -match "\s\*?$([regex]::Escape($asset))\s*$" } | ForEach-Object { ($_.Trim() -split "\s+")[0].ToLower() } | Select-Object -First 1)
$got = (Get-FileHash -Path $tmp -Algorithm SHA256).Hash.ToLower()
if (-not $want -or $want -ne $got) {
  Remove-Item -Force $tmp
  throw "checksum mismatch for $asset (expected '$want', got '$got')"
}
Move-Item -Force -Path $tmp -Destination (Join-Path $dir "rojohub.exe")

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $dir) {
  [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
  $env:Path = "$env:Path;$dir"
  Write-Host "Added $dir to your PATH (open a new terminal for other windows to see it)."
}
$installed = & (Join-Path $dir "rojohub.exe") version
Write-Host "Installed rojohub $installed to $dir\rojohub.exe"
Write-Host ""
Write-Host "Next:"
Write-Host "  rojohub install-plugin     # RojoHub plugin + RojoHub sync plugin into Studio"
Write-Host "  rojohub install-service    # run the hub at login"
Write-Host "  rojohub mcp-config         # register the MCP server with your agent"
