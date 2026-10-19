<#
  Prueba la interfaz de verdad, como una persona: abre la pagina en un Edge sin
  ventana, toca los controles y comprueba lo que se ve y lo que se enviaria al
  servidor (web\tests\ui-smoke.mjs).

      powershell -ExecutionPolicy Bypass -File install\probar-interfaz.ps1

  Prepara una carpeta de datos de usar y tirar (con unos shorts de ejemplo para la
  biblioteca), arranca el servidor de la interfaz con el ejecutable compilado, lanza
  la prueba y lo apaga todo. No toca tus datos ni tus shorts. Hace falta Node 22 o
  superior, Microsoft Edge y haber compilado (cargo build --release).
#>

[CmdletBinding()]
param(
    [string]$Edge = 'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
    [int]$Puerto = 8798
)

$ErrorActionPreference = 'Stop'
$Raiz = Split-Path -Parent $PSScriptRoot
$Exe = Join-Path $Raiz 'rust\target\release\viralshorts.exe'
if (-not (Test-Path $Exe)) { throw "No esta ${Exe}: compila antes con cargo build --release." }
if (-not (Test-Path $Edge)) { throw "No se encuentra Edge en $Edge (se puede pasar con -Edge)." }
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { throw 'Hace falta Node.js 22 o superior.' }

$Casa = Join-Path $env:TEMP ('vs-ui-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
# Herramientas de mentira: solo hace falta que existan para saltarse la pantalla de instalacion.
foreach ($f in 'bin\ffmpeg\bin\ffmpeg.exe', 'bin\ffmpeg\bin\ffprobe.exe', 'bin\yt-dlp.exe', 'bin\whisper\whisper-cli.exe') {
    $ruta = Join-Path $Casa $f
    New-Item -ItemType Directory -Force (Split-Path $ruta) | Out-Null
    New-Item -ItemType File -Force $ruta | Out-Null
}
# Dos shorts de ejemplo para la biblioteca.
$salida = Join-Path $Casa 'output\ejemplo_20260101_100000'
New-Item -ItemType Directory -Force $salida | Out-Null
foreach ($n in 1, 2) {
    Set-Content -Encoding UTF8 (Join-Path $salida "short_$n.mp4") 'no es un video de verdad'
    Set-Content -Encoding UTF8 (Join-Path $salida "short_$n.json") ('{"publicacion":{"title":"Short de ejemplo ' + $n + '","hook":"GANCHO","hashtags":["a"],"description":"texto"},"fuente":{"marca":"10:00"}}')
}

$env:VIRALSHORTS_HOME = $Casa
$servidor = Start-Process -FilePath $Exe -ArgumentList 'gui', '--no-browser', '--port', $Puerto -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $Casa 'servidor.log') -RedirectStandardOutput (Join-Path $Casa 'servidor.out')
try {
    $url = $null
    for ($i = 0; $i -lt 40 -and -not $url; $i++) {
        Start-Sleep -Milliseconds 250
        $texto = Get-Content (Join-Path $Casa 'servidor.out') -Raw -ErrorAction SilentlyContinue
        if ($texto -match 'http://127\.0\.0\.1:\d+/\?t=[\w-]+') { $url = $Matches[0] }
    }
    if (-not $url) { throw 'El servidor no arranco.' }
    & node (Join-Path $Raiz 'web\tests\ui-smoke.mjs') $url $Edge
    $codigo = $LASTEXITCODE
} finally {
    Stop-Process -Id $servidor.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    Remove-Item -Recurse -Force $Casa -ErrorAction SilentlyContinue
}
exit $codigo
