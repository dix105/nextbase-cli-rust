# Nextbase CLI (Wisper) installer for Windows.
#
#   iwr -useb https://raw.githubusercontent.com/dix105/nextbase-cli-rust/main/install.ps1 | iex
#
# Prefers a prebuilt binary, so neither git nor a Rust toolchain is needed. Falls
# back to building from source only if no binary exists.

$ErrorActionPreference = 'Stop'

$Repo    = if ($env:WISPER_REPO)    { $env:WISPER_REPO }    else { 'dix105/nextbase-cli-rust' }
$Version = if ($env:WISPER_VERSION) { $env:WISPER_VERSION } else { 'latest' }
$BinDir  = if ($env:WISPER_BIN_DIR) { $env:WISPER_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\Wisper' }

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("wisper-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null

function Say  ($m) { Write-Host $m }
function Ok   ($m) { Write-Host "√ $m" -ForegroundColor Green }
function Warn ($m) { Write-Host "! $m" -ForegroundColor Yellow }
function Die  ($m) { Write-Host "x $m" -ForegroundColor Red; exit 1 }

$Target = if ([Environment]::Is64BitOperatingSystem) {
  'x86_64-pc-windows-msvc'
} else {
  Die 'Only 64-bit Windows is supported.'
}

# A running listener holds the old executable open, so Windows refuses to replace
# it. Stop it before swapping anything.
function Stop-Listener {
  try {
    $existing = Join-Path $BinDir 'wisper.exe'
    if (Test-Path $existing) { & $existing stop 2>$null | Out-Null }
  } catch {}
  Get-Process -Name 'wisper', 'nextbase' -ErrorAction SilentlyContinue |
    Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
}

function Install-Prebuilt {
  $url = if ($Version -eq 'latest') {
    "https://github.com/$Repo/releases/latest/download/nextbase-wisper-$Target.zip"
  } else {
    "https://github.com/$Repo/releases/download/$Version/nextbase-wisper-$Target.zip"
  }

  Say "Looking for a prebuilt binary for $Target..."
  $zip = Join-Path $Tmp 'wisper.zip'
  try {
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
  } catch {
    return $false
  }

  Expand-Archive -Force -Path $zip -DestinationPath (Join-Path $Tmp 'unpacked')
  $exe = Get-ChildItem -Path (Join-Path $Tmp 'unpacked') -Filter 'wisper.exe' -Recurse |
    Select-Object -First 1
  if (-not $exe) { return $false }

  New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
  Stop-Listener
  Copy-Item -Force $exe.FullName (Join-Path $BinDir 'wisper.exe')
  $umbrella = Get-ChildItem -Path (Join-Path $Tmp 'unpacked') -Filter 'nextbase.exe' -Recurse |
    Select-Object -First 1
  if ($umbrella) { Copy-Item -Force $umbrella.FullName (Join-Path $BinDir 'nextbase.exe') }
  return $true
}

function Install-FromSource {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { return $false }

  Say 'No prebuilt binary available. Building from source with cargo...'
  $branch = if ($env:WISPER_BRANCH) { $env:WISPER_BRANCH } else { 'main' }
  $src = Join-Path $Tmp 'src.zip'
  try {
    # Source archive rather than `git clone`, so git is not required.
    Invoke-WebRequest -UseBasicParsing -OutFile $src `
      -Uri "https://codeload.github.com/$Repo/zip/refs/heads/$branch"
  } catch {
    return $false
  }

  Expand-Archive -Force -Path $src -DestinationPath (Join-Path $Tmp 'source')
  $root = Get-ChildItem -Path (Join-Path $Tmp 'source') -Directory | Select-Object -First 1
  if (-not $root) { return $false }

  Stop-Listener
  Push-Location $root.FullName
  try {
    cargo install --path crates/nextbase-cli --locked --root (Join-Path $Tmp 'out')
    if ($LASTEXITCODE -ne 0) { return $false }
  } finally {
    Pop-Location
  }

  New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
  Copy-Item -Force (Join-Path $Tmp 'out\bin\wisper.exe')   (Join-Path $BinDir 'wisper.exe')
  Copy-Item -Force (Join-Path $Tmp 'out\bin\nextbase.exe') (Join-Path $BinDir 'nextbase.exe')
  return $true
}

try {
  if (Install-Prebuilt) {
    Ok 'Installed a prebuilt binary.'
  } elseif (Install-FromSource) {
    Ok 'Built and installed from source.'
  } else {
    Say ''
    Die @"
Could not install.
No prebuilt binary exists for $Target yet, and building from source needs cargo.
Install Rust from https://rustup.rs and re-run this script.
"@
  }

  $installed = & (Join-Path $BinDir 'wisper.exe') --version
  Ok "$installed at $BinDir\wisper.exe"

  # Persist PATH for future sessions, and fix the current one.
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable('Path', "$BinDir;$userPath", 'User')
    Warn "Added $BinDir to your user PATH. Open a new terminal to pick it up."
  }
  $env:Path = "$BinDir;$env:Path"

  Say ''
  Say 'Next:'
  Say '  wisper setup     Choose a model, paste an API key, pick a shortcut'
  Say '  wisper doctor    Check microphone and shortcuts'
  Say '  wisper listen    Start the background listener'
} finally {
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
