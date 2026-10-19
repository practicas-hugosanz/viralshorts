@echo off
REM ==========================================================================
REM  viralshorts - unico fichero que hay que tocar.
REM
REM  Doble clic aqui:
REM    - la primera vez descarga solo lo que falta (ffmpeg, yt-dlp y
REM      whisper.cpp, dentro de esta carpeta) y abre la interfaz;
REM    - las siguientes veces abre la interfaz directamente.
REM ==========================================================================
setlocal
cd /d "%~dp0"
title viralshorts

REM El programa es un unico viralshorts.exe. Si no esta al lado (carpeta de
REM desarrollo), se busca el que deja "cargo build --release".
set "EXE=%~dp0viralshorts.exe"
if not exist "%EXE%" set "EXE=%~dp0rust\target\release\viralshorts.exe"
if not exist "%EXE%" goto :sin_programa

REM Si falta cualquiera de las herramientas, la instalacion no esta terminada.
REM El instalador es idempotente: si ya estaba todo, no rehace nada.
if not exist "install\.instalado" goto :instalar
if not exist "bin\whisper\whisper-cli.exe" goto :instalar
if not exist "bin\yt-dlp.exe" goto :instalar
goto :abrir

:instalar
powershell -NoProfile -ExecutionPolicy Bypass -File "install\instalar.ps1"
if errorlevel 1 goto :fallo

:abrir
REM Las herramientas portables se anaden al PATH solo para este proceso; el
REM programa tambien las busca solo en bin\, esto cubre el resto de casos.
if exist "%~dp0bin\ffmpeg\bin\ffmpeg.exe" set "PATH=%~dp0bin\ffmpeg\bin;%PATH%"
if exist "%~dp0bin\yt-dlp.exe" set "PATH=%~dp0bin;%PATH%"

"%EXE%" gui
if errorlevel 1 goto :fallo_arranque
exit /b 0

:sin_programa
echo(
echo  No encuentro viralshorts.exe.
echo  Si has descargado la carpeta del codigo, compilalo con:
echo      cargo build --release        (dentro de la carpeta "rust")
echo  Si has recibido un ZIP, descomprimelo ENTERO y vuelve a abrir este fichero.
echo(
pause
exit /b 1

:fallo
echo(
echo  La instalacion no termino. Arriba esta el motivo.
echo  Si quieres reintentarlo desde cero: install\Reparar instalacion.bat
echo(
pause
exit /b 1

:fallo_arranque
echo(
echo  La interfaz se cerro con un error.
echo  Prueba con: install\Reparar instalacion.bat
echo(
pause
exit /b 1
