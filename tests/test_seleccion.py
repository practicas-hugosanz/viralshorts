"""Pruebas de la parte que decide QUE se monta y con que ajustes.

Cubre la generacion de candidatos, la eleccion final (incluida la nueva
deduplicacion por tema), la configuracion, los `--set` de la linea de comandos,
el plan de montaje y los lotes y la cache de las llamadas al modelo.
"""

import json
import tempfile
import unittest
from pathlib import Path

import pipeline
from viralshorts import exports, fonts, scoring
from viralshorts.config import ClipsConfig, Config, LLMConfig, ScoringConfig
from viralshorts.llm import ClipJudge, ClipMetadata
from viralshorts.scoring import Window
from viralshorts.transcribe import Segment, Transcript, Word


def frase(inicio: float, palabras: str, paso: float = 0.4) -> Segment:
    """Un segmento de Whisper que termina en punto, como una frase de verdad."""
    trozos = palabras.split()
    lista = []
    momento = inicio
    for i, texto in enumerate(trozos):
        final = i == len(trozos) - 1
        lista.append(Word(text=texto + ("." if final else ""),
                          start=momento, end=momento + paso * 0.8))
        momento += paso
    return Segment(start=inicio, end=momento, text=palabras, words=lista)


def transcripcion(duracion=600.0, tema="bitcoin dinero banco inversion") -> Transcript:
    segmentos = []
    momento = 0.0
    while momento < duracion - 12:
        segmentos.append(frase(momento, f"hablamos de {tema} en el minuto {int(momento)}"))
        momento += 12.0
    return Transcript(language="es", duration=duracion, segments=segmentos)


class Candidatos(unittest.TestCase):

    def test_las_ventanas_respetan_duracion_y_margenes(self):
        cfg = ClipsConfig()
        cfg.min_duration, cfg.max_duration = 25, 45
        cfg.head_skip, cfg.tail_skip = 60, 30

        ventanas = scoring.build_windows(transcripcion(), cfg)
        self.assertTrue(ventanas)
        for v in ventanas:
            self.assertGreaterEqual(v.duration, cfg.min_duration - 1e-6)
            self.assertLessEqual(v.duration, cfg.max_duration + 1e-6)
            self.assertGreaterEqual(v.start, cfg.head_skip)
            self.assertLessEqual(v.end, 600.0 - cfg.tail_skip)

    def test_un_video_mas_corto_que_los_margenes_no_se_queda_sin_clips(self):
        cfg = ClipsConfig()
        cfg.min_duration, cfg.max_duration = 20, 40
        cfg.head_skip, cfg.tail_skip = 300, 300
        ventanas = scoring.build_windows(transcripcion(duracion=200.0), cfg)
        self.assertTrue(ventanas)      # se ignoran los margenes en vez de fallar

    def test_las_ventanas_no_cruzan_de_una_zona_a_otra(self):
        cfg = ClipsConfig()
        cfg.min_duration, cfg.max_duration = 20, 60
        cfg.head_skip = cfg.tail_skip = 0
        zonas = [(0.0, 100.0), (400.0, 500.0)]
        ventanas = scoring.build_windows(transcripcion(), cfg, regions=zonas)
        for v in ventanas:
            dentro = any(a <= v.start and v.end <= b for a, b in zonas)
            self.assertTrue(dentro, f"{v.start}-{v.end} se sale de las zonas")


