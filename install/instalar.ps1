<#
  Instalador de viralshorts.

  Deja la carpeta lista para usarse sin que nadie tenga que instalar nada a
  mano: descarga, dentro de esta misma carpeta, las tres herramientas que el
  programa usa como ejecutables:

      ffmpeg      - decodifica y monta los videos
      yt-dlp      - descarga de YouTube
      whisper.cpp - transcribe la voz (motor de CPU; el de GPU es opcional)

  Los modelos (voz y caras) los descarga el propio programa la primera vez que
  los necesita.

  Todo queda contenido aqui dentro. No toca el PATH del usuario y no necesita
  permisos de administrador. No hay Python ni nada que instalar en el sistema.

  Uso normal: no se llama a mano, lo lanza "viralshorts.bat".
      powershell -ExecutionPolicy Bypass -File install\instalar.ps1
      powershell -ExecutionPolicy Bypass -File install\instalar.ps1 -Force
      powershell -ExecutionPolicy Bypass -File install\instalar.ps1 -Gpu
#>

[CmdletBinding()]
param(
    # Vuelve a descargar todo, aunque ya este.
    [switch]$Force,
    # Instala solo el motor CUDA de whisper.cpp (para tarjetas NVIDIA).
    [switch]$Gpu,
    # No crear el acceso directo en el escritorio.
    [switch]$SinAccesoDirecto
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # las barras de Invoke-WebRequest son lentisimas

# La raiz del proyecto es la carpeta que contiene a "install".
$Raiz = Split-Path -Parent $PSScriptRoot
Set-Location $Raiz

$Bin     = Join-Path $Raiz 'bin'
$FFDir   = Join-Path $Bin 'ffmpeg'
$FFExe   = Join-Path $FFDir 'bin\ffmpeg.exe'
$YtExe   = Join-Path $Bin 'yt-dlp.exe'
$WhDir   = Join-Path $Bin 'whisper'
$WhExe   = Join-Path $WhDir 'whisper-cli.exe'
$CudaDir = Join-Path $Bin 'whisper-cuda'
$CudaExe = Join-Path $CudaDir 'whisper-cli.exe'
$Marca   = Join-Path $PSScriptRoot '.instalado'
$Log     = Join-Path $PSScriptRoot 'log-instalacion.txt'

# Versiones FIJADAS a proposito: que la descarga de hoy y la de dentro de un ano
# sean la misma, y que una version nueva con opciones quitadas no rompa el montaje.
$FFVersion      = '8.0'
$WhisperRelease = 'b5454'
$WhisperBase    = "https://github.com/ggml-org/whisper.cpp/releases/download/$WhisperRelease"
$YtdlpUrl       = 'https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe'

# ---------------------------------------------------------------------------
# Utilidades de salida. Sin acentos a proposito: la consola de Windows suele
# ir en codepage 850/1252 y los acentos salen como simbolos raros.
# ---------------------------------------------------------------------------
function Escribe([string]$texto, [string]$color = 'Gray') {
    Write-Host $texto -ForegroundColor $color
    Add-Content -LiteralPath $Log -Value $texto -Encoding utf8
}
function Paso([string]$texto)  { Escribe "" ; Escribe $texto 'Cyan' }
function Bien([string]$texto)  { Escribe "  OK  $texto" 'Green' }
function Aviso([string]$texto) { Escribe "  --  $texto" 'Yellow' }

function Muere([string]$texto, [string[]]$pistas = @()) {
    Escribe ""
    Escribe "  ERROR: $texto" 'Red'
    foreach ($p in $pistas) { Escribe "         $p" 'Yellow' }
    Escribe ""
    Escribe "  Detalle completo en: $Log" 'DarkGray'
    exit 1
}

# Descarga con reintentos y espejos alternativos. La red del usuario no es
# problema nuestro, pero un espejo caido no deberia romper la instalacion.
function Descarga([string[]]$urls, [string]$destino) {
    foreach ($url in $urls) {
        for ($intento = 1; $intento -le 2; $intento++) {
            try {
                Escribe "      descargando $([System.IO.Path]::GetFileName($url)) ..." 'DarkGray'
                Invoke-WebRequest -Uri $url -OutFile $destino -UseBasicParsing -TimeoutSec 1800
                if ((Get-Item -LiteralPath $destino).Length -gt 0) { return $true }
            } catch {
                Add-Content -LiteralPath $Log -Value "      fallo ($url): $($_.Exception.Message)" -Encoding utf8
                Start-Sleep -Seconds 2
            }
        }
    }
    return $false
}

# Descomprime un ZIP en una carpeta, quedandose solo con los ficheros que
# interesan (por nombre, sin importar en que subcarpeta del ZIP esten).
function ExtraeSolo([string]$zip, [string]$destino, [scriptblock]$quiero) {
    $tmp = Join-Path $env:TEMP ('viralshorts-' + [guid]::NewGuid().ToString('N'))
    Expand-Archive -LiteralPath $zip -DestinationPath $tmp -Force
    if (Test-Path -LiteralPath $destino) { Remove-Item -LiteralPath $destino -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $destino | Out-Null
    Get-ChildItem -LiteralPath $tmp -Recurse -File |
        Where-Object { & $quiero $_ } |
        ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $destino }
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# ---------------------------------------------------------------------------
# Arranque
# ---------------------------------------------------------------------------
"=== instalacion $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') ===" |
    Set-Content -LiteralPath $Log -Encoding utf8

Escribe ""
Escribe "==========================================================" 'White'
Escribe "  viralshorts - instalacion automatica" 'White'
Escribe "  No tienes que instalar nada a mano. Espera y ya esta." 'White'
Escribe "==========================================================" 'White'

New-Item -ItemType Directory -Force -Path $Bin | Out-Null

# ---------------------------------------------------------------------------
# Solo GPU: el motor CUDA de whisper.cpp (~700 MB con las librerias de NVIDIA).
# ---------------------------------------------------------------------------
if ($Gpu) {
    Paso "Instalando el motor CUDA de whisper.cpp..."
    if ((Test-Path -LiteralPath $CudaExe) -and -not $Force) {
        Bien "ya estaba instalado"
        exit 0
    }
    Escribe "      son unos 700 MB; puede tardar un rato, no cierres la ventana" 'DarkGray'
    $zip = Join-Path $env:TEMP 'viralshorts-whisper-cuda.zip'
    if (-not (Descarga @("$WhisperBase/whisper-bin-win-cuda-12.4.0-x64.zip") $zip)) {
        Muere "No se pudo descargar el motor CUDA." @(
            "Comprueba la conexion y vuelve a intentarlo.",
            "No pasa nada: el programa sigue funcionando con el procesador."
        )
    }
    # Aqui hacen falta TODOS los ficheros: las librerias de CUDA van con el motor.
    ExtraeSolo $zip $CudaDir { param($f) $true }
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path -LiteralPath $CudaExe)) {
        Muere "El motor CUDA se descargo pero no esta donde deberia."
    }
    Bien "motor CUDA listo: la transcripcion usara la tarjeta NVIDIA"
    exit 0
}

