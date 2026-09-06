"""Pruebas de compatibilidad con las distintas versiones de ffmpeg.

El caso real: una compilacion recien salida de la rama de desarrollo ya no
trae `-filter_complex_script` (la quitaron despues de la 8.1) y fallaban los
tres shorts seguidos con "Unrecognized option 'filter_complex_script'", cuando
ya se habian invertido once minutos en transcribir.

Aqui no se llama a ffmpeg: se sustituye la deteccion, que es lo unico que
cambia el comportamiento.
"""

import tempfile
import unittest
from pathlib import Path

from viralshorts import ffmpeg_utils as F


class OpcionDelFiltergraph(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.destino = Path(self.tmp.name) / "grafo.filter"
        self.addCleanup(self.tmp.cleanup)
        # La deteccion se cachea en un global: hay que dejarlo como estaba.
        self.cache = F._SOPORTA_FILTER_SCRIPT
        self.addCleanup(setattr, F, "_SOPORTA_FILTER_SCRIPT", self.cache)

    def test_ffmpeg_con_la_opcion_clasica(self):
        F._SOPORTA_FILTER_SCRIPT = True
        args = F.write_filter_script("nullsrc[vout]", self.destino)
        self.assertEqual(args[0], "-filter_complex_script")

    def test_ffmpeg_que_ya_no_la_trae(self):
        F._SOPORTA_FILTER_SCRIPT = False
        args = F.write_filter_script("nullsrc[vout]", self.destino)
        self.assertEqual(args[0], "-/filter_complex")

    def test_el_grafo_se_escribe_y_la_ruta_es_absoluta(self):
        for soporta in (True, False):
            with self.subTest(soporta=soporta):
                F._SOPORTA_FILTER_SCRIPT = soporta
                args = F.write_filter_script("color=c=red[vout]", self.destino)

                self.assertEqual(len(args), 2)
                # Absoluta o no, ffmpeg corre con cwd en la carpeta de trabajo
                # y una ruta relativa se resolveria dos veces contra ella.
                self.assertTrue(Path(args[1]).is_absolute())
                self.assertEqual(self.destino.read_text(encoding="utf-8"),
                                 "color=c=red[vout]")


class DeteccionDeLaOpcion(unittest.TestCase):
    def setUp(self):
        self.cache = F._SOPORTA_FILTER_SCRIPT
        self.original = F.subprocess.run
        self.addCleanup(setattr, F, "_SOPORTA_FILTER_SCRIPT", self.cache)
        self.addCleanup(setattr, F.subprocess, "run", self.original)

    def _finge_ayuda(self, texto):
        class Resultado:
            stdout = texto
        F._SOPORTA_FILTER_SCRIPT = None
        F.subprocess.run = lambda *a, **k: Resultado()

    def test_la_encuentra_en_la_ayuda(self):
        self._finge_ayuda("-filter_complex_script filename  read complex filtergraph")
        self.assertTrue(F.supports_filter_complex_script())

    def test_no_esta_en_una_compilacion_nueva(self):
        self._finge_ayuda("-filter_complex graph  set complex filtergraph")
        self.assertFalse(F.supports_filter_complex_script())

    def test_sin_ffmpeg_no_revienta(self):
        F._SOPORTA_FILTER_SCRIPT = None
        def explota(*a, **k):
            raise OSError("no existe el binario")
        F.subprocess.run = explota
        # Que falte ffmpeg lo dice require_ffmpeg mucho antes y con un mensaje
        # util; aqui solo hace falta no romper por el camino.
        self.assertFalse(F.supports_filter_complex_script())


if __name__ == "__main__":
    unittest.main()
