<#
  Crea el ZIP que se le manda a otra persona.

  Se lleva solo lo necesario: el ejecutable, los lanzadores y el instalador. Nada
  de videos, cache, herramientas ya descargadas ni historial de git. Las
  herramientas (ffmpeg, yt-dlp, whisper.cpp) las baja el instalador en el
  ordenador del otro.

  Si hay Rust instalado se compila antes el ejecutable, para que el paquete sea
  siempre de la ultima version del codigo.
#>

[CmdletBinding()]
param(
    # Donde dejar el ZIP. Por defecto, el Escritorio.
    [string]$Destino = [Environment]::GetFolderPath('Desktop'),
    # No compilar: usar el ejecutable que ya haya.
    [switch]$SinCompilar
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Raiz = Split-Path -Parent $PSScriptRoot
$nombre = 'viralshorts-' + (Get-Date -Format 'yyyy-MM-dd')
$zip = Join-Path $Destino "$nombre.zip"
$tmp = Join-Path $env:TEMP $nombre

Write-Host ""
Write-Host "Preparando el paquete..." -ForegroundColor Cyan

# --- el ejecutable -----------------------------------------------------------
$release = Join-Path $Raiz 'rust\target\release'
$exe = Join-Path $release 'viralshorts.exe'
if (-not $SinCompilar -and (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "  compilando (cargo build --release)..." -ForegroundColor DarkGray
    Push-Location (Join-Path $Raiz 'rust')
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build fallo con codigo $LASTEXITCODE" }
    } finally { Pop-Location }
}
if (-not (Test-Path -LiteralPath $exe)) {
    throw "No hay viralshorts.exe en $release. Instala Rust (https://rustup.rs) y vuelve a intentarlo."
}

if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force }
New-Item -ItemType Directory -Path $tmp | Out-Null

# Lo que SI viaja. Lista blanca a proposito: es mas seguro enumerar lo que se
# manda que intentar acordarse de todo lo que no.
Copy-Item -LiteralPath $exe -Destination $tmp
foreach ($f in @('viralshorts.bat', 'EMPIEZA AQUI.txt', 'config.yaml', 'README.md')) {
    $origen = Join-Path $Raiz $f
    if (Test-Path -LiteralPath $origen) {
        Copy-Item -LiteralPath $origen -Destination $tmp
    } else {
        Write-Host "  aviso: falta $f" -ForegroundColor Yellow
    }
}

# El instalador viaja entero menos el log y la marca del equipo de origen: si se
# colara la marca, en el ordenador del otro el programa creeria que ya esta
# instalado y arrancaria sin herramientas.
$destinoInstall = Join-Path $tmp 'install'
New-Item -ItemType Directory -Path $destinoInstall | Out-Null
Get-ChildItem -LiteralPath $PSScriptRoot -File |
    Where-Object { $_.Name -notin @('log-instalacion.txt', '.instalado') } |
    Copy-Item -Destination $destinoInstall

# Claves de API: no se mandan nunca, aunque esten en el config del que empaqueta.
$cfg = Join-Path $tmp 'config.yaml'
if (Test-Path -LiteralPath $cfg) {
    $texto = Get-Content -LiteralPath $cfg -Raw
    $limpio = [regex]::Replace(
        $texto,
        '(?im)^(\s*(?:api_key|anthropic_api_key|token)\s*:\s*).+$',
        '${1}""'
    )
    if ($limpio -ne $texto) {
        Set-Content -LiteralPath $cfg -Value $limpio -Encoding utf8
        Write-Host "  se ha quitado la clave de API del config.yaml del paquete" -ForegroundColor Yellow
    }
}

if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path (Join-Path $tmp '*') -DestinationPath $zip
Remove-Item -LiteralPath $tmp -Recurse -Force

$mb = [math]::Round((Get-Item -LiteralPath $zip).Length / 1MB, 1)

Write-Host ""
Write-Host "==========================================================" -ForegroundColor Green
Write-Host "  Paquete listo ($mb MB):" -ForegroundColor Green
Write-Host "  $zip" -ForegroundColor White
Write-Host "==========================================================" -ForegroundColor Green
Write-Host ""
Write-Host "  Mandaselo por WeTransfer, Drive o el correo si cabe."
Write-Host "  La otra persona solo tiene que:"
Write-Host "    1. descomprimir el ZIP entero (boton derecho > Extraer todo)"
Write-Host "    2. doble clic en  viralshorts.bat"
Write-Host ""