if ($Force) {
    Paso "[0/3] Borrando la instalacion anterior..."
    foreach ($d in @($FFDir, $WhDir)) {
        if (Test-Path -LiteralPath $d) { Remove-Item -LiteralPath $d -Recurse -Force }
    }
    Remove-Item -LiteralPath $YtExe, $Marca -Force -ErrorAction SilentlyContinue
    Bien "herramientas anteriores eliminadas (los modelos y tus videos se conservan)"
}

# ---------------------------------------------------------------------------
# 1. ffmpeg
# ---------------------------------------------------------------------------
Paso "[1/3] Comprobando ffmpeg..."

$ffSistema = (Get-Command ffmpeg -ErrorAction SilentlyContinue) -and
             (Get-Command ffprobe -ErrorAction SilentlyContinue)

if (Test-Path -LiteralPath $FFExe) {
    Bien "copia portable ya descargada"
} elseif ($ffSistema) {
    Bien "ffmpeg del sistema"
} else {
    Escribe "      descargando ffmpeg portable (~90 MB, solo esta vez)..."
    Escribe "      puede tardar unos minutos; no cierres la ventana" 'DarkGray'
    $zip = Join-Path $env:TEMP 'viralshorts-ffmpeg.zip'
    $tmp = Join-Path $env:TEMP 'viralshorts-ffmpeg-tmp'
    # Version ESTABLE y fijada, servida desde GitHub: las compilaciones de la rama
    # de desarrollo van quitando opciones (asi desaparecio -filter_complex_script)
    # y rompen el montaje; y el CDN de GitHub va a decenas de MB/s, mientras que
    # el servidor propio de gyan.dev puede bajar a menos de 100 KB/s. Ese queda
    # de respaldo, que sirve la misma compilacion.
    $espejos = @(
        "https://github.com/GyanD/codexffmpeg/releases/download/$FFVersion/ffmpeg-$FFVersion-essentials_build.zip",
        'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip'
    )
    if (-not (Descarga $espejos $zip)) {
        Muere "No se pudo descargar ffmpeg." @(
            "Comprueba la conexion y vuelve a ejecutar 'viralshorts.bat'.",
            "Alternativa manual: abre una terminal y ejecuta  winget install Gyan.FFmpeg"
        )
    }
    if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force }
    Expand-Archive -LiteralPath $zip -DestinationPath $tmp -Force
    # Los dos espejos empaquetan igual: una unica carpeta raiz con bin\ dentro.
    $src = Get-ChildItem -LiteralPath $tmp -Directory | Select-Object -First 1
    if (-not $src) { Muere "El ZIP de ffmpeg llego corrupto. Vuelve a ejecutar el instalador." }
    if (Test-Path -LiteralPath $FFDir) { Remove-Item -LiteralPath $FFDir -Recurse -Force }
    # Copiar y borrar, no mover: TEMP suele estar en C: y el proyecto puede estar
    # en otra unidad, y Move-Item no cruza volumenes con carpetas.
    Copy-Item -LiteralPath $src.FullName -Destination $FFDir -Recurse
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path -LiteralPath $FFExe)) {
        Muere "ffmpeg se descargo pero no esta donde deberia." @(
            "Ejecuta 'install\Reparar instalacion.bat'."
        )
    }
    Bien "ffmpeg listo"
}

