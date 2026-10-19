@echo off
REM Instala el motor CUDA de whisper.cpp para que la transcripcion use la
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
echo   Descarga unos 700 MB (el motor de transcripcion para GPU,
echo   con las librerias de NVIDIA incluidas) dentro de esta
echo   carpeta. La transcripcion pasa a ir varias veces mas rapida.
echo(
echo   Si tu ordenador no tiene tarjeta NVIDIA, cierra esta
echo   ventana: no te hace falta y el programa ya funciona.
echo ==========================================================
echo(
pause

powershell -NoProfile -ExecutionPolicy Bypass -File "install\instalar.ps1" -Gpu
if errorlevel 1 goto :error

echo(
echo  Listo. La proxima vez que generes shorts se usara la GPU.
echo  Para comprobarlo: install\Comprobar GPU.bat
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
