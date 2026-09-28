# Cortex installer for Windows (PowerShell 5.1 or 7+).
#
#   irm https://raw.githubusercontent.com/AstroQuestStudio/cortex/main/install.ps1 | iex
#
# What it does, and nothing else:
#   1. downloads cortex-x86_64-pc-windows-msvc.zip and SHA256SUMS.txt from the GitHub release,
#   2. refuses to continue unless the SHA-256 checksum matches,
#   3. copies cortex.exe into %LOCALAPPDATA%\cortex\bin (no administrator rights needed),
#   4. adds that folder to your user PATH (not the system PATH), once.
#
# Options (environment variables, set before running):
#   CORTEX_INSTALL_DIR      install folder (default: %LOCALAPPDATA%\cortex\bin)
#   CORTEX_VERSION          release to install, e.g. 0.3.0 (default: latest)
#   CORTEX_NO_MODIFY_PATH=1 do not touch the user PATH
#
# Source: https://github.com/AstroQuestStudio/cortex (PolyForm Shield 1.0.0). Made by AstroQuest.

& {
    Set-StrictMode -Version 2.0
    $ErrorActionPreference = 'Stop'
    $oldProgress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is much faster without it

    $repo = 'AstroQuestStudio/cortex'
    $version = if ($env:CORTEX_VERSION) { $env:CORTEX_VERSION } else { 'latest' }
    $installDir = if ($env:CORTEX_INSTALL_DIR) { $env:CORTEX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'cortex\bin' }
    $modifyPath = -not ($env:CORTEX_NO_MODIFY_PATH -in @('1', 'true', 'yes'))

    function Fail([string]$msg) { throw "cortex-install: $msg" }

    # --- platform ---------------------------------------------------------------
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Write-Host 'Note: no native ARM64 build yet; the x64 build runs under Windows emulation.' }
        default { Fail "unsupported CPU architecture: $arch (64-bit Windows is required)" }
    }
    $archive = 'cortex-x86_64-pc-windows-msvc.zip'
    $base = if ($version -eq 'latest') {
        "https://github.com/$repo/releases/latest/download"
    } elseif ($version.StartsWith('v')) {
        "https://github.com/$repo/releases/download/$version"
    } else {
        "https://github.com/$repo/releases/download/v$version"
    }

    # Windows PowerShell 5.1 may default to TLS 1.0/1.1, which GitHub refuses.
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch { }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('cortex-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Cortex installer: $archive ($version)"
        Write-Host "  downloading from $base/"
        $zip = Join-Path $tmp $archive
        $sums = Join-Path $tmp 'SHA256SUMS.txt'
        foreach ($pair in @(@("$base/$archive", $zip), @("$base/SHA256SUMS.txt", $sums))) {
            try {
                Invoke-WebRequest -Uri $pair[0] -OutFile $pair[1] -UseBasicParsing
            } catch {
                Fail "download failed: $($pair[0]) ($($_.Exception.Message))"
            }
        }

        # --- verify ---------------------------------------------------------------
        $expected = $null
        foreach ($line in Get-Content -LiteralPath $sums) {
            $parts = $line.Trim() -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $archive) { $expected = $parts[0].ToLowerInvariant(); break }
        }
        if (-not $expected) { Fail "$archive is not listed in SHA256SUMS.txt" }
        $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            Fail "checksum mismatch for $archive (expected $expected, got $actual). Nothing was installed."
        }
        Write-Host "  sha256 verified: $actual"

        $extract = Join-Path $tmp 'x'
        Expand-Archive -LiteralPath $zip -DestinationPath $extract -Force
        $exe = Join-Path $extract 'cortex.exe'
        if (-not (Test-Path -LiteralPath $exe)) { Fail "cortex.exe not found in $archive" }
        $oldEncoding = $null
        try { $oldEncoding = [Console]::OutputEncoding; [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }
        try {
            $installed = & $exe --version
        } finally {
            if ($oldEncoding) { try { [Console]::OutputEncoding = $oldEncoding } catch { } }
        }
        if ($LASTEXITCODE -ne 0) { Fail 'the downloaded cortex.exe does not run on this system' }

        # --- install --------------------------------------------------------------
        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $target = Join-Path $installDir 'cortex.exe'
        if (Test-Path -LiteralPath $target) {
            # A running cortex.exe (an MCP server, for instance) cannot be overwritten, but it can
            # be renamed: move it aside, then remove it if nothing holds it any more.
            $old = "$target.old"
            Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
            Move-Item -LiteralPath $target -Destination $old -Force
            Copy-Item -LiteralPath $exe -Destination $target
            Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
        } else {
            Copy-Item -LiteralPath $exe -Destination $target
        }
        Write-Host "  installed $installed -> $target"
        Write-Host ''

        # --- PATH -----------------------------------------------------------------
        $full = [IO.Path]::GetFullPath($installDir).TrimEnd('\')
        $inSession = @($env:Path -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ieq $full }).Count -gt 0
        if ($modifyPath) {
            # Read the raw user PATH (without expanding %VARIABLES%) and keep its registry type.
            $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
            try {
                $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                $entries = @($raw -split ';' | Where-Object { $_ })
                $present = @($entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $full }).Count -gt 0
                if (-not $present) {
                    $new = (@($entries) + $full) -join ';'
                    $kind = if ($key.GetValueNames() -contains 'Path') { $key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
                    $key.SetValue('Path', $new, $kind)
                    # Broadcast the change so new terminals see it (no-op on a missing variable).
                    [Environment]::SetEnvironmentVariable('CORTEX_INSTALL_REFRESH', $null, 'User')
                    Write-Host "  added $full to your user PATH (open a new terminal to use it everywhere)"
                }
            } finally {
                $key.Close()
            }
            if (-not $inSession) { $env:Path = "$env:Path;$full" }
        } elseif (-not $inSession) {
            Write-Host "$full is not on your PATH (CORTEX_NO_MODIFY_PATH is set). Call it by full path or add it yourself."
        }

        Write-Host ''
        Write-Host 'Next steps:'
        Write-Host '  cd your-project; cortex index . --name MyProject'
        Write-Host '  cortex find "where are sessions signed"'
        Write-Host '  claude mcp add --scope user cortex -- cortex mcp    # or any MCP client: command "cortex", args ["mcp"]'
        Write-Host ''
        Write-Host "Docs: https://github.com/$repo#readme"
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
        $ProgressPreference = $oldProgress
    }
}
