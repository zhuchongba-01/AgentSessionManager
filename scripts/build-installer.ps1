$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$releaseExe = Join-Path $projectRoot "src-tauri\target\release\agent-session-manager.exe"
$installerScript = Join-Path $projectRoot "installer\AgentSessionManager.iss"
$innoCandidates = @(
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
    (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe")
)
$innoCompiler = $innoCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1

if (-not $innoCompiler) {
    throw "Inno Setup 6 (ISCC.exe) was not found."
}

Push-Location $projectRoot
try {
    # Do not replace this with `cargo build --release`: Tauri's production
    # build step enables the embedded custom protocol and bundles dist/ into
    # the executable. A plain Cargo build opens the development localhost URL.
    & pnpm tauri build --no-bundle
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri production build failed with exit code $LASTEXITCODE."
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
    Pop-Location
}
