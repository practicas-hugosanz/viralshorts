@echo off
REM ==========================================================================
REM  viralshorts - unico fichero que hay que tocar.
REM
REM  Doble clic aqui:
REM    - la primera vez instala todo solo (Python, dependencias, ffmpeg)
REM      y abre la interfaz;
REM    - las siguientes veces abre la interfaz directamente.
REM ==========================================================================
setlocal
cd /d "%~dp0"
title viralshorts

REM Si falta cualquiera de las dos senales, la instalacion no esta terminada.
REM El instalador es idempotente: si ya estaba todo, no rehace nada.
if not exist ".venv\Scripts\python.exe" goto :instalar
if not exist "install\.instalado" goto :instalar
goto :abrir

:instalar
powershell -NoProfile -ExecutionPolicy Bypass -File "install\setup.ps1"
if errorlevel 1 goto :fallo

:abrir
REM La copia portable de ffmpeg vive dentro de la carpeta: se anade al PATH
REM solo para este proceso, no se toca la configuracion del ordenador.
if exist "%~dp0bin\ffmpeg\bin\ffmpeg.exe" set "PATH=%~dp0bin\ffmpeg\bin;%PATH%"

".venv\Scripts\python.exe" gui.py
if errorlevel 1 goto :fallo_arranque
exit /b 0

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
