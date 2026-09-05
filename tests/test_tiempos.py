"""Pruebas de lo que falla en silencio: los tiempos.

Un error aqui no revienta nada. Sale un short entero, con buena pinta, y los
subtitulos van medio segundo por detras de la voz -- o el encuadre se mueve
cuando ya no toca. Como el resultado es un mp4 y nadie lo mira fotograma a
fotograma, este es justo el sitio donde hacen falta pruebas.

    python -m unittest discover -s tests -t .
"""

import unittest

from viralshorts.config import JumpCutsConfig, LayoutConfig, SubtitlesConfig
from viralshorts.jumpcuts import JumpCutPlan, plan
from viralshorts.render import (_even, _fit_crop, _piecewise_linear,
                                _select_expr)
from viralshorts.subtitles import format_time, group_words, hex_to_ass_color
from viralshorts.transcribe import Word


def words(*spans):
    return [Word(text=f"p{i}", start=a, end=b) for i, (a, b) in enumerate(spans)]


class JumpCuts(unittest.TestCase):

    def test_sin_silencios_no_toca_nada(self):
        cfg = JumpCutsConfig()
        habla = words((0.0, 1.0), (1.1, 2.0), (2.1, 3.0))
        resultado = plan(habla, 3.0, cfg)
        self.assertFalse(resultado.enabled)
        self.assertAlmostEqual(resultado.output_duration, 3.0)

    def test_quita_el_silencio_de_en_medio(self):
        cfg = JumpCutsConfig()
        habla = words((0.0, 2.0), (8.0, 10.0))      # 6 segundos de nada
        resultado = plan(habla, 10.0, cfg)
        self.assertTrue(resultado.enabled)
        self.assertLess(resultado.output_duration, 10.0)
        self.assertGreater(resultado.removed, 4.0)

    def test_remap_es_monotono_y_no_pasa_de_la_duracion(self):
        cfg = JumpCutsConfig()
        habla = words((0.0, 2.0), (8.0, 10.0))
        resultado = plan(habla, 10.0, cfg)

        anterior = -1.0
        for decima in range(0, 101):
            momento = resultado.remap(decima / 10.0)
            self.assertGreaterEqual(momento, anterior - 1e-9)
            self.assertLessEqual(momento, resultado.output_duration + 1e-6)
            anterior = momento

    def test_las_palabras_conservan_el_orden_tras_recortar(self):
        cfg = JumpCutsConfig()
        habla = words((0.0, 1.0), (1.2, 2.0), (9.0, 9.8))
        resultado = plan(habla, 10.0, cfg)
        movidas = resultado.remap_words(habla)

        self.assertEqual(len(movidas), len(habla))
        for antes, despues in zip(movidas, movidas[1:]):
            self.assertLessEqual(antes.start, despues.start + 1e-9)
        for palabra in movidas:
            # Ninguna palabra puede quedarse sin duracion: desapareceria del
            # subtitulo sin que nadie se entere.
            self.assertGreater(palabra.end - palabra.start, 0.0)

    def test_un_clip_todo_silencio_no_se_borra_entero(self):
        cfg = JumpCutsConfig()
        resultado = plan([], 12.0, cfg)
        self.assertAlmostEqual(resultado.output_duration, 12.0)


class ExpresionesDeFfmpeg(unittest.TestCase):

    def test_interpolacion_por_tramos_devuelve_los_extremos(self):
        puntos = [(0.0, 100.0), (2.0, 300.0)]
        expresion = _piecewise_linear(puntos, "t")
        self.assertIn("100.0", expresion)
        self.assertIn("300.0", expresion)
        self.assertIn("if(lt(t,", expresion)

    def test_un_solo_punto_es_una_constante(self):
        self.assertEqual(_piecewise_linear([(0.0, 540.0)], "t"), "540.0")

    def test_select_usa_suma_como_or(self):
        expresion = _select_expr([(0.0, 1.0), (3.0, 4.0)])
        self.assertEqual(expresion.count("between"), 2)
        self.assertIn("+", expresion)

    def test_el_recorte_mantiene_la_proporcion_del_panel(self):
        class Fuente:
            width, height, sar = 1920, 1080, 1.0
            display_width, aspect = 1920, 1920 / 1080

        ancho, alto = _fit_crop(Fuente(), 1080, 960)
        self.assertEqual(ancho % 2, 0)
        self.assertEqual(alto % 2, 0)
        self.assertLessEqual(ancho, 1920)
        self.assertLessEqual(alto, 1080)
        self.assertAlmostEqual(ancho / alto, 1080 / 960, places=1)

    def test_las_dimensiones_siempre_son_pares(self):
        self.assertEqual(_even(961), 960)
        self.assertEqual(_even(960), 960)


