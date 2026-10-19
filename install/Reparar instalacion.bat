@echo off
REM Rehace la instalacion desde cero: vuelve a descargar ffmpeg, yt-dlp y
REM whisper.cpp. No borra tus videos, tus modelos ni tu configuracion.
setlocal
cd /d "%~dp0.."
title viralshorts - reparar

echo(
echo  Se va a rehacer la instalacion de las herramientas.
echo  Tus videos (carpeta output) y tu config.yaml NO se tocan.
echo(
pause

powershell -NoProfile -ExecutionPolicy Bypass -File "install\instalar.ps1" -Force
if errorlevel 1 (
  echo(
  echo  La reparacion fallo. Mira install\log-instalacion.txt
  echo(
  pause
  exit /b 1
)

call "viralshorts.bat"
