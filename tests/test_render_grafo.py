"""El filtergraph del render, sin llamar a ffmpeg.

Es lo que mas facil se rompe sin que nadie se entere: un recorte con la
proporcion equivocada sale estirado y el short se genera igualmente. Aqui se
comprueba la geometria y la estructura del grafo, no su ejecucion.
"""

import re
import unittest
from pathlib import Path
from unittest import mock

from viralshorts import render
from viralshorts.config import Config
from viralshorts.facecrop import Track
from viralshorts.ffmpeg_utils import MediaInfo
from viralshorts.jumpcuts import JumpCutPlan
from viralshorts.render import RenderRequest


def video(width=1920, height=1080, sar=1.0):
    return MediaInfo(path="v.mp4", width=width, height=height, duration=600.0,
                     fps=30.0, has_audio=True, sar=sar)


def peticion(plan=None, track=None, ass="short_1.ass"):
    return RenderRequest(
        podcast=Path("p.mp4"), background=Path("b.mp4"), start=10.0, end=40.0,
        background_offset=0.0,
        plan=plan or JumpCutPlan(keeps=[(0.0, 30.0)], source_duration=30.0),
        track=track, ass_name=ass, output=Path("short_1.mp4"), work_dir=Path("w"))


def grafo(cfg=None, podcast=None, fondo=None, soporta_eval=False, **kwargs):
    cfg = cfg or Config.load(None)
    alto_arriba = render._even(int(cfg.layout.height * cfg.layout.top_ratio))
    with mock.patch.object(render, "filter_supports", return_value=soporta_eval):
        return render._build_graph(
            request=peticion(**kwargs), podcast_info=podcast or video(),
            background_info=fondo or video(), top_height=alto_arriba,
            bottom_height=cfg.layout.height - alto_arriba, cfg=cfg)


class Geometria(unittest.TestCase):
    def test_el_recorte_tiene_la_proporcion_del_panel(self):
        for origen in (video(1920, 1080), video(1280, 720), video(1080, 1920),
                       video(3840, 2160), video(720, 576, sar=16 / 11)):
            for ancho, alto in ((1080, 960), (1080, 1280), (1080, 640)):
                with self.subTest(origen=(origen.width, origen.height, origen.sar),
                                  panel=(ancho, alto)):
                    w, h = render._fit_crop(origen, ancho, alto)
                    self.assertAlmostEqual(w / h, ancho / alto, delta=0.01)
                    self.assertLessEqual(w, origen.display_width)
                    self.assertLessEqual(h, origen.height)
                    self.assertEqual((w % 2, h % 2), (0, 0))

    def test_los_dos_paneles_suman_el_alto_de_la_pantalla(self):
        for ratio in (0.4, 0.5, 0.55, 0.61):
            cfg = Config.load(None)
            cfg.layout.top_ratio = ratio
            arriba = render._even(int(cfg.layout.height * ratio))
            self.assertEqual(arriba % 2, 0)
            self.assertGreater(cfg.layout.height - arriba, 0)

    def test_pixeles_no_cuadrados_se_corrigen_antes_de_recortar(self):
        anamorfico = video(720, 576, sar=16 / 11)
        self.assertTrue(render._square_pixels(anamorfico))
        self.assertEqual(render._square_pixels(video()), [])

    def test_cover_no_estira(self):
        filtros = render._cover(1080, 960)
        self.assertIn("force_original_aspect_ratio=increase", filtros[0])
        self.assertEqual(filtros[1], "crop=1080:960")


class Estructura(unittest.TestCase):
    def test_las_etiquetas_que_ffmpeg_mapea_existen(self):
        g = grafo()
        for etiqueta in ("[top]", "[bottom]", "[vout]", "[aout]", "vstack=inputs=2"):
            self.assertIn(etiqueta, g)

    def test_cada_etiqueta_se_produce_y_se_consume_una_vez(self):
        g = grafo()
        for etiqueta in ("top", "bottom"):
            self.assertEqual(g.count(f"[{etiqueta}]"), 2, etiqueta)

    def test_subtitulos_solo_si_hay_fichero_ass(self):
        self.assertIn("subtitles=filename=short_1.ass", grafo())
        self.assertNotIn("subtitles=", grafo(ass=None))

    def test_carpeta_de_fuentes_propia(self):
        cfg = Config.load(None)
        cfg.subtitles.fonts_dir = "fuentes"
        self.assertIn(":fontsdir=", grafo(cfg))

    def test_normalizacion_de_volumen(self):
        cfg = Config.load(None)
        self.assertIn("loudnorm=I=", grafo(cfg))
        cfg.audio.loudnorm = False
        self.assertNotIn("loudnorm", grafo(cfg))

    def test_zoom_se_puede_apagar(self):
        cfg = Config.load(None)
        self.assertIn("zoompan=", grafo(cfg))
        cfg.zoom.enabled = False
        self.assertNotIn("zoompan", grafo(cfg))

    def test_el_zoom_mantiene_el_tamano_del_panel(self):
        self.assertIn("s=1080x960", grafo())

    def test_eval_frame_solo_si_este_ffmpeg_lo_acepta(self):
        self.assertIn(":eval=frame", grafo(soporta_eval=True))
        self.assertNotIn(":eval=frame", grafo(soporta_eval=False))

    def test_el_audio_sale_en_stereo_a_48k(self):
        g = grafo()
        self.assertIn("aresample=48000", g)
        self.assertIn("channel_layouts=stereo", g)

    def test_el_fondo_se_recorta_centrado_al_panel_inferior(self):
        g = grafo()
        bottom = [c for c in g.split(";\n") if c.endswith("[bottom]")][0]
        self.assertIn("(in_w-", bottom)
        self.assertIn("crop=1080:960", bottom)


