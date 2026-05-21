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
Step "Assemble desktop/src-tauri/resources/"
if (Test-Path $ResourceDir) { Remove-Item -Recurse -Force $ResourceDir }
New-Item -ItemType Directory -Path $ResourceDir | Out-Null

# 4a. node/
$resNodeDir = Join-Path $ResourceDir "node"
New-Item -ItemType Directory -Path $resNodeDir | Out-Null
Copy-Item -Path (Join-Path $nodeExtractDir "node.exe") -Destination $resNodeDir
Info "Copied node.exe ($([math]::Round((Get-Item (Join-Path $resNodeDir 'node.exe')).Length / 1MB, 1)) MB)"

# 4b. openclaw/ (npm-prebuilt package + its node_modules siblings under openclaw-runtime/node_modules)
$resOpenClawDir = Join-Path $ResourceDir "openclaw"
Info "Mirroring openclaw npm runtime (package + deps) ..."
# Copy the openclaw package itself.
$null = & robocopy $OpenClawRuntime $resOpenClawDir /MIR /NJH /NJS /NP /NDL /XD .git test __screenshots__ 2>&1
if ($LASTEXITCODE -ge 8) { throw "robocopy openclaw failed: $LASTEXITCODE" }
$global:LASTEXITCODE = 0

# Copy the sibling node_modules (npm hoists deps to the top-level node_modules).
$runtimeRoot = Split-Path -Parent (Split-Path -Parent $OpenClawRuntime) # -> openclaw-runtime/
$siblingNodeModules = Join-Path $runtimeRoot "node_modules"
$destNodeModules = Join-Path $resOpenClawDir "node_modules"
# Avoid duplicating openclaw inside its own node_modules.
$null = & robocopy $siblingNodeModules $destNodeModules /MIR /NJH /NJS /NP /NDL /XD openclaw .git 2>&1
if ($LASTEXITCODE -ge 8) { throw "robocopy node_modules failed: $LASTEXITCODE" }
$global:LASTEXITCODE = 0

$sizeMb = [math]::Round((Get-ChildItem $resOpenClawDir -Recurse | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
Info "openclaw/ runtime bundle size: $sizeMb MB"

Step "Done. Next: cd desktop && pnpm tauri build"
