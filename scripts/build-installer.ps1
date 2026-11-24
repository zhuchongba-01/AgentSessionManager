$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$releaseExe = Join-Path $projectRoot "src-tauri\target\release\agent-session-manager.exe"
$installerScript = Join-Path $projectRoot "installer\AgentSessionManager.iss"
$innoFromPath = Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1
$innoCandidates = @(
    $innoFromPath,
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
    (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe")
) | Where-Object { $_ }
$innoCompiler = $innoCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1

if (-not $innoCompiler) {
    throw "Inno Setup 6 (ISCC.exe) was not found."
}

$tempTauriOverride = $null

Push-Location $projectRoot
try {
    & node (Join-Path $PSScriptRoot "version.mjs") --check
    if ($LASTEXITCODE -ne 0) {
        throw "Release versions are inconsistent. Run pnpm version:set X.Y.Z before packaging."
    }

    & node (Join-Path $PSScriptRoot "csp-hash.mjs")
    if ($LASTEXITCODE -ne 0) {
        throw "Inline-script CSP hash check failed."
    }

    # Do not replace this with `cargo build --release`: Tauri's production
    # build step enables the embedded custom protocol and bundles dist/ into
    # the executable. A plain Cargo build opens the development localhost URL.
    if (Get-Command pnpm -ErrorAction SilentlyContinue) {
        & pnpm tauri build --no-bundle --ignore-version-mismatches -- --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Tauri production build failed with exit code $LASTEXITCODE."
        }
    }
    else {
        & npx --no-install vite build
        if ($LASTEXITCODE -ne 0) {
            throw "Frontend production build failed with exit code $LASTEXITCODE."
        }

        $tempTauriOverride = Join-Path ([System.IO.Path]::GetTempPath()) "asm-tauri-override-$PID-$([Guid]::NewGuid().ToString('N')).json"
        [System.IO.File]::WriteAllText(
            $tempTauriOverride,
            '{"build":{"beforeBuildCommand":null}}',
            [System.Text.UTF8Encoding]::new($false)
        )

        & npx --no-install tauri build --no-bundle --ignore-version-mismatches -c $tempTauriOverride -- --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Tauri production build failed with exit code $LASTEXITCODE."
        }
    }
    if (-not (Test-Path -LiteralPath $releaseExe)) {
        throw "The production executable was not generated: $releaseExe"
    }

    & $innoCompiler $installerScript
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup failed with exit code $LASTEXITCODE."
    }
}
finally {
    if ($tempTauriOverride -and (Test-Path -LiteralPath $tempTauriOverride)) {
        Remove-Item -LiteralPath $tempTauriOverride -Force -ErrorAction SilentlyContinue
    }
    Pop-Location
}