class SilenciosEliminados(unittest.TestCase):
    PLAN = JumpCutPlan(keeps=[(0.0, 5.0), (7.0, 12.0)], source_duration=12.0)

    def test_con_cortes_video_y_audio_usan_la_misma_seleccion(self):
        g = grafo(plan=self.PLAN)
        expresion = render._select_expr(self.PLAN.keeps)
        self.assertIn(f"select='{expresion}'", g)
        self.assertIn(f"aselect='{expresion}'", g)
        self.assertEqual(expresion, "between(t,0.000,5.000)+between(t,7.000,12.000)")

    def test_sin_cortes_no_hay_seleccion(self):
        self.assertNotIn("select=", grafo())


def evaluar(expresion, t):
    """Evalua una expresion de ffmpeg que solo usa if(), lt(), t y aritmetica."""
    codigo = re.sub(r"\bif\(", "_if(", expresion)
    codigo = re.sub(r"\blt\(", "_lt(", codigo)
    return eval(codigo, {"_if": lambda c, a, b: a if c else b,
                         "_lt": lambda a, b: a < b, "t": t})


class Seguimiento(unittest.TestCase):
    def test_sin_trayectoria_el_centro_es_el_del_video(self):
        sin_corte = JumpCutPlan(keeps=[(0.0, 30.0)], source_duration=30.0)
        self.assertEqual(render._centre_expression(None, sin_corte, 1920, "t"), "960.0")

    def test_interpolacion_lineal_entre_puntos(self):
        puntos = [(0.0, 100.0), (10.0, 300.0), (20.0, 300.0), (30.0, 100.0)]
        e = render._piecewise_linear(puntos, "t")
        for t, esperado in ((-5, 100), (0, 100), (5, 200), (10, 300), (15, 300),
                            (25, 200), (30, 100), (99, 100)):
            with self.subTest(t=t):
                self.assertAlmostEqual(evaluar(e, t), esperado, delta=0.5)

    def test_un_solo_punto_es_constante(self):
        self.assertEqual(render._piecewise_linear([(1.0, 640.0)], "t"), "640.0")

    def test_la_trayectoria_sigue_a_la_imagen_tras_quitar_silencios(self):
        # La cara pasa de la izquierda (0.25) a la derecha (0.75) en el
        # segundo 8, que cae en el hueco eliminado [5, 7]: tras el corte esos
        # dos segundos no existen y el segundo 8 original es el 6 del video.
        plan = JumpCutPlan(keeps=[(0.0, 5.0), (7.0, 12.0)], source_duration=12.0)
        pista = Track(points=[(0.0, 0.25), (8.0, 0.25), (9.0, 0.75), (12.0, 0.75)],
                      detection_ratio=1.0)
        e = render._centre_expression(pista, plan, 1000, "t")
        self.assertAlmostEqual(evaluar(e, 0.0), 250, delta=1)
        self.assertAlmostEqual(evaluar(e, 5.9), 250, delta=1)
        self.assertAlmostEqual(evaluar(e, 7.0), 750, delta=1)
        self.assertAlmostEqual(evaluar(e, 10.0), 750, delta=1)

    def test_el_recorte_horizontal_nunca_se_sale_del_video(self):
        pista = Track(points=[(0.0, 0.0), (10.0, 1.0)], detection_ratio=1.0)
        g = grafo(track=pista)
        self.assertIn("clip(", g)


class Codificador(unittest.TestCase):
    def setUp(self):
        self.addCleanup(setattr, render, "_ENCODER_CHOICE", render._ENCODER_CHOICE)

    def argumentos(self, codificador):
        render._ENCODER_CHOICE = codificador
        return render._video_encoder_args(Config.load(None).output)

    def test_cpu(self):
        a = self.argumentos("libx264")
        self.assertEqual(a[:2], ["-c:v", "libx264"])
        self.assertIn("-crf", a)

    def test_nvenc(self):
        a = self.argumentos("h264_nvenc")
        self.assertEqual(a[:2], ["-c:v", "h264_nvenc"])
        self.assertIn("-cq", a)
        self.assertNotIn("-crf", a)

    def test_codificador_desconocido_se_pasa_tal_cual(self):
        self.assertEqual(self.argumentos("h264_mf")[:2], ["-c:v", "h264_mf"])


if __name__ == "__main__":
    unittest.main()
