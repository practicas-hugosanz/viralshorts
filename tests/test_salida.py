"""Una carpeta por ejecucion: el segundo podcast ya no pisa los shorts del primero."""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

import pipeline
from viralshorts import exports
from viralshorts.config import Config


class NombreDeCarpeta(unittest.TestCase):
    def test_titulo_con_tildes_y_simbolos(self):
        carpeta = exports.run_folder(Path("output"), "¿Qué es la Inflación? | Ep. 12", "20260101_101500")
        self.assertEqual(carpeta, Path("output") / "que-es-la-inflacion-ep-12_20260101_101500")

    def test_titulo_vacio_o_solo_simbolos(self):
        for titulo in ("", "???", "日本語"):
            with self.subTest(titulo=titulo):
                self.assertEqual(exports.run_folder(Path("o"), titulo, "t").name, "podcast_t")

    def test_titulo_larguisimo_no_revienta_la_ruta(self):
        nombre = exports.run_folder(Path("o"), "palabra " * 100, "t").name
        self.assertLessEqual(len(nombre), 42)
        self.assertFalse(nombre.startswith("-"))

    def test_dos_podcasts_dos_carpetas(self):
        uno = exports.run_folder(Path("o"), "Podcast A", "20260101_100000")
        otro = exports.run_folder(Path("o"), "Podcast B", "20260101_100000")
        self.assertNotEqual(uno, otro)


class CarpetaDeLaEjecucion(unittest.TestCase):
    def test_crea_la_subcarpeta_y_avisa_a_la_interfaz(self):
        cfg = Config.load(None)
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(pipeline.progress, "result") as avisar:
            carpeta = pipeline._run_output_dir(cfg, Path(tmp), "Mi Podcast", "tag")
            self.assertEqual(carpeta, Path(tmp) / "mi-podcast_tag")
            self.assertTrue(carpeta.is_dir())
            avisar.assert_called_once_with("salida", ruta=str(carpeta.absolute()))

    def test_sin_subcarpeta_todo_va_a_la_base(self):
        cfg = Config.load(None)
        cfg.output.run_subfolder = False
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(pipeline.progress, "result"):
            self.assertEqual(pipeline._run_output_dir(cfg, Path(tmp), "x", "t"), Path(tmp))

    def test_montar_dos_veces_el_mismo_plan_reutiliza_la_carpeta(self):
        cfg = Config.load(None)
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(pipeline.progress, "result"):
            tag = Path("work/plans/plan_20260101_101500.json").stem.removeprefix("plan_")
            primera = pipeline._run_output_dir(cfg, Path(tmp), "Pod", tag)
            segunda = pipeline._run_output_dir(cfg, Path(tmp), "Pod", tag)
            self.assertEqual(primera, segunda)


if __name__ == "__main__":
    unittest.main()