# ---------------------------------------------------------------------------
# 2. yt-dlp
# ---------------------------------------------------------------------------
Paso "[2/3] Comprobando yt-dlp..."

if (Test-Path -LiteralPath $YtExe) {
    Bien "ya descargado"
} else {
    # La ultima version: YouTube cambia y yt-dlp se actualiza para seguirlo; una
    # version vieja deja de poder bajar videos.
    if (-not (Descarga @($YtdlpUrl) $YtExe)) {
        Muere "No se pudo descargar yt-dlp." @(
            "Comprueba la conexion y vuelve a ejecutar 'viralshorts.bat'.",
            "Alternativa manual: baja yt-dlp.exe de https://github.com/yt-dlp/yt-dlp/releases",
            "y dejalo en la carpeta 'bin'."
        )
    }
    Bien "yt-dlp listo"
}

# ---------------------------------------------------------------------------
# 3. whisper.cpp
# ---------------------------------------------------------------------------
Paso "[3/3] Comprobando whisper.cpp (la transcripcion)..."

if (Test-Path -LiteralPath $WhExe) {
    Bien "ya descargado"
} else {
    $zip = Join-Path $env:TEMP 'viralshorts-whisper.zip'
    if (-not (Descarga @("$WhisperBase/whisper-bin-x64.zip") $zip)) {
        Muere "No se pudo descargar whisper.cpp." @(
            "Comprueba la conexion y vuelve a ejecutar 'viralshorts.bat'."
        )
    }
    # Solo lo necesario: el ZIP trae decenas de ejecutables de ejemplo.
    ExtraeSolo $zip $WhDir { param($f) $f.Name -eq 'whisper-cli.exe' -or $f.Name -eq 'whisper.dll' -or ($f.Name -like 'ggml*.dll') }
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path -LiteralPath $WhExe)) {
        Muere "whisper.cpp se descargo pero no esta donde deberia." @(
            "Ejecuta 'install\Reparar instalacion.bat'."
        )
    }
    Bien "whisper.cpp listo"
}

# ---------------------------------------------------------------------------
# Comprobacion final: que el programa arranca de verdad y ve todas las
# herramientas. Es preferible fallar aqui, con la ventana del instalador
# delante, que al abrir la interfaz.
# ---------------------------------------------------------------------------
Paso "Comprobando que todo funciona..."
$programa = Join-Path $Raiz 'viralshorts.exe'
if (-not (Test-Path -LiteralPath $programa)) { $programa = Join-Path $Raiz 'rust\target\release\viralshorts.exe' }
if (Test-Path -LiteralPath $programa) {
    # El programa escribe sus avisos por stderr; con 'Stop' PowerShell 5.1 los
    # convierte en un error fatal aunque el programa haya salido bien.
    $previo = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $diagnostico = & $programa doctor 2>&1 | ForEach-Object { "$_" } | Out-String
    $codigo = $LASTEXITCODE
    $ErrorActionPreference = $previo
    Add-Content -LiteralPath $Log -Value $diagnostico -Encoding utf8
    if ($codigo -ne 0) {
        Escribe $diagnostico 'DarkGray'
        Muere "La instalacion quedo incompleta." @(
            "Ejecuta 'install\Reparar instalacion.bat' para rehacerla desde cero."
        )
    }
    Bien "todo correcto"
} else {
    Aviso "no encuentro viralshorts.exe para comprobarlo (no pasa nada)"
}

# Acceso directo en el escritorio. Es lo que convierte esto en "un icono y ya".
if (-not $SinAccesoDirecto) {
    try {
        $escritorio = [Environment]::GetFolderPath('Desktop')
        $ws = New-Object -ComObject WScript.Shell
        $acceso = $ws.CreateShortcut((Join-Path $escritorio 'viralshorts.lnk'))
        $acceso.TargetPath       = Join-Path $Raiz 'viralshorts.bat'
        $acceso.WorkingDirectory = $Raiz
        $acceso.Description      = 'Abrir viralshorts'
        $acceso.IconLocation     = "$env:SystemRoot\System32\shell32.dll,137"
        $acceso.Save()
        Bien "acceso directo creado en el Escritorio"
    } catch {
        Aviso "no se pudo crear el acceso directo (no pasa nada)"
    }
}

Set-Content -LiteralPath $Marca -Value (Get-Date -Format 'o') -Encoding ascii

Escribe ""
Escribe "==========================================================" 'Green'
Escribe "  Listo. Abriendo la interfaz..." 'Green'
Escribe "  La proxima vez: doble clic en 'viralshorts' (Escritorio)" 'Green'
Escribe "  o en 'viralshorts.bat' dentro de esta carpeta." 'Green'
Escribe "==========================================================" 'Green'
Escribe ""
exit 0
