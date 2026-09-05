"""Comprobacion de que la fuente de los subtitulos existe en el sistema.

libass no avisa cuando no encuentra una fuente: la sustituye por la que sea y
sigue. El resultado es un short entero con subtitulos que no se parecen en nada
a los que se configuraron, y no hay forma de enterarse hasta verlo.

Aqui se comprueba ANTES de renderizar. La comprobacion es deliberadamente
optimista: si no se puede saber (sistema raro, sin herramientas), se dice que
si esta y no se molesta al usuario con una alarma falsa.
"""

from __future__ import annotations

import logging
import os
import subprocess
import sys
from pathlib import Path
from typing import List, Optional, Set

log = logging.getLogger(__name__)

_CACHE: Optional[Set[str]] = None


def _normalise(name: str) -> str:
    return "".join(ch for ch in name.lower() if ch.isalnum())


def installed_families() -> Set[str]:
    """Nombres de familia normalizados que hay instalados. Vacio = no se sabe."""
    global _CACHE
    if _CACHE is None:
        try:
            _CACHE = _scan()
        except Exception as exc:            # nunca romper el pipeline por esto
            log.debug("No se pudo listar las fuentes del sistema: %s", exc)
            _CACHE = set()
    return _CACHE


def is_available(font_name: str, fonts_dir: str | None = None) -> bool:
    """True si la fuente se puede usar (o si no hay forma de comprobarlo)."""
    if not font_name:
        return True

    if fonts_dir:
        # Con fontsdir libass carga los .ttf de esa carpeta aunque no esten
        # instalados; comprobarlo por nombre de fichero es aproximado pero
        # suficiente para no dar una alarma falsa.
        folder = Path(fonts_dir)
        if folder.is_dir():
            wanted = _normalise(font_name)
            for path in folder.glob("*"):
                if path.suffix.lower() in (".ttf", ".otf", ".ttc"):
                    if wanted in _normalise(path.stem):
                        return True

    families = installed_families()
    if not families:
        return True                          # no se sabe: no alarmar

    wanted = _normalise(font_name)
    return any(wanted == fam or fam.startswith(wanted) for fam in families)


def _scan() -> Set[str]:
    if sys.platform == "win32":
        return _scan_windows()
    if sys.platform == "darwin":
        return _scan_fc_list() or _scan_dirs([
            Path("/System/Library/Fonts"), Path("/Library/Fonts"),
            Path.home() / "Library/Fonts",
        ])
    return _scan_fc_list() or _scan_dirs([
        Path("/usr/share/fonts"), Path("/usr/local/share/fonts"),
        Path.home() / ".fonts", Path.home() / ".local/share/fonts",
    ])


def _scan_windows() -> Set[str]:
    """Nombres del registro: '(TrueType)' y las variantes van en el mismo valor."""
    import winreg

    found: Set[str] = set()
    roots = [
        (winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts"),
        (winreg.HKEY_CURRENT_USER, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts"),
    ]
    for hive, path in roots:
        try:
            key = winreg.OpenKey(hive, path)
        except OSError:
            continue
        with key:
            index = 0
            while True:
                try:
                    name, _value, _kind = winreg.EnumValue(key, index)
                except OSError:
                    break
                index += 1
                # "Arial Black (TrueType)" -> "Arial Black"; ademas cada estilo
                # ("Arial Black Bold") se guarda aparte, asi que anadimos
                # tambien la familia sin el estilo final.
                clean = name.split("(")[0].strip()
                for variant in _family_variants(clean):
                    found.add(_normalise(variant))
    return found


def _family_variants(name: str) -> List[str]:
    """'Arial Black Bold Italic' -> ['Arial Black Bold Italic', ..., 'Arial']."""
    parts = name.replace("&", " ").split()
    variants = []
    while parts:
        variants.append(" ".join(parts))
        parts = parts[:-1]
    return variants


def _scan_fc_list() -> Set[str]:
    try:
        result = subprocess.run(["fc-list", "--format", "%{family}\n"],
                                capture_output=True, text=True, timeout=15)
    except (OSError, subprocess.SubprocessError):
        return set()
    if result.returncode != 0:
        return set()
    found: Set[str] = set()
    for line in result.stdout.splitlines():
        for family in line.split(","):
            if family.strip():
                found.add(_normalise(family.strip()))
    return found


def _scan_dirs(folders: List[Path]) -> Set[str]:
    found: Set[str] = set()
    for folder in folders:
        if not folder.is_dir():
            continue
        for path in folder.rglob("*"):
            if path.suffix.lower() in (".ttf", ".otf", ".ttc"):
                found.add(_normalise(path.stem))
    return found


def suggest_alternative(font_name: str) -> str:
    """Fuente instalada que mas se parezca a la pedida, para el aviso."""
    families = installed_families()
    for candidate in ("Arial Black", "Impact", "Montserrat ExtraBold",
                      "Anton", "Bebas Neue", "Verdana Bold", "Arial",
                      "DejaVu Sans Bold", "Liberation Sans Bold"):
        if _normalise(candidate) in families:
            return candidate
    return "Arial"
