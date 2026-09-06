"""Pruebas del apano cuando la tarjeta grafica no se puede usar.

El caso real: un equipo con GPU NVIDIA pero sin las librerias CUDA. ctranslate2
carga cuBLAS de forma perezosa, asi que Whisper se carga sin una queja y el
fallo ('cublas64_12.dll is not found or cannot be loaded') sale despues, ya
empezada la transcripcion. Antes ahi moria el pipeline entero.

Como en el resto de pruebas, no se toca ni ffmpeg ni un modelo de verdad: se
imita el motor de faster-whisper, que es lo unico que hace falta.
"""

import tempfile
import unittest
from pathlib import Path

from viralshorts import transcribe as T


class Info:
    language = "es"


class Palabra:
    def __init__(self, inicio, fin, texto):
        self.start, self.end, self.word, self.probability = inicio, fin, texto, 0.9


class Segmento:
    def __init__(self, inicio, fin, texto):
        self.start, self.end, self.text = inicio, fin, texto
        self.words = [Palabra(inicio, fin, texto)]


class MotorFalso:
    """Imita a faster-whisper: devuelve un generador perezoso."""

    def __init__(self, device, falla_en_gpu=True):
        self.device = device
        self.falla_en_gpu = falla_en_gpu

    def transcribe(self, audio, **kwargs):
        def segmentos():
            if self.device == "cuda" and self.falla_en_gpu:
                raise RuntimeError(
                    "Library cublas64_12.dll is not found or cannot be loaded")
            yield Segmento(0.0, 1.0, "hola")
            yield Segmento(1.0, 2.0, "que tal")
        return segmentos(), Info()


class Cfg:
    model_size = "medium"
    language = "es"
    beam_size = 5
    vad_filter = True
    condition_on_previous_text = False
    batched = False
    batch_size = 8
    cpu_threads = 2
    device = "auto"
    compute_type = "auto"


class TranscriptorFalso(T.Transcriber):
    """Cargar el modelo nunca falla, igual que en ctranslate2 de verdad."""

    def __init__(self, *args, falla_en_gpu=True, **kwargs):
        super().__init__(*args, **kwargs)
        self.falla_en_gpu = falla_en_gpu
        self.cargas = []

    def _load_model(self):
        if self._model is not None:
            return self._model
        device, _ = self._resolve_device()
        self.cargas.append(device)
        self._device = device
        self._model = MotorFalso(device, self.falla_en_gpu)
        return self._model


class GPUQueFalla(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cache = Path(self.tmp.name)
        self.cuda_original = T._cuda_available
        T._cuda_available = lambda: True     # el equipo dice tener GPU
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(setattr, T, "_cuda_available", self.cuda_original)

    def test_cae_a_cpu_cuando_cublas_falla_a_mitad(self):
        t = TranscriptorFalso(Cfg(), cache_dir=self.cache)
        crudos, _ = t._run("audio")
        segmentos = list(crudos)

        self.assertEqual(t.cargas, ["cuda", "cpu"])
        self.assertEqual(len(segmentos), 2, "no se pierde ningun segmento")
        self.assertEqual(t._device, "cpu")

    def test_no_reintenta_la_gpu_una_vez_ha_fallado(self):
        t = TranscriptorFalso(Cfg(), cache_dir=self.cache)
        list(t._run("audio")[0])
        list(t._run("otro audio")[0])
        # La segunda llamada no vuelve a probar la GPU: seria un minuto tirado
        # por cada zona del podcast.
        self.assertEqual(t.cargas, ["cuda", "cpu"])

    def test_la_gpu_que_funciona_se_usa(self):
        t = TranscriptorFalso(Cfg(), cache_dir=self.cache, falla_en_gpu=False)
        segmentos = list(t._run("audio")[0])

        self.assertEqual(t.cargas, ["cuda"])
        self.assertEqual(len(segmentos), 2)
        self.assertEqual(t._device, "cuda")

    def test_un_error_que_no_es_de_gpu_no_se_traga(self):
        class MotorRoto(MotorFalso):
            def transcribe(self, audio, **kwargs):
                def segmentos():
                    raise ValueError("el audio esta corrupto")
                    yield
                return segmentos(), Info()

        t = TranscriptorFalso(Cfg(), cache_dir=self.cache)
        t._load_model()
        t._model = MotorRoto("cuda")

        # Reintentar en CPU no arreglaria esto: tiene que salir a la superficie.
        with self.assertRaises(ValueError):
            list(t._run("audio")[0])


class ReconocerElFallo(unittest.TestCase):
    def test_mensajes_de_cuda(self):
        for texto in (
            "Library cublas64_12.dll is not found or cannot be loaded",
            "Unable to load libcudnn_ops.so.9",
            "CUDA failed with error out of memory",
            "no kernel image is available for execution on the device",
        ):
            with self.subTest(texto=texto):
                self.assertTrue(T._fallo_de_gpu(RuntimeError(texto)))

    def test_otros_errores_no_cuentan(self):
        for texto in ("el fichero no existe", "audio vacio", "timeout"):
            with self.subTest(texto=texto):
                self.assertFalse(T._fallo_de_gpu(ValueError(texto)))


if __name__ == "__main__":
    unittest.main()
