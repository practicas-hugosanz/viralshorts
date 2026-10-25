<#
  Compila el motor de whisper.cpp para tarjetas AMD e Intel (Vulkan) y lo deja
  empaquetado en dist\whisper-vulkan-<version>-x64.zip, listo para subirlo a la
  release "motores" del repo publico. whisper.cpp no publica este motor para
  Windows; el de CPU y el de NVIDIA (CUDA) si, y la app los baja de alli.

      powershell -ExecutionPolicy Bypass -File install\compilar-whisper-vulkan.ps1

  No instala nada en el sistema: CMake y Ninja van con pip (usuario), y el
  compilador de shaders (glslc), el cargador de Vulkan y las cabeceras se sacan
  de paquetes de conda-forge y de los repos de Khronos a una carpeta temporal de
  ruta corta (MSVC no aguanta rutas de mas de 260 caracteres). Hace falta Visual
  Studio Build Tools 2022 con C++, Python 3 y git.

  Al terminar imprime la huella SHA-256: hay que copiarla en
  VULKAN_ENGINE_SHA256 (rust/src/setup.rs) y subir el ZIP:

      gh release upload motores dist\whisper-vulkan-<version>-x64.zip --repo practicas-hugosanz/viralshorts-releases --clobber
#>

[CmdletBinding()]
param(
    # La misma version que el motor de CPU (WHISPER_RELEASE en rust/src/setup.rs).
    [string]$Version = 'b5454',
    [string]$Trabajo = (Join-Path $env:TEMP 'vsvk')
)

$ErrorActionPreference = 'Stop'
$Raiz = Split-Path -Parent $PSScriptRoot
$VsDev = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $VsDev)) { throw "Falta Visual Studio Build Tools 2022 con C++ ($VsDev)." }

function Paso($texto) { Write-Host ''; Write-Host "== $texto" -ForegroundColor Cyan }
function Ejecutar($exe, [string[]]$argumentos) {
    & $exe @argumentos
    if ($LASTEXITCODE -ne 0) { throw "$exe fallo (codigo $LASTEXITCODE)." }
}

New-Item -ItemType Directory -Force $Trabajo | Out-Null
$Sdk = Join-Path $Trabajo 'sdk'

Paso 'CMake, Ninja y zstandard (pip, para el usuario)'
Ejecutar 'python' @('-m', 'pip', 'install', '--user', '--quiet', 'cmake', 'ninja', 'zstandard')
$Scripts = (& python -c "import sysconfig; print(sysconfig.get_path('scripts', scheme='nt_user'))").Trim()

Paso 'glslc y el cargador de Vulkan (conda-forge)'
$descomprimir = @'
import io, json, sys, tarfile, urllib.request, zipfile, zstandard
name, version, dest = sys.argv[1], sys.argv[2], sys.argv[3]
files = json.load(urllib.request.urlopen(f"https://api.anaconda.org/package/conda-forge/{name}/files"))
files = [f for f in files if f["attrs"].get("subdir") == "win-64" and f["version"].startswith(version) and f["basename"].endswith(".conda")]
files.sort(key=lambda f: f["upload_time"])
data = urllib.request.urlopen("https:" + files[-1]["download_url"]).read()
with zipfile.ZipFile(io.BytesIO(data)) as z:
    for inner in z.namelist():
        if inner.startswith("pkg-") and inner.endswith(".tar.zst"):
            stream = zstandard.ZstdDecompressor().stream_reader(io.BytesIO(z.read(inner)))
            with tarfile.open(fileobj=stream, mode="r|") as t:
                t.extractall(dest, filter="data")
print(name, files[-1]["version"])
'@
$py = Join-Path $Trabajo 'conda.py'
Set-Content -Path $py -Value $descomprimir -Encoding UTF8
foreach ($paquete in @(@('shaderc', '2026'), @('glslang', '16'), @('spirv-tools', '2026'), @('libvulkan-loader', '1.4'))) {
    Ejecutar 'python' @('-E', $py, $paquete[0], $paquete[1], $Sdk)
}

