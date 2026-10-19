<#
  Fabrica el instalador de viralshorts: dist\viralshorts-setup-<version>.exe

  Es lo que se le da a quien quiera usar el programa: un unico .exe que se
  ejecuta, instala viralshorts como cualquier programa (menu Inicio, Escritorio,
  desinstalador) y listo. No hace falta Python, ni Rust, ni nada.

  Solo lo necesitas tu, que lo compilas. Hace falta:
    - Rust       (https://rustup.rs)
    - Inno Setup (https://jrsoftware.org/isdl.php), la herramienta que arma el .exe

      powershell -ExecutionPolicy Bypass -File install\crear-instalador.ps1
      powershell -ExecutionPolicy Bypass -File install\crear-instalador.ps1 -SinCompilar
#>

[CmdletBinding()]
param(
    # No compilar el programa: usar el .exe que ya haya en rust\target\release.
    [switch]$SinCompilar
)

$ErrorActionPreference = 'Stop'

$Raiz = Split-Path -Parent $PSScriptRoot
$Exe  = Join-Path $Raiz 'rust\target\release\viralshorts.exe'

# --- version, de rust\Cargo.toml -------------------------------------------------
$cargo = Get-Content -LiteralPath (Join-Path $Raiz 'rust\Cargo.toml') -Raw
$version = if ($cargo -match '(?m)^version\s*=\s*"([^"]+)"') { $Matches[1] } else { '0.1.0' }

# --- el programa -------------------------------------------------------------------
if (-not $SinCompilar) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'No encuentro cargo. Instala Rust desde https://rustup.rs o usa -SinCompilar.'
    }
    Write-Host 'Compilando viralshorts (cargo build --release)...' -ForegroundColor Cyan
    Push-Location (Join-Path $Raiz 'rust')
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build fallo con codigo $LASTEXITCODE" }
    } finally { Pop-Location }
}
if (-not (Test-Path -LiteralPath $Exe)) { throw "No existe $Exe" }

# --- Inno Setup ----------------------------------------------------------------------
$candidatos = @(
    (Get-Command iscc -ErrorAction SilentlyContinue | ForEach-Object Source),
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path -LiteralPath $_) }
$iscc = $candidatos | Select-Object -First 1
if (-not $iscc) {
    throw 'No encuentro Inno Setup (ISCC.exe). Instalalo desde https://jrsoftware.org/isdl.php y vuelve a ejecutar esto.'
}

Write-Host "Armando el instalador con $iscc ..." -ForegroundColor Cyan
& $iscc "/DAppVersion=$version" "/DExePath=$Exe" (Join-Path $PSScriptRoot 'viralshorts.iss')
if ($LASTEXITCODE -ne 0) { throw "Inno Setup fallo con codigo $LASTEXITCODE" }

$setup = Join-Path $Raiz "dist\viralshorts-setup-$version.exe"
$mb = [math]::Round((Get-Item -LiteralPath $setup).Length / 1MB, 1)
Write-Host ''
Write-Host '==========================================================' -ForegroundColor Green
Write-Host "  Instalador listo ($mb MB):" -ForegroundColor Green
Write-Host "  $setup" -ForegroundColor White
Write-Host '==========================================================' -ForegroundColor Green
