# share installer for Windows  --  https://github.com/rootagi/share
#
#   irm https://raw.githubusercontent.com/rootagi/share/main/install.ps1 | iex
#
# Optional environment variables (set before running):
#   $env:SHARE_VERSION      release tag to install, e.g. v0.1.0   (default: latest)
#   $env:SHARE_INSTALL_DIR  install folder   (default: %LOCALAPPDATA%\Programs\share)
#   $env:NO_COLOR = '1'     plain ASCII output, no fancy symbols
#   $env:SHARE_VCREDIST     '1' = install the Visual C++ Runtime without asking if it is missing,
#                           '0' = never offer to install it
#
# share itself installs per-user (no admin) and adds the folder to your user PATH.
# If the Microsoft Visual C++ Runtime is missing, the script offers to install it (needs one UAC prompt).

function Install-Share {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference    = 'SilentlyContinue'   # much faster downloads on Windows PowerShell 5.1

    # Windows PowerShell 5.1 may default to old TLS versions that GitHub rejects.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

    $repo   = 'rootagi/share'
    $bin    = 'share'
    $total  = 7
    $script:stepNo = 0
    $t0     = Get-Date

    # ---- styling: unicode only on terminals that render it well -------------
    $uni = (-not $env:NO_COLOR) -and ($env:WT_SESSION -or $env:TERM_PROGRAM -or $PSVersionTable.PSVersion.Major -ge 7)
    $oldEnc = $null
    if ($uni) {
        try { $oldEnc = [Console]::OutputEncoding; [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { $uni = $false }
    }
    if ($uni) {
        $ARROW = [string][char]0x25B8; $OK = [string][char]0x2714; $BAD = [string][char]0x2716
        $WARN  = [string][char]0x25B2; $DOT = [string][char]0x2022; $INFO = [string][char]0x2139
        $TL = [string][char]0x256D; $TR = [string][char]0x256E; $BL = [string][char]0x2570; $BR = [string][char]0x256F
        $H  = [string][char]0x2500; $V  = [string][char]0x2502
    } else {
        $ARROW = '>'; $OK = '+'; $BAD = 'x'; $WARN = '!'; $DOT = '-'; $INFO = 'i'
        $TL = '+'; $TR = '+'; $BL = '+'; $BR = '+'; $H = '-'; $V = '|'
    }
    $hl = $H * 48

    # ---- output helpers -----------------------------------------------------
    function Say([string]$text, [string]$color = 'Gray', [switch]$NoNewLine) {
        if ($NoNewLine) { Write-Host $text -ForegroundColor $color -NoNewline } else { Write-Host $text -ForegroundColor $color }
    }
    function Blank { Write-Host '' }
    function Step([string]$title) {
        $script:stepNo++
        Blank
        Say "$ARROW [$($script:stepNo)/$total] $title" 'Cyan'
    }
    function Info([string]$label, [string]$value) {
        Say ('    ' + $label.PadRight(13) + ' ') 'DarkGray' -NoNewLine
        Say $value 'Gray'
    }
    function Ok([string]$text)   { Say '    ' 'Gray' -NoNewLine; Say $OK 'Green' -NoNewLine; Say " $text" 'Gray' }
    function Warn([string]$text) { Say "    $WARN $text" 'Yellow' }
    function Note([string]$text) { Say "    $text" 'DarkGray' }
    function Req([string]$kind, [string]$label, [string]$text) {
        switch ($kind) {
            'ok'   { $sym = $OK;   $col = 'Green' }
            'warn' { $sym = $WARN; $col = 'Yellow' }
            default{ $sym = $INFO; $col = 'Magenta' }
        }
        Say '    ' 'Gray' -NoNewLine
        Say $sym $col -NoNewLine
        Say (' ' + $label.PadRight(18)) 'White' -NoNewLine
        Say $text 'Gray'
    }
    function Human([long]$bytes) {
        if ($bytes -ge 1MB) { '{0:N1} MB' -f ($bytes / 1MB) } else { '{0:N0} KB' -f ($bytes / 1KB) }
    }
    function Short([string]$hash) { $hash.Substring(0, 16) + [string][char]0x2026 + $hash.Substring(56, 8) }
    function BoxRow([string]$text, [string]$color) {
        Say "  $V " 'Cyan' -NoNewLine
        Say $text.PadRight(46) $color -NoNewLine
        Say " $V" 'Cyan'
    }

    $verified = $false
    $runs     = $false
    $tmp      = $null
    $vcState  = 'present'   # present | installed | missing | skipped

    try {
        # ---- banner ---------------------------------------------------------
        Blank
        Say "  $TL$hl$TR" 'Cyan'
        BoxRow 'share  -  LAN file server & live monitor' 'White'
        BoxRow 'installer for Windows' 'DarkGray'
        Say "  $BL$hl$BR" 'Cyan'
        Blank
        Say '  This script will:' 'White'
        Say "    $DOT check that your system has what it needs"
        Say "    $DOT download the official release from github.com/$repo"
        Say "    $DOT verify it against the published SHA-256 checksum"
        Say "    $DOT copy one file ($bin.exe) into a folder of your own"
        Say '  share itself needs no admin rights. Only if the Microsoft Visual C++ Runtime is missing, we will offer to install it (one admin prompt).' 'DarkGray'

        # ---- 1. requirements ------------------------------------------------
        Step 'Checking requirements'
        if (-not (Get-Command Expand-Archive -ErrorAction SilentlyContinue)) { throw 'Expand-Archive is missing. Please use Windows PowerShell 5.0 or newer.' }
        if (-not (Get-Command Get-FileHash   -ErrorAction SilentlyContinue)) { throw 'Get-FileHash is missing. Please use Windows PowerShell 4.0 or newer.' }
        Ok "PowerShell $($PSVersionTable.PSVersion)"
        Ok 'Unzip and checksum tools available (built into Windows)'

        # temp folder is created up front: the Visual C++ installer (if needed) is saved here too
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ('share-install-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $tmp | Out-Null
        $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }

        # share.exe is built with the MSVC toolchain and needs vcruntime140.dll.
        # Fresh Windows installs often lack it ("VCRUNTIME140.dll was not found").
        $sysDir = if (Test-Path (Join-Path $env:windir 'Sysnative')) { Join-Path $env:windir 'Sysnative' } else { Join-Path $env:windir 'System32' }
        $vcDll  = Join-Path $sysDir 'vcruntime140.dll'
        if (Test-Path $vcDll) {
            Ok 'Microsoft Visual C++ Runtime found'
        } else {
            $vcState = 'missing'
            Warn 'Microsoft Visual C++ Runtime is NOT installed (share.exe needs it to start)'
            $vcFile = if ($arch -eq 'ARM64') { 'vc_redist.arm64.exe' } else { 'vc_redist.x64.exe' }
            $vcUrl  = "https://aka.ms/vs/17/release/$vcFile"
            Note "official Microsoft installer: $vcUrl"

            $answer = $env:SHARE_VCREDIST
            if (-not $answer) {
                try { $r = Read-Host '    Install it now? Windows will ask for admin approval [Y/n]'; $answer = if ($r -match '^\s*[nN]') { '0' } else { '1' } }
                catch { $answer = '0'; Note 'not an interactive session, so not asking' }
            }

            if ($answer -eq '1') {
                try {
                    $vcPath = Join-Path $tmp $vcFile
                    Note 'downloading the Visual C++ Runtime from Microsoft...'
                    Invoke-WebRequest -Uri $vcUrl -OutFile $vcPath -UseBasicParsing
                    Ok "Downloaded $(Human (Get-Item $vcPath).Length)"

                    $sig = Get-AuthenticodeSignature -FilePath $vcPath
                    if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
                        throw 'the downloaded file is not signed by Microsoft, refusing to run it'
                    }
                    Ok 'Digital signature is valid (Microsoft Corporation)'

                    Note 'installing... approve the Windows (UAC) prompt if it appears'
                    $proc = Start-Process -FilePath $vcPath -ArgumentList '/install', '/quiet', '/norestart' -Verb RunAs -Wait -PassThru
                    # 0 = ok, 3010 = ok but reboot suggested, 1638 = a newer version is already installed
                    if (@(0, 1638, 3010) -contains $proc.ExitCode -and (Test-Path $vcDll)) {
                        Ok 'Visual C++ Runtime installed'
                        if ($proc.ExitCode -eq 3010) { Warn 'Windows suggests a restart to finish the Visual C++ setup' }
                        $vcState = 'installed'
                    } else {
                        throw "the installer finished with code $($proc.ExitCode)"
                    }
                } catch {
                    Warn "Could not install it automatically: $($_.Exception.Message)"
                    Note "Install it manually from $vcUrl and run share again."
                }
            } else {
                $vcState = 'skipped'
                Warn 'Skipping. share will not start until the Visual C++ Runtime is installed:'
                Note "  $vcUrl"
            }
        }
        Note 'Rust is NOT needed here: this installer downloads a prebuilt binary.'

        # ---- 2. detect CPU --------------------------------------------------
        Step 'Detecting your system'
        switch ($arch) {
            'AMD64' { $cpu = 'x86_64';  $cpuName = '64-bit Intel/AMD' }
            'ARM64' { $cpu = 'aarch64'; $cpuName = '64-bit ARM' }
            default { throw "Unsupported CPU architecture '$arch' (released builds: x86_64, aarch64). Build from source instead (needs Rust 1.85+): cargo install --git https://github.com/$repo" }
        }
        $target = "$cpu-pc-windows-msvc"
        Info 'OS'    'Windows'
        Info 'CPU'   "$cpuName ($arch)"
        Info 'Build' $target

        # ---- 3. resolve version --------------------------------------------
        Step 'Finding the release'
        Info 'Repository' "github.com/$repo"
        $tag = $env:SHARE_VERSION
        if (-not $tag) {
            Info 'Requested' 'latest release'
            Note "following github.com/$repo/releases/latest (no API, no rate limit)"
            $resp  = Invoke-WebRequest -Uri "https://github.com/$repo/releases/latest" -Method Head -UseBasicParsing
            $final = if ($resp.BaseResponse.ResponseUri) { $resp.BaseResponse.ResponseUri.AbsoluteUri }
                     else { $resp.BaseResponse.RequestMessage.RequestUri.AbsoluteUri }
            $tag = ($final -split '/tag/')[-1]
            if (-not $tag -or $tag -eq $final) { throw "Could not determine the latest release of $repo. Check your internet connection, or pin one: `$env:SHARE_VERSION='v0.1.0'" }
        } else {
            Info 'Requested' "$tag (pinned via SHARE_VERSION)"
        }
        if (-not $tag.StartsWith('v')) { $tag = "v$tag" }
        $name = "$bin-$tag-$target.zip"
        $base = "https://github.com/$repo/releases/download/$tag"
        Ok "Version $tag"

        # ---- 4. download ----------------------------------------------------
        Step 'Downloading'
        $zip = Join-Path $tmp $name
        Info 'File' $name
        Info 'From' "$base/"
        Info 'To'   "$tmp (temporary)"
        Note 'downloading, please wait...'
        try { Invoke-WebRequest -Uri "$base/$name" -OutFile $zip -UseBasicParsing }
        catch { throw "Download failed: $base/$name`nDoes release $tag exist? See https://github.com/$repo/releases`n$($_.Exception.Message)" }
        Ok "Downloaded $(Human (Get-Item $zip).Length)"

        # ---- 5. verify checksum --------------------------------------------
        Step 'Verifying integrity'
        $sumsFile = Join-Path $tmp 'SHA256SUMS'
        $haveSums = $true
        Info 'Checksum file' "$base/SHA256SUMS"
        try { Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsFile -UseBasicParsing } catch { $haveSums = $false }
        if ($haveSums) {
            $line = Get-Content $sumsFile | Where-Object { $_ -like "*$name*" } | Select-Object -First 1
            if (-not $line) { throw "SHA256SUMS has no entry for $name. The release may be incomplete: https://github.com/$repo/issues" }
            $expected = ([regex]::Match($line, '[0-9a-fA-F]{64}')).Value.ToLower()
            $actual   = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
            Info 'Expected' (Short $expected)
            Info 'Computed' (Short $actual)
            if ($actual -ne $expected) { throw "Checksum mismatch - the download is corrupted or has been tampered with. Nothing was installed." }
            Ok 'SHA-256 matches, the file is intact'
            $verified = $true
        } else {
            Warn 'Could not fetch SHA256SUMS, skipping verification'
        }

        # ---- 6. unpack ------------------------------------------------------
        Step 'Unpacking'
        $out = Join-Path $tmp 'x'
        Expand-Archive -Path $zip -DestinationPath $out -Force
        $files = @(Get-ChildItem -Path $out -Recurse -File)
        Info 'Archive' "$($files.Count) file(s)"
        $files | Select-Object -First 6 | ForEach-Object { Note ("  $DOT " + $_.FullName.Substring($out.Length + 1)) }
        if ($files.Count -gt 6) { Note "  ... and $($files.Count - 6) more" }
        $exe = Get-ChildItem -Path $out -Recurse -Filter "$bin.exe" | Select-Object -First 1
        if (-not $exe) { throw "Could not find $bin.exe inside $name. Please report it at https://github.com/$repo/issues" }
        Ok "Found the binary: $($exe.FullName.Substring($out.Length + 1))"

        # ---- 7. install + PATH + final check -------------------------------
        Step 'Installing'
        if ($env:SHARE_INSTALL_DIR) { $dir = $env:SHARE_INSTALL_DIR; $why = 'from SHARE_INSTALL_DIR' }
        else { $dir = Join-Path $env:LOCALAPPDATA 'Programs\share'; $why = 'your user folder, no admin needed' }
        Info 'Folder' $dir
        Note $why
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $dest = Join-Path $dir "$bin.exe"
        if (Test-Path $dest) { Warn "Replacing existing install at $dest" }
        try { Copy-Item -Path $exe.FullName -Destination $dest -Force }
        catch { throw "Could not write $dest. If share is currently running, close it and try again.`n$($_.Exception.Message)" }
        Ok "Copied to $dest"

        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $parts = if ($userPath) { $userPath -split ';' | Where-Object { $_ } } else { @() }
        $addedToPath = $false
        if ($parts -notcontains $dir) {
            [Environment]::SetEnvironmentVariable('Path', (($parts + $dir) -join ';'), 'User')
            $addedToPath = $true
            Ok 'Added the folder to your user PATH'
        } else {
            Ok 'Folder is already on your user PATH'
        }
        if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }

        $ver = $null
        if ($vcState -eq 'present' -or $vcState -eq 'installed') {
            try { $ver = (& $dest --version 2>$null | Select-Object -First 1) } catch { }
            if ($ver) { Ok "Binary runs: $ver"; $runs = $true }
            else { Warn "Installed, but '$bin --version' did not run here" }
        } else {
            Warn "Skipped the '$bin --version' test (it would fail without the Visual C++ Runtime)"
        }

        # ---- summary --------------------------------------------------------
        $secs = [int]((Get-Date) - $t0).TotalSeconds
        Blank
        Say "  $OK share $tag installed " 'Green' -NoNewLine
        Say "in ${secs}s" 'DarkGray'
        if ($addedToPath) {
            Blank
            Warn 'Open a NEW terminal window to use "share" from anywhere.'
        }

        Blank
        Say '  Get started' 'White'
        Say '    $ share $HOME\Downloads' 'Cyan'  -NoNewLine; Say '         serve a folder on your network' 'DarkGray'
        Say '    $ share .\file.iso --qr' 'Cyan'   -NoNewLine; Say '         share one file with a QR code' 'DarkGray'
        Say '    $ share --help' 'Cyan'            -NoNewLine; Say '                  all options' 'DarkGray'

        Blank
        Say '  Requirements' 'White'
        Req 'ok' 'Rust / Cargo' 'not needed - you installed a prebuilt binary'
        if (Get-Command rustc -ErrorAction SilentlyContinue) { Note '  (Rust is on this machine, but share does not use it)' }
        switch ($vcState) {
            'present'   { Req 'ok'   'Visual C++ Runtime' 'found on this PC (no OpenSSL, Node.js or Python needed)' }
            'installed' { Req 'ok'   'Visual C++ Runtime' 'was missing - installed by this script' }
            default     {
                Req 'warn' 'Visual C++ Runtime' 'MISSING - share.exe will not start without it'
                Say ('    ' + (' ' * 21) + "Install: $vcUrl") 'DarkGray'
            }
        }
        if ($verified) { Req 'ok' 'Integrity' 'SHA-256 checksum verified' }
        else           { Req 'warn' 'Integrity' 'checksum was NOT verified - consider re-running' }
        if (-not $runs) {
            if ($vcState -eq 'present' -or $vcState -eq 'installed') {
                Req 'warn' 'Binary check' "could not run '$bin --version' - see the warning above"
            }
        }
        Req 'info' 'Build from source' 'only if you want to: Rust 1.85+ (https://rustup.rs), then'
        Say ('    ' + (' ' * 21) + "cargo install --git https://github.com/$repo") 'DarkGray'
        Req 'info' 'Homebrew' 'macOS/Linux only: brew install rootagi/tap/share'

        Blank
        Say '  Good to know' 'White'
        Say "    $DOT Windows Firewall will ask to allow share on first run."
        Say '      Choose "Private networks".' 'DarkGray'
        Say "    $DOT First HTTPS visit shows a self-signed certificate warning."
        Say '      Choose Advanced > Proceed, or use --http if you do not need encryption.' 'DarkGray'
        Blank
        Say "  Docs & issues: https://github.com/$repo" 'DarkGray'
        Blank
    }
    catch {
        Blank
        Say "  $BAD error: $($_.Exception.Message)" 'Red'
        Blank
    }
    finally {
        if ($tmp) { Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue }
        if ($oldEnc) { try { [Console]::OutputEncoding = $oldEnc } catch { } }
    }
}

Install-Share
