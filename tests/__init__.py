"""Pruebas de viralshorts.

    python -m unittest discover -s tests -t .

Ninguna prueba llama a ffmpeg, a YouTube ni a la API: todas trabajan sobre
datos inventados, asi que corren en menos de un segundo y se pueden lanzar
despues de cualquier cambio.
"""

import logging

# Los modulos avisan por consola de cosas que aqui son justamente lo que se
# esta probando (un video mas corto que los margenes, la API caida...).
logging.disable(logging.CRITICAL)
