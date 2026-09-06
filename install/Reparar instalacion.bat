@echo off
REM Rehace la instalacion desde cero: borra el entorno virtual y vuelve a
REM instalar las dependencias. No borra tus videos ni tu configuracion.
setlocal
cd /d "%~dp0.."
title viralshorts - reparar

echo(
echo  Se va a rehacer la instalacion del programa.
echo  Tus videos (carpeta output) y tu config.yaml NO se tocan.
echo(
pause

powershell -NoProfile -ExecutionPolicy Bypass -File "install\setup.ps1" -Force
if errorlevel 1 (
  echo(
  echo  La reparacion fallo. Mira install\log-instalacion.txt
  echo(
  pause
  exit /b 1
)

call "viralshorts.bat"
