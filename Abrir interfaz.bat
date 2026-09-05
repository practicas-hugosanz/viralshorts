@echo off
REM Doble clic aqui para abrir la interfaz visual de viralshorts.
REM Si es la primera vez, ejecuta antes INSTALAR.bat
setlocal
cd /d "%~dp0"
title viralshorts

REM ffmpeg portable descargado por el instalador, si existe.
if exist "%~dp0bin\ffmpeg\bin\ffmpeg.exe" set "PATH=%~dp0bin\ffmpeg\bin;%PATH%"

set "PYEXE=%~dp0.venv\Scripts\python.exe"
if not exist "%PYEXE%" set "PYEXE=python"

"%PYEXE%" gui.py
if errorlevel 1 (
  echo(
  echo No se pudo arrancar. Ejecuta primero INSTALAR.bat
  echo(
  pause
)