class Eleccion(unittest.TestCase):

    def _ventana(self, inicio, texto, nota):
        v = Window(start=inicio, end=inicio + 30, text=texto)
        v.llm_content = nota
        v.heat = nota
        return v

    def test_respeta_el_hueco_minimo_entre_clips(self):
        cfg_clips = ClipsConfig()
        cfg_clips.num_clips, cfg_clips.min_gap = 2, 60
        ventanas = [self._ventana(0, "uno dos tres", 1.0),
                    self._ventana(35, "cuatro cinco seis", 0.9),
                    self._ventana(400, "siete ocho nueve", 0.8)]
        elegidos = scoring.finalise(ventanas, ScoringConfig(), cfg_clips)
        self.assertEqual(len(elegidos), 2)
        self.assertGreater(elegidos[1].start - elegidos[0].end, 0)

    def test_descarta_el_segundo_clip_del_mismo_tema(self):
        cfg_clips = ClipsConfig()
        cfg_clips.num_clips, cfg_clips.min_gap = 3, 10
        cfg_scoring = ScoringConfig()
        cfg_scoring.max_similarity = 0.5

        mismo = "bitcoin bitcoin bitcoin comprar comprar precio precio subida"
        otro = "perros gatos veterinario vacunas paseo comida animales"
        ventanas = [self._ventana(0, mismo, 1.0),
                    self._ventana(200, mismo, 0.9),
                    self._ventana(400, otro, 0.8)]

        elegidos = scoring.finalise(ventanas, cfg_scoring, cfg_clips)
        self.assertEqual(len(elegidos), 2)
        self.assertIn("perros", elegidos[-1].text)

    def test_el_limite_manda_sobre_num_clips(self):
        cfg_clips = ClipsConfig()
        cfg_clips.num_clips, cfg_clips.min_gap = 2, 10
        ventanas = [self._ventana(i * 100, f"tema{i} palabra{i} distinto{i}", 1 - i / 10)
                    for i in range(6)]
        self.assertEqual(len(scoring.finalise(ventanas, ScoringConfig(), cfg_clips, limit=5)), 5)


class Configuracion(unittest.TestCase):

    def test_el_yaml_solo_pisa_lo_que_trae(self):
        with tempfile.TemporaryDirectory() as carpeta:
            path = Path(carpeta) / "c.yaml"
            path.write_text("clips:\n  num_clips: 7\n", encoding="utf-8")
            cfg = Config.load(path)
            self.assertEqual(cfg.clips.num_clips, 7)
            self.assertEqual(cfg.clips.min_duration, Config().clips.min_duration)

    def test_una_clave_inventada_falla_con_un_mensaje_claro(self):
        with tempfile.TemporaryDirectory() as carpeta:
            path = Path(carpeta) / "c.yaml"
            path.write_text("clips:\n  numero_de_clips: 7\n", encoding="utf-8")
            with self.assertRaises(ValueError) as caso:
                Config.load(path)
            self.assertIn("numero_de_clips", str(caso.exception))

    def test_set_convierte_al_tipo_del_ajuste(self):
        cfg = Config()
        pipeline.apply_setting(cfg, "subtitles.font_size=80")
        pipeline.apply_setting(cfg, "subtitles.vertical_position_ratio=0.42")
        pipeline.apply_setting(cfg, "llm.enabled=false")
        pipeline.apply_setting(cfg, "subtitles.censor_words=chorra, tonto")
        pipeline.apply_setting(cfg, "transcribe.language=null")

        self.assertEqual(cfg.subtitles.font_size, 80)
        self.assertIsInstance(cfg.subtitles.font_size, int)
        self.assertAlmostEqual(cfg.subtitles.vertical_position_ratio, 0.42)
        self.assertIs(cfg.llm.enabled, False)
        self.assertEqual(cfg.subtitles.censor_words, ["chorra", "tonto"])
        self.assertIsNone(cfg.transcribe.language)

    def test_set_rechaza_ajustes_que_no_existen(self):
        cfg = Config()
        for malo in ("subtitulos.tamano=80", "subtitles.tamano=80", "sin_igual"):
            with self.assertRaises(ValueError):
                pipeline.apply_setting(cfg, malo)


