<#
.SYNOPSIS
  Prepare OpenClaw + portable Node.js for bundling into the Tauri MSI/NSIS installer.

.DESCRIPTION
  Steps (Windows host):
    1. Verify openclaw/ source is present.
    2. Run pnpm install in openclaw/ (production deps + native rebuild) if not already done.
    3. Run pnpm ui:build to compile Control UI into openclaw/dist/control-ui.
    4. Download portable Node.js for win-x64 into portable-node/ (cached).
    5. Copy openclaw/ (source + dist + node_modules) + portable node into desktop/src-tauri/resources/.

  Output layout after run:
    desktop/src-tauri/resources/
      node/node.exe
      openclaw/
        openclaw.mjs
        package.json
        dist/                  (build output incl. control-ui)
        node_modules/          (production deps with .node native bindings)
        ...

.PARAMETER NodeVersion
  Node.js version to bundle. Default matches host (auto-detected) so native bindings stay ABI-compatible.

.PARAMETER SkipInstall
  Skip pnpm install (use if openclaw/node_modules already populated).

.PARAMETER SkipUiBuild
  Skip pnpm ui:build (use if openclaw/dist/control-ui already exists).
#>

param(
  [string] $NodeVersion = "",
  [switch] $SkipInstall,
  [switch] $SkipUiBuild
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
# Runtime comes from the npm-published package (preinstalled into openclaw-runtime/),
# not from the openclaw/ source clone — upstream `pnpm build` crashes on Windows+Node 24.
$OpenClawRuntime = Join-Path $ProjectRoot "openclaw-runtime2\node_modules\openclaw"
$ResourceDir = Join-Path $ProjectRoot "desktop\src-tauri\resources"
$PortableDir = Join-Path $ProjectRoot "portable-node"

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Info($msg) { Write-Host "    $msg" -ForegroundColor Gray }

# ---- 0. Pre-flight ----
Step "Pre-flight checks"
if (-not (Test-Path $OpenClawRuntime)) {
  throw "OpenClaw runtime not found at $OpenClawRuntime. Run:`n  mkdir openclaw-runtime; cd openclaw-runtime; npm init -y; npm install openclaw@latest"
}
if (-not (Test-Path (Join-Path $OpenClawRuntime "dist\control-ui"))) {
  throw "openclaw/dist/control-ui missing — npm package layout unexpected at $OpenClawRuntime"
}
if ([string]::IsNullOrEmpty($NodeVersion)) {
  $NodeVersion = (& node -v).TrimStart('v')
  Info "Auto-detected host Node version: $NodeVersion (will bundle matching portable Node for ABI compatibility)"
}
Info "OpenClaw runtime: $OpenClawRuntime ($(Get-ChildItem $OpenClawRuntime\package.json | Select-String '"version"' | ForEach-Object { $_.Line.Trim() }))"

# ---- 3. Download portable Node ----
Step "Acquire portable Node.js v$NodeVersion (win-x64)"
$nodeZipName = "node-v$NodeVersion-win-x64.zip"
$nodeUrl = "https://nodejs.org/dist/v$NodeVersion/$nodeZipName"
$nodeZipPath = Join-Path $PortableDir $nodeZipName
$nodeExtractDir = Join-Path $PortableDir "node-v$NodeVersion-win-x64"

if (-not (Test-Path $nodeExtractDir)) {
  if (-not (Test-Path $PortableDir)) { New-Item -ItemType Directory -Path $PortableDir | Out-Null }
  if (-not (Test-Path $nodeZipPath)) {
    Info "Downloading $nodeUrl"
    Invoke-WebRequest -Uri $nodeUrl -OutFile $nodeZipPath -UseBasicParsing
  }
  Info "Extracting to $nodeExtractDir"
  Expand-Archive -Path $nodeZipPath -DestinationPath $PortableDir -Force
} else {
  Info "Already extracted at $nodeExtractDir"
}

# ---- 4. Assemble resources/ ----
# NOTE: do NOT bulk Remove-Item the resources dir first — on Windows an
# antivirus scan or a leftover sidecar can hold a handle on it and make the
# delete fail. robocopy /MIR below mirrors (adds new, removes stale) per-file
# with retries, which is robust against transient locks.
Step "Assemble desktop/src-tauri/resources/"
if (-not (Test-Path $ResourceDir)) { New-Item -ItemType Directory -Path $ResourceDir | Out-Null }

# 4a. node/
$resNodeDir = Join-Path $ResourceDir "node"
if (-not (Test-Path $resNodeDir)) { New-Item -ItemType Directory -Path $resNodeDir | Out-Null }
Copy-Item -Path (Join-Path $nodeExtractDir "node.exe") -Destination $resNodeDir -Force
Info "Copied node.exe ($([math]::Round((Get-Item (Join-Path $resNodeDir 'node.exe')).Length / 1MB, 1)) MB)"

# Bundle npm CLI + shell wrappers. Without these, openclaw's npm-runner falls
# back to the user's system npm (or throws on Windows where there's no
# fallback) — observed failure was @openai/codex installing without its
# darwin-arm64 native binary. See scripts/prepare-bundle.sh for the full
# rationale.
$npmSrc = Join-Path $nodeExtractDir "node_modules\npm"
$npmDest = Join-Path $resNodeDir "node_modules\npm"
if (Test-Path $npmSrc) {
  if (-not (Test-Path (Join-Path $resNodeDir "node_modules"))) {
    New-Item -ItemType Directory -Path (Join-Path $resNodeDir "node_modules") | Out-Null
  }
  $null = & robocopy $npmSrc $npmDest /E /R:2 /W:2 /NJH /NJS /NP /NDL 2>&1
  if ($LASTEXITCODE -ge 8) { throw "robocopy npm failed: $LASTEXITCODE" }
  $global:LASTEXITCODE = 0
  $npmMb = [math]::Round((Get-ChildItem $npmDest -Recurse | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
  Info "Copied npm CLI ($npmMb MB)"
}
foreach ($f in @("npm", "npm.cmd", "npm.ps1", "npx", "npx.cmd", "npx.ps1")) {
  $src = Join-Path $nodeExtractDir $f
  if (Test-Path $src) {
    Copy-Item -Path $src -Destination $resNodeDir -Force
  }
}

# 4b. openclaw/ (npm package + its deps — layout depends on whether openclaw
#     ships npm-shrinkwrap.json: with shrinkwrap deps are BUNDLED inside
#     openclaw/node_modules/, without shrinkwrap deps are HOISTED to top-level
#     openclaw-runtime2/node_modules/. We take the UNION of both layouts.)
$resOpenClawDir = Join-Path $ResourceDir "openclaw"
Info "Mirroring openclaw npm runtime (package + deps) ..."
# Pre-clean dest so stale state doesn't survive into the new build.
if (Test-Path $resOpenClawDir) {
  Remove-Item -Recurse -Force $resOpenClawDir -ErrorAction SilentlyContinue
}

# Copy the openclaw package (incl. any bundled node_modules inside it). NO /MIR
# on the 2nd pass below, so we can layer hoisted deps on top non-destructively.
$null = & robocopy $OpenClawRuntime $resOpenClawDir /E /R:2 /W:2 /NJH /NJS /NP /NDL /XD .git test __screenshots__ 2>&1
if ($LASTEXITCODE -ge 8) { throw "robocopy openclaw failed: $LASTEXITCODE" }
$global:LASTEXITCODE = 0
$bundledCount = 0
$bundledNodeModules = Join-Path $resOpenClawDir "node_modules"
if (Test-Path $bundledNodeModules) {
  $bundledCount = (Get-ChildItem $bundledNodeModules -Force | Measure-Object).Count
}
Info "openclaw package copied — $bundledCount entries already inside node_modules/ (bundled)"

# Layer top-level hoisted deps onto whatever was bundled. /XC /XN /XO together
# mean "don't overwrite files that already exist in dest" (no robocopy flag is
# directly equivalent to rsync's --ignore-existing — this combination is).
$runtimeRoot = Split-Path -Parent (Split-Path -Parent $OpenClawRuntime)
$siblingNodeModules = Join-Path $runtimeRoot "node_modules"
$destNodeModules = Join-Path $resOpenClawDir "node_modules"
if (-not (Test-Path $destNodeModules)) { New-Item -ItemType Directory -Path $destNodeModules | Out-Null }
$null = & robocopy $siblingNodeModules $destNodeModules /E /R:2 /W:2 /NJH /NJS /NP /NDL /XC /XN /XO /XD openclaw .git 2>&1
if ($LASTEXITCODE -ge 8) { throw "robocopy node_modules failed: $LASTEXITCODE" }
$global:LASTEXITCODE = 0
$totalCount = (Get-ChildItem $destNodeModules -Force | Measure-Object).Count
Info "after hoisted merge: $totalCount entries in node_modules/"

# Strip compile-time-only files: TypeScript declarations + sourcemaps.
# Node never loads these at runtime, so this is NOT a feature cut (no .js removed).
# It also shortens deep SDK paths that otherwise hit Windows MAX_PATH in NSIS.
Step "Strip .d.ts / .map (compile-time only; runtime unaffected)"
Get-ChildItem $resOpenClawDir -Recurse -File -Include *.map, *.d.ts, *.d.cts, *.d.mts -ErrorAction SilentlyContinue |
  Remove-Item -Force -ErrorAction SilentlyContinue

# Drop nested @mistralai duplicates (pi-coding-agent's copy has very long SDK
# filenames that break Windows NSIS MAX_PATH). The top-level @mistralai (same
# 2.2.1) resolves via normal Node lookup → zero functional impact.
$topMistral = Join-Path $resOpenClawDir "node_modules\@mistralai"
Get-ChildItem $resOpenClawDir -Recurse -Directory -Filter "@mistralai" -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -ne $topMistral } |
  ForEach-Object { Info "drop nested $($_.FullName)"; Remove-Item -Recurse -Force $_.FullName -ErrorAction SilentlyContinue }

$sizeMb = [math]::Round((Get-ChildItem $resOpenClawDir -Recurse | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
Info "openclaw/ runtime bundle size: $sizeMb MB"

# Defense in depth: confirm the critical runtime deps actually landed. If json5
# is missing the embedded Node will crash with ERR_MODULE_NOT_FOUND on first
# `config patch` call. Fail the build loudly here, not silently in the .exe.
foreach ($dep in @("json5", "tokenjuice", "@mistralai/mistralai")) {
  $depPath = Join-Path $resOpenClawDir "node_modules/$dep"
  if (-not (Test-Path $depPath)) {
    Write-Host "ERROR: required dep '$dep' missing from $resOpenClawDir/node_modules/" -ForegroundColor Red
    Get-ChildItem (Join-Path $resOpenClawDir "node_modules") | Select-Object -First 30 | ForEach-Object { Write-Host "  $_" }
    throw "bundle verification failed"
  }
}
Info "verified critical deps: json5, tokenjuice, @mistralai/mistralai"

Step "Done. Next: cd desktop && pnpm tauri build"