Paso "Fuentes: whisper.cpp $Version, Vulkan-Headers, SPIRV-Headers"
Push-Location $Trabajo
try {
    if (-not (Test-Path 'whisper')) { Ejecutar 'git' @('clone', '-q', '--depth', '1', '--branch', $Version, 'https://github.com/ggml-org/whisper.cpp', 'whisper') }
    if (-not (Test-Path 'headers')) { Ejecutar 'git' @('clone', '-q', '--depth', '1', 'https://github.com/KhronosGroup/Vulkan-Headers', 'headers') }
    if (-not (Test-Path 'spirv-src')) { Ejecutar 'git' @('clone', '-q', '--depth', '1', 'https://github.com/KhronosGroup/SPIRV-Headers', 'spirv-src') }
} finally { Pop-Location }
# ggml-vulkan busca spirv/unified1/spirv.hpp junto a las cabeceras de Vulkan.
Copy-Item -Recurse -Force (Join-Path $Trabajo 'spirv-src\include\spirv') (Join-Path $Trabajo 'headers\include')

Paso 'Compilando (unos minutos)'
$bat = Join-Path $Trabajo 'compilar.bat'
@"
@echo off
call "$VsDev" >nul || exit /b 1
set PATH=$Scripts;$Sdk\Library\bin;%PATH%
cmake -S "$Trabajo\whisper" -B "$Trabajo\build" -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON -DGGML_VULKAN=ON -DGGML_BACKEND_DL=ON -DGGML_CPU_ALL_VARIANTS=ON -DGGML_NATIVE=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_SERVER=OFF -DWHISPER_SDL2=OFF -DVulkan_INCLUDE_DIR="$Trabajo\headers\include" -DVulkan_LIBRARY="$Sdk\Library\lib\vulkan-1.lib" -DVulkan_GLSLC_EXECUTABLE="$Sdk\Library\bin\glslc.exe" || exit /b 1
cmake --build "$Trabajo\build" --config Release --target whisper-cli || exit /b 1
"@ | Set-Content -Path $bat -Encoding ASCII
Ejecutar 'cmd.exe' @('/c', $bat)

Paso 'Empaquetando'
$bin = Join-Path $Trabajo 'build\bin'
$redist = Get-ChildItem 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Redist\MSVC' -Directory | Where-Object { $_.Name -match '^\d' } | Sort-Object Name | Select-Object -Last 1
$crt = @('Microsoft.VC143.CRT\msvcp140.dll', 'Microsoft.VC143.CRT\vcruntime140.dll', 'Microsoft.VC143.CRT\vcruntime140_1.dll', 'Microsoft.VC143.OpenMP\vcomp140.dll') |
    ForEach-Object { Join-Path $redist.FullName "x64\$_" }
$ficheros = @(Get-ChildItem $bin -Filter '*.dll') + @(Get-Item (Join-Path $bin 'whisper-cli.exe')) + @($crt | ForEach-Object { Get-Item $_ }) + @(Get-Item (Join-Path $Trabajo 'whisper\LICENSE'))
$zip = Join-Path $Raiz "dist\whisper-vulkan-$Version-x64.zip"
New-Item -ItemType Directory -Force (Split-Path $zip) | Out-Null
if (Test-Path $zip) { Remove-Item $zip }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archivo = [System.IO.Compression.ZipFile]::Open($zip, 'Create')
try {
    foreach ($f in $ficheros) {
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archivo, $f.FullName, $f.Name, 'Optimal') | Out-Null
    }
} finally { $archivo.Dispose() }

$huella = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
$mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host ''
Write-Host "Motor listo: $zip ($mb MB)" -ForegroundColor Green
Write-Host "SHA-256: $huella"
Write-Host 'Copiala en VULKAN_ENGINE_SHA256 (rust/src/setup.rs) y sube el ZIP a la release "motores".'