class PlanDeMontaje(unittest.TestCase):

    class Fuente:
        def __init__(self, ruta):
            self.path = Path(ruta)
            self.title = "El Programa - Invitado"
            self.duration = 3600.0
            self.info = {"webpage_url": "https://example.com/v"}

    def _clip(self, inicio=10.0):
        palabras = [Word(text="hola", start=inicio, end=inicio + 0.4),
                    Word(text="mundo.", start=inicio + 0.5, end=inicio + 0.9)]
        v = Window(start=inicio, end=inicio + 30, words=palabras, text="hola mundo.")
        v.score, v.reason, v.heat = 0.7, "engancha", 0.5
        return v

    def test_ida_y_vuelta_conserva_lo_que_importa(self):
        from viralshorts import plan as planning

        with tempfile.TemporaryDirectory() as carpeta:
            destino = Path(carpeta) / "plan.json"
            cfg = Config()
            cfg.subtitles.font_size = 77
            metas = {0: ClipMetadata("Titulo", "GANCHO", "Descripcion.", ["a", "b"])}

            planning.save(destino, cfg, self.Fuente("p.mp4"), self.Fuente("b.mp4"),
                          [self._clip(10), self._clip(200)], metas, selected=1)
            leido = planning.load(destino)

            self.assertEqual(len(leido.clips), 2)
            self.assertEqual(leido.selected, [0])
            self.assertEqual(leido.config["subtitles"]["font_size"], 77)
            self.assertEqual(leido.clips[0].meta.title, "Titulo")
            self.assertEqual(len(leido.clips[0].words), 2)

            ventana = leido.clips[0].to_window()
            self.assertAlmostEqual(ventana.start, 10.0)
            self.assertAlmostEqual(ventana.heat, 0.5)
            self.assertEqual(ventana.words[0].text, "hola")

    def test_la_interfaz_puede_marcar_y_retitular(self):
        from viralshorts import plan as planning

        with tempfile.TemporaryDirectory() as carpeta:
            destino = Path(carpeta) / "plan.json"
            planning.save(destino, Config(), self.Fuente("p.mp4"), self.Fuente("b.mp4"),
                          [self._clip(10), self._clip(200)], {}, selected=2)

            planning.update(destino, [
                {"n": 1, "seleccionado": False},
                {"n": 2, "publicacion": {"title": "Otro titulo",
                                         "hashtags": ["#uno", "dos"]}},
            ])
            leido = planning.load(destino)
            self.assertEqual(leido.selected, [1])
            self.assertEqual(leido.clips[1].meta.title, "Otro titulo")
            self.assertEqual(leido.clips[1].meta.hashtags, ["uno", "dos"])

            planning.apply_config(destino, {"subtitles.font_size": 120})
            self.assertEqual(planning.load(destino).config["subtitles"]["font_size"], 120)

    def test_un_plan_de_otra_version_no_se_usa_a_medias(self):
        from viralshorts import plan as planning

        with tempfile.TemporaryDirectory() as carpeta:
            destino = Path(carpeta) / "plan.json"
            destino.write_text(json.dumps({"version": 0, "clips": []}), encoding="utf-8")
            with self.assertRaises(ValueError):
                planning.load(destino)


