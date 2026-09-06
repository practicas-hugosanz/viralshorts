<#
  Instalador de viralshorts.

  Deja la carpeta lista para usarse sin que nadie tenga que instalar nada a
  mano: busca (o instala) Python, crea un entorno virtual propio, mete las
  dependencias y descarga una copia portable de ffmpeg dentro de esta misma
  carpeta.

  Todo queda contenido aqui dentro. No toca el Python del sistema, no cambia
  el PATH del usuario y no necesita permisos de administrador.

  Uso normal: no se llama a mano, lo lanza "viralshorts.bat".
      powershell -ExecutionPolicy Bypass -File install\setup.ps1
      powershell -ExecutionPolicy Bypass -File install\setup.ps1 -Force
#>

[CmdletBinding()]
param(
    # Rehace el entorno virtual y reinstala las dependencias desde cero.
    [switch]$Force,
    # No crear el acceso directo en el escritorio.
    [switch]$SinAccesoDirecto
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # las barras de Invoke-WebRequest son lentisimas

# La raiz del proyecto es la carpeta que contiene a "install".
$Raiz = Split-Path -Parent $PSScriptRoot
Set-Location $Raiz

$VenvPy = Join-Path $Raiz '.venv\Scripts\python.exe'
$Reqs   = Join-Path $Raiz 'requirements.txt'
$Sello  = Join-Path $Raiz '.venv\.requisitos.sha256'
$Marca  = Join-Path $PSScriptRoot '.instalado'
$FFDir  = Join-Path $Raiz 'bin\ffmpeg'
$FFExe  = Join-Path $FFDir 'bin\ffmpeg.exe'
$Log    = Join-Path $PSScriptRoot 'log-instalacion.txt'

# Version de Python que se instala si no hay ninguna. 3.12 es la ultima con
# ruedas precompiladas de todo lo que usamos (ctranslate2, opencv, numpy).
$PyVersion = '3.12.10'
$PyUrl     = "https://www.python.org/ftp/python/$PyVersion/python-$PyVersion-amd64.exe"

# Version de ffmpeg que se descarga. Fijada a proposito: ver los espejos.
$FFVersion = '8.0'

# Rango soportado, como major*100+minor. Por debajo faltan cosas del lenguaje;
# por encima aun no hay ruedas de alguna dependencia y pip intentaria
# compilarlas desde el codigo fuente (y fallaria sin Visual Studio).
$MinPy = 310
$MaxPy = 313

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
                Invoke-WebRequest -Uri $url -OutFile $destino -UseBasicParsing -TimeoutSec 900
                if ((Get-Item -LiteralPath $destino).Length -gt 0) { return $true }
            } catch {
                Add-Content -LiteralPath $Log -Value "      fallo ($url): $($_.Exception.Message)" -Encoding utf8
                Start-Sleep -Seconds 2
            }
        }
    }
    return $false
}

# Devuelve la version de un interprete como numero (3.12 -> 312), o 0 si ese
# comando no existe de verdad. Hace falta porque el lanzador "py -3.12" puede
# responder aunque esa version concreta no este instalada.
#
# El fragmento de Python va sin comillas ni % a proposito: Windows PowerShell
# 5.1 rompe el entrecomillado al pasar argumentos a un .exe y el codigo llega
# destrozado. Y la preferencia de errores se baja aqui dentro porque, con
# 'Stop', el stderr de un candidato que no existe se convierte en excepcion.
function VersionDe([string]$exe, [string[]]$parametros = @()) {
    $ErrorActionPreference = 'SilentlyContinue'
    try {
        $salida = & $exe @parametros -c 'import sys;print(sys.version_info[0]*100+sys.version_info[1])' 2>$null
        if ($LASTEXITCODE -ne 0 -or -not $salida) { return 0 }
        return [int](([string]$salida).Trim())
    } catch { return 0 }
}

function EsSoportada([int]$v) {
    return ($v -ge $MinPy -and $v -le $MaxPy)
}

