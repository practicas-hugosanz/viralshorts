@echo off
setlocal
cd /d "%~dp0"
title Instalador de viralshorts
echo(
echo ==========================================================
echo   viralshorts - instalacion automatica
echo   No tienes que instalar nada a mano. Espera y ya esta.
echo ==========================================================
echo(

REM ------------------------------------------------------------------
REM 1. Buscar un Python valido. Se prefiere 3.12/3.11: son los que
REM    tienen ruedas precompiladas de todas las dependencias.
REM ------------------------------------------------------------------
set "PY="
call :probar "py -3.12"
call :probar "py -3.11"
call :probar "py -3.10"
call :probar "py -3"
call :probar "python"
call :probar "python3"
if defined PY goto :tienepython

echo [1/3] No hay Python. Instalandolo con winget...
winget install --id Python.Python.3.12 -e --source winget --accept-package-agreements --accept-source-agreements
if errorlevel 1 goto :sinpython
set PY="%LOCALAPPDATA%\Programs\Python\Python312\python.exe"
%PY% --version >nul 2>&1
if errorlevel 1 goto :sinpython
goto :tienepython

:sinpython
echo(
echo  No se pudo instalar Python automaticamente.
echo  Instalalo desde https://www.python.org/downloads/  (marca "Add python.exe to PATH")
echo  y vuelve a hacer doble clic en este fichero.
echo(
pause
exit /b 1

:tienepython
echo [1/3] Python encontrado:
%PY% --version

REM ------------------------------------------------------------------
REM 2. Entorno virtual propio + dependencias. No toca el Python global.
REM ------------------------------------------------------------------
echo(
echo [2/3] Instalando dependencias (tarda unos minutos la primera vez)...
if not exist ".venv\Scripts\python.exe" %PY% -m venv .venv
if not exist ".venv\Scripts\python.exe" goto :errvenv
".venv\Scripts\python.exe" -m pip install --upgrade pip --quiet
".venv\Scripts\python.exe" -m pip install -r requirements.txt
if errorlevel 1 goto :errpip

REM ------------------------------------------------------------------
REM 3. ffmpeg. Se descarga una copia portable dentro de esta carpeta,
REM    asi no hace falta ni permisos de administrador ni tocar el PATH.
REM ------------------------------------------------------------------
echo(
echo [3/3] Comprobando ffmpeg...
if exist "bin\ffmpeg\bin\ffmpeg.exe" goto :listo
where ffmpeg >nul 2>&1
if not errorlevel 1 goto :listo
echo      Descargando ffmpeg portable (~90 MB)...
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ErrorActionPreference='Stop';" ^
  "$zip=Join-Path $env:TEMP 'ffmpeg.zip';" ^
  "Invoke-WebRequest -Uri 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip' -OutFile $zip -UseBasicParsing;" ^
  "$tmp=Join-Path $env:TEMP 'ffmpeg_x';" ^
  "if(Test-Path $tmp){Remove-Item $tmp -Recurse -Force};" ^
  "Expand-Archive -Path $zip -DestinationPath $tmp -Force;" ^
  "$src=(Get-ChildItem $tmp -Directory | Select-Object -First 1).FullName;" ^
  "New-Item -ItemType Directory -Force -Path 'bin' | Out-Null;" ^
  "if(Test-Path 'bin\ffmpeg'){Remove-Item 'bin\ffmpeg' -Recurse -Force};" ^
  "Move-Item $src 'bin\ffmpeg';" ^
  "Remove-Item $zip,$tmp -Recurse -Force -ErrorAction SilentlyContinue"
if not exist "bin\ffmpeg\bin\ffmpeg.exe" goto :errffmpeg

:listo
echo(
echo ==========================================================
echo   Listo. Abriendo la interfaz...
echo   La proxima vez basta con "Abrir interfaz.bat".
echo ==========================================================
echo(
call "Abrir interfaz.bat"
exit /b 0

:errvenv
echo(
echo  Error creando el entorno virtual (.venv).
pause
exit /b 1

:errpip
echo(
echo  Fallo instalando las dependencias.
echo  Suele pasar con versiones de Python muy nuevas (3.13/3.14).
echo  Instala Python 3.12 desde python.org, borra la carpeta .venv
echo  y vuelve a ejecutar este instalador.
pause
exit /b 1

:errffmpeg
echo(
echo  No se pudo descargar ffmpeg automaticamente.
echo  Alternativa: abre una terminal y ejecuta   winget install Gyan.FFmpeg
pause
exit /b 1

REM ------------------------------------------------------------------
REM Comprueba que un interprete existe de verdad (el lanzador "py"
REM puede devolver 0 aunque la version pedida no este instalada).
REM ------------------------------------------------------------------
:probar
if defined PY goto :eof
set "OUT="
for /f "delims=" %%v in ('%~1 -c "print(97531)" 2^>nul') do set "OUT=%%v"
if "%OUT%"=="97531" set "PY=%~1"
goto :eof
