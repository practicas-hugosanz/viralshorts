<#
  Crea el ZIP que se le manda a otra persona.

  Se lleva solo lo necesario para que el programa se instale y funcione en el
  ordenador del otro: nada de entorno virtual, videos, cache ni historial de
  git. Eso son gigas que ademas no sirven en otra maquina (el .venv apunta a
  rutas absolutas del ordenador donde se creo).
#>

[CmdletBinding()]
param(
    # Donde dejar el ZIP. Por defecto, el Escritorio.
    [string]$Destino = [Environment]::GetFolderPath('Desktop')
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Raiz = Split-Path -Parent $PSScriptRoot
$nombre = 'viralshorts-' + (Get-Date -Format 'yyyy-MM-dd')
$zip = Join-Path $Destino "$nombre.zip"
$tmp = Join-Path $env:TEMP $nombre

Write-Host ""
Write-Host "Preparando el paquete..." -ForegroundColor Cyan

if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force }
New-Item -ItemType Directory -Path $tmp | Out-Null

# Lo que SI viaja. Lista blanca a proposito: es mas seguro enumerar lo que se
# manda que intentar acordarse de todo lo que no.
$ficheros = @(
    'viralshorts.bat',
    'EMPIEZA AQUI.txt',
    'gui.py',
    'pipeline.py',
    'requirements.txt',
    'config.yaml',
    'README.md'
)
$carpetas = @('viralshorts', 'web', 'tests')

foreach ($f in $ficheros) {
    $origen = Join-Path $Raiz $f
    if (Test-Path -LiteralPath $origen) {
        Copy-Item -LiteralPath $origen -Destination $tmp
    } else {
        Write-Host "  aviso: falta $f" -ForegroundColor Yellow
    }
}
foreach ($c in $carpetas) {
    $origen = Join-Path $Raiz $c
    if (Test-Path -LiteralPath $origen) {
        Copy-Item -LiteralPath $origen -Destination $tmp -Recurse
    }
}

# El instalador viaja entero menos el log y la marca del equipo de origen: si
# se colara la marca, en el ordenador del otro el programa creeria que ya esta
# instalado y arrancaria sin dependencias.
$destinoInstall = Join-Path $tmp 'install'
New-Item -ItemType Directory -Path $destinoInstall | Out-Null
Get-ChildItem -LiteralPath $PSScriptRoot -File |
    Where-Object { $_.Name -notin @('log-instalacion.txt', '.instalado') } |
    Copy-Item -Destination $destinoInstall

# Cache de Python: son binarios de la version concreta del equipo de origen.
Get-ChildItem -LiteralPath $tmp -Recurse -Directory -Filter '__pycache__' -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force

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
