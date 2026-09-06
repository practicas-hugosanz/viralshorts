@echo off
REM Dice si la transcripcion va a usar la tarjeta grafica o el procesador.
setlocal
cd /d "%~dp0.."
title viralshorts - comprobar GPU

if not exist ".venv\Scripts\python.exe" (
  echo(
  echo  Primero hay que instalar el programa: viralshorts.bat
  echo(
  pause
  exit /b 1
)

echo(
".venv\Scripts\python.exe" "install\comprueba_gpu.py"
echo(
pause