class LlamadasAlModelo(unittest.TestCase):
    """Que se manden en lotes y que lo pagado no se pague dos veces."""

    class Falso(ClipJudge):
        def __init__(self, cfg, cache_dir):
            super().__init__(cfg, cache_dir)
            self.peticiones = []

        @property
        def available(self):
            return True

        def _call(self, system, user, schema):
            import re
            ids = [int(n) for n in re.findall(r'id="(\d+)"', user)]
            self.peticiones.append(ids)
            return {"clips": [{"id": i, "content_score": 8, "hook_score": 7,
                               "reason": "vale"} for i in ids]}

    def _ventanas(self, cuantas):
        return [Window(start=i * 100, end=i * 100 + 30, text=f"fragmento numero {i}")
                for i in range(cuantas)]

    def test_trocea_en_lotes(self):
        cfg = LLMConfig()
        cfg.batch_size, cfg.cache = 4, False
        juez = self.Falso(cfg, None)
        notas = juez.score_windows(self._ventanas(10))

        self.assertEqual(len(notas), 10)
        self.assertEqual([len(p) for p in juez.peticiones], [4, 4, 2])
        self.assertAlmostEqual(notas[0].content, 0.8)

    def test_la_segunda_vez_no_vuelve_a_preguntar(self):
        with tempfile.TemporaryDirectory() as carpeta:
            cfg = LLMConfig()
            cfg.batch_size = 4
            ventanas = self._ventanas(6)

            primero = self.Falso(cfg, Path(carpeta))
            primero.score_windows(ventanas)
            self.assertEqual(sum(len(p) for p in primero.peticiones), 6)

            segundo = self.Falso(cfg, Path(carpeta))
            notas = segundo.score_windows(ventanas)
            self.assertEqual(segundo.peticiones, [])
            self.assertEqual(len(notas), 6)

    def test_cambiar_de_modelo_invalida_lo_guardado(self):
        with tempfile.TemporaryDirectory() as carpeta:
            cfg = LLMConfig()
            ventanas = self._ventanas(3)
            self.Falso(cfg, Path(carpeta)).score_windows(ventanas)

            otro = LLMConfig()
            otro.model = "claude-sonnet-5"
            segundo = self.Falso(otro, Path(carpeta))
            segundo.score_windows(ventanas)
            self.assertEqual(sum(len(p) for p in segundo.peticiones), 3)

    def test_un_fallo_no_tira_la_ejecucion(self):
        class Rota(self.Falso):
            def _call(self, system, user, schema):
                raise RuntimeError("Error code: 529 overloaded")

        cfg = LLMConfig()
        cfg.cache, cfg.max_retries = False, 1
        self.assertEqual(Rota(cfg, None).score_windows(self._ventanas(3)), {})


class Exportacion(unittest.TestCase):

    def test_el_csv_recoge_los_textos_de_cada_short(self):
        with tempfile.TemporaryDirectory() as carpeta:
            salida = Path(carpeta)
            (salida / "short_1.mp4").write_bytes(b"x")
            (salida / "short_1.json").write_text(json.dumps({
                "video": "short_1.mp4",
                "fuente": {"titulo": "El Programa", "url": "https://x", "marca": "12:30"},
                "duracion_final": 38.4,
                "puntuacion": {"total": 0.81},
                "publicacion": {"title": "Un titulo", "hook": "UN GANCHO",
                                "description": "Una descripcion.",
                                "hashtags": ["podcast", "clip"]},
            }), encoding="utf-8")

            path = exports.write_csv(salida)
            self.assertIsNotNone(path)
            texto = path.read_text(encoding="utf-8-sig")
            self.assertIn("Un titulo", texto)
            self.assertIn("#podcast #clip", texto)
            self.assertIn("12:30", texto)

    def test_sin_shorts_no_escribe_nada(self):
        with tempfile.TemporaryDirectory() as carpeta:
            self.assertIsNone(exports.write_csv(Path(carpeta)))


class Fuentes(unittest.TestCase):

    def test_una_fuente_inventada_no_esta(self):
        if not fonts.installed_families():
            self.skipTest("este sistema no sabe listar sus fuentes")
        self.assertFalse(fonts.is_available("Fuente Que No Existe 12345"))

    def test_las_variantes_van_de_mas_a_menos(self):
        self.assertEqual(fonts._family_variants("Arial Black Bold"),
                         ["Arial Black Bold", "Arial Black", "Arial"])

    def test_una_carpeta_de_ttf_propios_vale(self):
        with tempfile.TemporaryDirectory() as carpeta:
            (Path(carpeta) / "MiFuenteRara.ttf").write_bytes(b"x")
            self.assertTrue(fonts.is_available("MiFuenteRara", carpeta))


if __name__ == "__main__":
    unittest.main()
