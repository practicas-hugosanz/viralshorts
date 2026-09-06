@echo off
REM Instala las librerias CUDA que necesita la transcripcion para usar la
REM tarjeta grafica. Solo tiene sentido con una GPU NVIDIA.
REM
REM Sin esto el programa funciona igual, pero transcribe con el procesador y
REM tarda bastante mas.
setlocal
cd /d "%~dp0.."
title viralshorts - activar GPU NVIDIA

echo(
echo ==========================================================
echo   Activar la tarjeta grafica NVIDIA
echo(
echo   Descarga unos 600 MB de librerias de NVIDIA (cuBLAS y
echo   cuDNN) dentro de esta carpeta. La transcripcion pasa a
echo   ir varias veces mas rapida.
echo(
echo   Si tu ordenador no tiene tarjeta NVIDIA, cierra esta
echo   ventana: no te hace falta y el programa ya funciona.
echo ==========================================================
echo(
pause

if not exist ".venv\Scripts\python.exe" (
  echo(
  echo  Primero hay que instalar el programa: viralshorts.bat
  echo(
  pause
  exit /b 1
)

echo(
echo  Instalando (tarda unos minutos)...
".venv\Scripts\python.exe" -m pip install "nvidia-cublas-cu12" "nvidia-cudnn-cu12>=9,<10" --disable-pip-version-check
if errorlevel 1 goto :error

echo(
echo  Comprobando...
".venv\Scripts\python.exe" "install\comprueba_gpu.py"
if errorlevel 1 goto :error

echo(
echo  Listo. La proxima vez que generes shorts se usara la GPU.
echo(
pause
exit /b 0

:error
echo(
echo  No se pudo activar la GPU.
echo  No pasa nada: el programa sigue funcionando con el procesador.
echo(
pause
exit /b 1
