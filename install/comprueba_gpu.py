#!/usr/bin/env python3
"""Dice si la transcripcion va a poder usar la tarjeta grafica.

Sirve para dos cosas: comprobar que 'Activar GPU NVIDIA.bat' ha funcionado, y
diagnosticar sin tener que lanzar un pipeline entero de veinte minutos.

    .venv\\Scripts\\python.exe install\\comprueba_gpu.py
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from viralshorts import transcribe   # noqa: E402


def main() -> int:
    try:
        import ctranslate2
    except ImportError:
        print("ctranslate2 no esta instalado: ejecuta antes viralshorts.bat")
        return 1

    tarjetas = ctranslate2.get_cuda_device_count()
    print(f"Tarjetas NVIDIA detectadas: {tarjetas}")
    if tarjetas <= 0:
        print()
        print("No hay GPU NVIDIA utilizable. Se transcribira con el procesador,")
        print("que funciona igual de bien pero tarda mas. No hay nada que hacer.")
        return 0

    transcribe._registra_dlls_de_nvidia()
    if transcribe._librerias_cuda_cargables():
        print("Librerias CUDA (cuBLAS): se cargan correctamente")
        print()
        print("Todo listo: la transcripcion usara la tarjeta grafica.")
        return 0

    print("Librerias CUDA (cuBLAS): NO se pueden cargar")
    print()
    print("Hay tarjeta, pero le faltan las librerias. Ejecuta")
    print("  install\\Activar GPU NVIDIA.bat")
    print("Mientras tanto el programa funciona con el procesador.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
