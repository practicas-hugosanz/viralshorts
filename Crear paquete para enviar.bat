@echo off
REM Genera en el Escritorio un ZIP listo para mandarle a otra persona.
REM Solo lo necesitas tu; el que lo recibe no tiene que ejecutar esto.
setlocal
cd /d "%~dp0"
title viralshorts - crear paquete

powershell -NoProfile -ExecutionPolicy Bypass -File "install\empaquetar.ps1"
if errorlevel 1 (
  echo(
  echo  No se pudo crear el paquete.
  echo(
)
pause
