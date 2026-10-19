@echo off
REM Dice si la transcripcion va a usar la tarjeta grafica o el procesador, y
REM comprueba de paso que el resto de herramientas estan en su sitio.
setlocal
cd /d "%~dp0.."
title viralshorts - comprobar

set "EXE=%~dp0..\viralshorts.exe"
if not exist "%EXE%" set "EXE=%~dp0..\rust\target\release\viralshorts.exe"
if not exist "%EXE%" (
  echo(
  echo  No encuentro viralshorts.exe.
  echo(
  pause
  exit /b 1
)

if exist "bin\ffmpeg\bin\ffmpeg.exe" set "PATH=%CD%\bin\ffmpeg\bin;%PATH%"
if exist "bin\yt-dlp.exe" set "PATH=%CD%\bin;%PATH%"

echo(
"%EXE%" doctor
echo(
pause