# "312" -> "3.12", para los mensajes.
function Legible([int]$v) {
    if ($v -le 0) { return 'desconocida' }
    return "$([math]::Floor($v / 100)).$($v % 100)"
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

if (-not (Test-Path -LiteralPath $Reqs)) {
    Muere "No encuentro requirements.txt en $Raiz." @(
        "Parece que la carpeta esta incompleta.",
        "Vuelve a descomprimir el ZIP entero, sin dejarte ficheros."
    )
}

if ($Force -and (Test-Path -LiteralPath (Join-Path $Raiz '.venv'))) {
    Paso "[0/4] Borrando la instalacion anterior..."
    Remove-Item -LiteralPath (Join-Path $Raiz '.venv') -Recurse -Force
    Remove-Item -LiteralPath $Marca -Force -ErrorAction SilentlyContinue
    Bien "entorno anterior eliminado"
}

# ---------------------------------------------------------------------------
# 1. Python
# ---------------------------------------------------------------------------
Paso "[1/4] Buscando Python..."

$candidatos = @(
    @{ exe = 'py';      p = @('-3.12') },
    @{ exe = 'py';      p = @('-3.11') },
    @{ exe = 'py';      p = @('-3.13') },
    @{ exe = 'py';      p = @('-3.10') },
    @{ exe = 'python';  p = @() },
    @{ exe = 'python3'; p = @() },
    @{ exe = 'py';      p = @('-3') }
)
foreach ($v in @('312', '311', '313', '310')) {
    $candidatos += @{ exe = "$env:LOCALAPPDATA\Programs\Python\Python$v\python.exe"; p = @() }
    $candidatos += @{ exe = "C:\Program Files\Python$v\python.exe";                  p = @() }
}

function BuscaPython {
    foreach ($c in $candidatos) {
        $ver = VersionDe $c.exe $c.p
        if (EsSoportada $ver) {
            return @{ exe = $c.exe; p = $c.p; ver = $ver }
        }
    }
    return $null
}

$py = BuscaPython

if (-not $py) {
    Aviso "no hay ningun Python compatible instalado"
    Escribe "      Instalando Python $PyVersion (solo para tu usuario)..."

    $tocado = $false

    # Via 1: winget. Es la limpia cuando existe (Windows 10 21H1+ / Windows 11).
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Escribe "      probando con winget..." 'DarkGray'
        try {
            winget install --id Python.Python.3.12 -e --source winget --scope user `
                   --accept-package-agreements --accept-source-agreements |
                Out-File -FilePath $Log -Append -Encoding utf8
        } catch {
            Add-Content -LiteralPath $Log -Value "      winget fallo: $($_.Exception.Message)" -Encoding utf8
        }
        $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
                    [Environment]::GetEnvironmentVariable('Path', 'User')
        $tocado = $true
    }

    # Via 2: instalador oficial de python.org, en silencio. Funciona siempre,
    # tambien en equipos sin winget.
    if (-not (BuscaPython)) {
        Escribe "      descargando el instalador oficial de python.org..." 'DarkGray'
        $exe = Join-Path $env:TEMP "python-$PyVersion-amd64.exe"
        if (-not (Descarga @($PyUrl) $exe)) {
            Muere "No se pudo descargar Python." @(
                "Comprueba la conexion a internet y vuelve a intentarlo.",
                "O instalalo a mano desde https://www.python.org/downloads/",
                "marcando la casilla 'Add python.exe to PATH'."
            )
        }
        # Sin admin, para el usuario actual, con el lanzador "py" y sin ninguna
        # ventana que haya que atender.
        $argumentos = @(
            '/quiet', 'InstallAllUsers=0', 'PrependPath=1', 'Include_launcher=1',
            'Include_test=0', 'Include_doc=0'
        )
        $proc = Start-Process -FilePath $exe -ArgumentList $argumentos -Wait -PassThru
        Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue
        Add-Content -LiteralPath $Log -Value "      instalador de python salio con codigo $($proc.ExitCode)" -Encoding utf8
        $tocado = $true
    }

    if ($tocado) {
        # El PATH de esta ventana es el de antes de instalar: hay que releerlo.
        $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
                    [Environment]::GetEnvironmentVariable('Path', 'User')
    }

    $py = BuscaPython
    if (-not $py) {
        Muere "Python se ha instalado pero no lo encuentro." @(
            "Cierra esta ventana, vuelve a abrir 'viralshorts.bat' y deberia funcionar.",
            "Si sigue igual, reinicia el ordenador y prueba otra vez."
        )
    }
}

Bien "Python $(Legible $py.ver)  ($($py.exe) $($py.p -join ' '))"

# ---------------------------------------------------------------------------
# 2. Entorno virtual
# ---------------------------------------------------------------------------
Paso "[2/4] Preparando el entorno del programa..."

# Un .venv de una version que ya no vale (Python actualizado, carpeta copiada
# de otro equipo) rompe todo mas adelante con errores incomprensibles. Mejor
# detectarlo aqui y rehacerlo.
if (Test-Path -LiteralPath $VenvPy) {
    if (-not (EsSoportada (VersionDe $VenvPy))) {
        Aviso "el entorno existente no es utilizable, se rehace"
        Remove-Item -LiteralPath (Join-Path $Raiz '.venv') -Recurse -Force
        Remove-Item -LiteralPath $Sello -Force -ErrorAction SilentlyContinue
    }
}

if (-not (Test-Path -LiteralPath $VenvPy)) {
    & $py.exe @($py.p) '-m' 'venv' '.venv' 2>&1 |
        Out-File -FilePath $Log -Append -Encoding utf8
    if (-not (Test-Path -LiteralPath $VenvPy)) {
        Muere "No se pudo crear el entorno virtual (.venv)." @(
            "Suele ser el antivirus bloqueando la creacion de ficheros.",
            "Prueba a mover la carpeta al Escritorio y ejecutar otra vez."
        )
    }
    Bien "entorno creado"
} else {
    Bien "entorno ya existente"
}

# ---------------------------------------------------------------------------
# 3. Dependencias
# ---------------------------------------------------------------------------
Paso "[3/4] Instalando dependencias..."

# Si requirements.txt no ha cambiado desde la ultima instalacion correcta no
# hay nada que hacer: asi la segunda ejecucion es instantanea.
$hashActual = (Get-FileHash -LiteralPath $Reqs -Algorithm SHA256).Hash
$hashPrevio = if (Test-Path -LiteralPath $Sello) { (Get-Content -LiteralPath $Sello -Raw).Trim() } else { '' }

if ($hashActual -eq $hashPrevio -and -not $Force) {
    Bien "ya estaban instaladas y al dia"
} else {
    Escribe "      la primera vez tarda varios minutos, es normal" 'DarkGray'
    & $VenvPy -m pip install --upgrade pip --quiet --disable-pip-version-check 2>&1 |
        Out-File -FilePath $Log -Append -Encoding utf8
    & $VenvPy -m pip install -r $Reqs --disable-pip-version-check 2>&1 |
        Tee-Object -FilePath $Log -Append
    if ($LASTEXITCODE -ne 0) {
        Muere "Fallo la instalacion de las dependencias." @(
            "Lo mas comun es quedarse sin internet a medias: vuelve a ejecutarlo.",
            "Si se repite, ejecuta 'install\Reparar instalacion.bat'."
        )
    }
    Set-Content -LiteralPath $Sello -Value $hashActual -Encoding ascii
    Bien "dependencias instaladas"
}

# ---------------------------------------------------------------------------
# 4. ffmpeg
# ---------------------------------------------------------------------------
Paso "[4/4] Comprobando ffmpeg..."

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
    # Version ESTABLE y fijada, servida desde GitHub. Las tres cosas importan:
    #  - estable: las compilaciones de la rama de desarrollo van quitando
    #    opciones (asi desaparecio -filter_complex_script) y rompen el montaje;
    #  - fijada: que la descarga de hoy y la de dentro de un ano sean iguales;
    #  - GitHub: su CDN va a decenas de MB/s, mientras que el servidor propio
    #    de gyan.dev puede bajar a menos de 100 KB/s y parecer que esto se ha
    #    colgado. Ese queda de respaldo, que sirve la misma compilacion.
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
    New-Item -ItemType Directory -Force -Path (Join-Path $Raiz 'bin') | Out-Null
    if (Test-Path -LiteralPath $FFDir) { Remove-Item -LiteralPath $FFDir -Recurse -Force }
    # Copiar y borrar, no mover: TEMP suele estar en C: y el proyecto puede
    # estar en otra unidad, y Move-Item no cruza volumenes con carpetas.
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
# Comprobacion final: que el programa arranca de verdad. Es preferible fallar
# aqui, con la ventana del instalador delante, que al abrir la interfaz.
# ---------------------------------------------------------------------------
Paso "Comprobando que todo funciona..."
if (Test-Path -LiteralPath $FFExe) {
    $env:Path = (Join-Path $FFDir 'bin') + ';' + $env:Path
}
# En un fichero y no con -c: PowerShell 5.1 estropea las comillas al pasar
# argumentos a un .exe, y este fragmento esta lleno de ellas.
$verifica = @'
import shutil, sys

faltan = []
for modulo in ("yt_dlp", "faster_whisper", "numpy", "yaml"):
    try:
        __import__(modulo)
    except Exception as error:
        faltan.append("%s: %s" % (modulo, error))
for binario in ("ffmpeg", "ffprobe"):
    if shutil.which(binario) is None:
        faltan.append(binario + " no esta accesible")

print("\n".join(faltan))
sys.exit(1 if faltan else 0)
'@
$scriptVerifica = Join-Path $env:TEMP 'viralshorts-verifica.py'
Set-Content -LiteralPath $scriptVerifica -Value $verifica -Encoding ascii
$fallos = & $VenvPy $scriptVerifica 2>&1
$codigo = $LASTEXITCODE
Remove-Item -LiteralPath $scriptVerifica -Force -ErrorAction SilentlyContinue
if ($codigo -ne 0) {
    Escribe ($fallos | Out-String) 'DarkGray'
    Muere "La instalacion quedo incompleta." @(
        "Ejecuta 'install\Reparar instalacion.bat' para rehacerla desde cero."
    )
}
Bien "todo correcto"

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
