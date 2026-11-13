# Installs the latest discord-mcp release on Windows.
#   irm https://raw.githubusercontent.com/DeityLamb/discord-mcp/main/install.ps1 | iex
# Set $env:DISCORD_MCP_VERSION = "v0.1.0" for a given release, $env:DISCORD_MCP_INSTALL_DIR for another
# place than %LOCALAPPDATA%\Programs\discord-mcp.
$ErrorActionPreference = "Stop"

$repo = "DeityLamb/discord-mcp"
$dir = if ($env:DISCORD_MCP_INSTALL_DIR) { $env:DISCORD_MCP_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\discord-mcp" }
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }
$target = "$arch-pc-windows-msvc"

$version = $env:DISCORD_MCP_VERSION
if (-not $version) {
    $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}

$name = "discord-mcp-$version-$target"
$base = "https://github.com/$repo/releases/download/$version"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    Write-Host "Downloading discord-mcp $version for $target"
    Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
    $expected = (Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape("$name.zip"))$" }) -split " " | Select-Object -First 1
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "discord-mcp: checksum mismatch, not installing" }

    Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item "$tmp\$name\discord-mcp.exe" $dir -Force
    Write-Host "Installed $dir\discord-mcp.exe"

    $path = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($path -split ";") -notcontains $dir) {
        [Environment]::SetEnvironmentVariable("Path", "$path;$dir", "User")
        Write-Host "Added $dir to your PATH; open a new terminal to use it"
    }
} finally {
    Remove-Item -Recurse -Force $tmp
}
