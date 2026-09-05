"""Cerrojo sobre la carpeta de trabajo.

Dos ejecuciones a la vez sobre el mismo `work/` se pisan: yt-dlp escribe el
mismo `.part`, no puede renombrarlo y falla con WinError 32 despues de haberse
tirado varios minutos descargando. Tambien se pisarian los `.ass` y los
ficheros de filtros del render.

Este cerrojo hace que la segunda ejecucion falle en el primer segundo con un
mensaje claro, en vez de a la media hora con uno incomprensible.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Optional


class WorkDirBusy(RuntimeError):
    """Otra ejecucion esta usando la misma carpeta de trabajo."""


class WorkLock:
    """Cerrojo de fichero, exclusivo y no bloqueante.

    Se apoya en el bloqueo del sistema operativo (msvcrt en Windows, flock en
    el resto) en lugar de en "existe el fichero": si el proceso muere de golpe,
    el sistema suelta el cerrojo solo y no queda un candado fantasma que
    obligue a borrarlo a mano.
    """

    def __init__(self, work_dir: Path) -> None:
        self.work_dir = Path(work_dir)
        self.path = self.work_dir / ".lock"
        self._fd: Optional[int] = None

    def __enter__(self) -> "WorkLock":
        self.acquire()
        return self

    def __exit__(self, *exc_info) -> None:
        self.release()

    def acquire(self) -> None:
        self.work_dir.mkdir(parents=True, exist_ok=True)
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT)
        try:
            _lock(fd)
        except OSError as exc:
            os.close(fd)
            raise WorkDirBusy(
                f"La carpeta de trabajo '{self.work_dir}' ya la esta usando otra "
                "ejecucion (otra ventana, la interfaz, o un proceso que se quedo "
                "colgado).\n"
                "  - Espera a que termine, o\n"
                "  - lanza esta con otra carpeta: paths.work_dir en el config.yaml."
            ) from exc

        # No escribimos nada dentro: en Windows el bloqueo por rango impide
        # incluso leer ese byte desde otro proceso, asi que un PID guardado ahi
        # no serviria para nada.
        self._fd = fd

    def release(self) -> None:
        if self._fd is None:
            return
        try:
            _unlock(self._fd)
        except OSError:
            pass          # al cerrar el descriptor el SO lo suelta igualmente
        finally:
            os.close(self._fd)
            self._fd = None
            # No borramos el .lock: en Windows otro proceso puede tenerlo
            # abierto y borrarlo daria error. Un fichero vacio no molesta.


# ----------------------------------------------------------------------------

if os.name == "nt":
    import msvcrt

    def _lock(fd: int) -> None:
        msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)

    def _unlock(fd: int) -> None:
        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)

else:
    import fcntl

    def _lock(fd: int) -> None:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def _unlock(fd: int) -> None:
        fcntl.flock(fd, fcntl.LOCK_UN)