class FormatoASS(unittest.TestCase):

    def test_tiempo_en_el_formato_del_ass(self):
        self.assertEqual(format_time(0), "0:00:00.00")
        self.assertEqual(format_time(61.5), "0:01:01.50")
        self.assertEqual(format_time(3661.23), "1:01:01.23")

    def test_los_tiempos_negativos_no_rompen_el_fichero(self):
        self.assertEqual(format_time(-3), "0:00:00.00")

    def test_color_invertido_como_pide_el_formato(self):
        self.assertEqual(hex_to_ass_color("#FFF200"), "&H0000F2FF&")
        self.assertEqual(hex_to_ass_color("#000000"), "&H00000000&")
        with self.assertRaises(ValueError):
            hex_to_ass_color("rojo")

    def test_los_bloques_respetan_el_maximo_de_palabras(self):
        cfg = SubtitlesConfig()
        cfg.max_words_per_group = 3
        habla = words(*[(i * 0.4, i * 0.4 + 0.3) for i in range(10)])
        for bloque in group_words(habla, cfg):
            self.assertLessEqual(len(bloque.words), 3)

    def test_una_pausa_larga_abre_bloque_nuevo(self):
        cfg = SubtitlesConfig()
        cfg.max_words_per_group = 9
        habla = words((0.0, 0.3), (0.4, 0.7), (5.0, 5.3))
        bloques = group_words(habla, cfg)
        self.assertGreaterEqual(len(bloques), 2)


class SubtitulosCompletos(unittest.TestCase):
    """El .ASS entero: la tarjeta y el gancho no pueden coincidir en pantalla."""

    def _construir(self):
        from viralshorts.config import HookConfig, TitleCardConfig
        from viralshorts.subtitles import build_ass

        habla = words(*[(i * 0.5, i * 0.5 + 0.4) for i in range(12)])
        return build_ass(
            words=habla, width=1080, height=1920,
            cfg_subs=SubtitlesConfig(), cfg_hook=HookConfig(),
            hook_text="UN GANCHO", duration=6.0,
            cfg_title=TitleCardConfig(), title_text="El Programa\nInvitado",
        )

    def test_el_gancho_empieza_cuando_se_va_la_tarjeta(self):
        from viralshorts.config import TitleCardConfig

        contenido = self._construir()
        tarjeta = [l for l in contenido.splitlines() if ",Titulo," in l]
        gancho = [l for l in contenido.splitlines() if ",Hook," in l]
        self.assertEqual(len(tarjeta), 1)
        self.assertEqual(len(gancho), 1)

        fin_tarjeta = tarjeta[0].split(",")[2]
        inicio_gancho = gancho[0].split(",")[1]
        self.assertEqual(fin_tarjeta, inicio_gancho)
        self.assertEqual(inicio_gancho, format_time(TitleCardConfig().duration))

    def test_los_subtitulos_no_dejan_huecos_dentro_de_un_bloque(self):
        contenido = self._construir()
        dialogos = [l for l in contenido.splitlines()
                    if l.startswith("Dialogue: 0,")]
        self.assertGreater(len(dialogos), 3)
        for linea in dialogos:
            partes = linea.split(",")
            self.assertLess(partes[1], partes[2])     # inicio < fin, siempre


if __name__ == "__main__":
    unittest.main()
