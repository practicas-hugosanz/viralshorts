"""Pruebas del servidor de la interfaz: seguridad, cola y linea de comandos.

El servidor se levanta de verdad en un puerto libre (no hay nada que simular
en un http.server) y el pipeline nunca se lanza: `Job._start` y `_spawn` se
sustituyen.
"""

import http.client
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import gui


class Servidor(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), gui.Handler)
        cls.server.daemon_threads = True
        cls.port = cls.server.server_address[1]
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def pedir(self, metodo, ruta, host=None, token=None, cuerpo=None, extra=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        headers = {"Host": host or f"127.0.0.1:{self.port}"}
        if token:
            headers["X-VS-Token"] = token
        headers.update(extra or {})
        conn.request(metodo, ruta, body=cuerpo, headers=headers)
        respuesta = conn.getresponse()
        datos = respuesta.read()
        conn.close()
        return respuesta, datos


class Seguridad(Servidor):
    def test_la_pagina_sin_token_no_se_sirve(self):
        respuesta, datos = self.pedir("GET", "/")
        self.assertEqual(respuesta.status, 403)
        self.assertNotIn(gui.TOKEN.encode(), datos)

    def test_la_pagina_con_token_en_la_url_se_sirve(self):
        respuesta, datos = self.pedir("GET", f"/?t={gui.TOKEN}")
        self.assertEqual(respuesta.status, 200)
        self.assertIn(gui.TOKEN.encode(), datos)

    def test_dns_rebinding_un_dominio_ajeno_no_obtiene_ni_la_pagina(self):
        # El dominio atacante resuelve a 127.0.0.1: llega aqui con su Host.
        respuesta, datos = self.pedir("GET", f"/?t={gui.TOKEN}",
                                      host=f"evil.example:{self.port}")
        self.assertEqual(respuesta.status, 403)
        self.assertNotIn(gui.TOKEN.encode(), datos)

    def test_los_datos_tambien_piden_token(self):
        for ruta in ("/api/state", "/api/existing", "/api/video?f=short_1.mp4"):
            with self.subTest(ruta=ruta):
                respuesta, _ = self.pedir("GET", ruta)
                self.assertEqual(respuesta.status, 403)

    def test_los_datos_con_token_en_cabecera(self):
        respuesta, _ = self.pedir("GET", "/api/existing", token=gui.TOKEN)
        self.assertEqual(respuesta.status, 200)

    def test_un_token_con_caracteres_raros_no_rompe_el_servidor(self):
        respuesta, _ = self.pedir("GET", "/api/existing?t=%C3%B1%C3%B1")
        self.assertEqual(respuesta.status, 403)

    def test_post_con_token_pero_host_ajeno(self):
        respuesta, _ = self.pedir("POST", "/api/stop", token=gui.TOKEN,
                                  host="evil.example", cuerpo=b"{}")
        self.assertEqual(respuesta.status, 403)

    def test_post_sin_token(self):
        respuesta, _ = self.pedir("POST", "/api/stop", cuerpo=b"{}")
        self.assertEqual(respuesta.status, 403)

    def test_hosts_permitidos(self):
        for host in ("127.0.0.1:8760", "localhost:8760", "LOCALHOST", "[::1]:8760"):
            self.assertTrue(gui._host_allowed(host), host)
        for host in ("", "evil.example", "127.0.0.1.evil.example:8760",
                     "localhost.evil.example"):
            self.assertFalse(gui._host_allowed(host), host)


class Videos(Servidor):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.carpeta = Path(self.tmp.name)
        (self.carpeta / "short_1.mp4").write_bytes(b"0123456789")
        (self.carpeta.parent / "secreto.mp4").write_bytes(b"no")
        self.addCleanup(setattr, gui.JOB, "output_dir", gui.JOB.output_dir)
        gui.JOB.output_dir = self.carpeta

    def video(self, nombre, **extra):
        return self.pedir("GET", f"/api/video?f={nombre}&t={gui.TOKEN}",
                          extra=extra)

    def test_sirve_el_video(self):
        respuesta, datos = self.video("short_1.mp4")
        self.assertEqual(respuesta.status, 200)
        self.assertEqual(datos, b"0123456789")

    def test_rango_para_saltar_por_la_barra_de_tiempo(self):
        respuesta, datos = self.video("short_1.mp4", Range="bytes=2-5")
        self.assertEqual(respuesta.status, 206)
        self.assertEqual(datos, b"2345")
        self.assertEqual(respuesta.getheader("Content-Range"), "bytes 2-5/10")

    def test_rango_fuera_del_fichero(self):
        respuesta, _ = self.video("short_1.mp4", Range="bytes=99-")
        self.assertEqual(respuesta.status, 416)

    def test_no_se_sale_de_la_carpeta(self):
        for nombre in ("../secreto.mp4", "..%5Csecreto.mp4", "%2e%2e/secreto.mp4",
                       "C:%5Cwindows%5Cx.mp4", "short_1.txt"):
            with self.subTest(nombre=nombre):
                respuesta, datos = self.video(nombre)
                self.assertEqual(respuesta.status, 400)
                self.assertNotEqual(datos, b"no")

    def test_fichero_que_no_existe(self):
        respuesta, _ = self.video("short_9.mp4")
        self.assertEqual(respuesta.status, 404)


class Cola(unittest.TestCase):
    def nuevo_job(self):
        job = gui.Job()
        job.started = []

        def arrancar(form):
            job.status = "running"
            job.started.append(form)

        patcher = mock.patch.object(job, "_start", side_effect=arrancar)
        patcher.start()
        self.addCleanup(patcher.stop)
        return job

    def test_ejecuciones_simultaneas_arrancan_solo_una(self):
        job = self.nuevo_job()
        puerta = threading.Barrier(16)

        def lanzar(n):
            puerta.wait()
            job.enqueue({"podcast": f"p{n}", "background": "b"})

        hilos = [threading.Thread(target=lanzar, args=(n,)) for n in range(16)]
        for hilo in hilos:
            hilo.start()
        for hilo in hilos:
            hilo.join()

        self.assertEqual(len(job.started), 1)
        self.assertEqual(len(job.pending), 15)

    def test_al_terminar_arranca_el_siguiente_de_la_cola(self):
        job = self.nuevo_job()
        job.status = "running"
        job.pending = [{"podcast": "siguiente", "background": "b"}]
        proceso = mock.Mock(stderr=iter([]))
        proceso.wait.return_value = 0

        with mock.patch.object(gui.time, "sleep"):
            job._pump(proceso)

        self.assertEqual([f["podcast"] for f in job.started], ["siguiente"])
        self.assertEqual(job.pending, [])

    def test_un_enqueue_durante_el_relevo_no_arranca_dos(self):
        job = self.nuevo_job()
        job.status = "running"
        job.pending = [{"podcast": "siguiente", "background": "b"}]
        proceso = mock.Mock(stderr=iter([]))
        proceso.wait.return_value = 0
        intruso = threading.Thread(
            target=job.enqueue, args=({"podcast": "intruso", "background": "b"},))

        def dormir(_):
            # Justo en el hueco entre "terminado" y "arranca el siguiente".
            intruso.start()
            intruso.join(0.2)

        with mock.patch.object(gui.time, "sleep", side_effect=dormir):
            job._pump(proceso)
        intruso.join()

        self.assertEqual([f["podcast"] for f in job.started], ["siguiente"])
        self.assertEqual([f["podcast"] for f in job.pending], ["intruso"])

    def test_detener_vacia_la_cola_y_mata_el_proceso(self):
        job = self.nuevo_job()
        job.status = "running"
        job.pending = [{"podcast": "x", "background": "b"}]
        job.process = mock.Mock()
        job.process.poll.return_value = None

        with mock.patch.object(gui, "_kill_tree") as matar:
            job.stop()

        matar.assert_called_once_with(job.process)
        self.assertEqual(job.status, "stopped")
        self.assertEqual(job.pending, [])

    def test_si_no_se_puede_lanzar_no_se_queda_en_running(self):
        job = gui.Job()
        with mock.patch.object(gui, "_spawn", side_effect=OSError("sin python")):
            job.enqueue({"podcast": "p", "background": "b"})
        self.assertEqual(job.status, "error")

    def test_el_evento_salida_cambia_la_carpeta_de_los_videos(self):
        job = gui.Job()
        job._handle_event('{"type": "salida", "ruta": "C:/x/podcast_2026"}')
        self.assertEqual(job.output_dir, Path("C:/x/podcast_2026"))


class CarpetaDeSalida(unittest.TestCase):
    def test_elige_la_ejecucion_mas_reciente_con_shorts(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            vieja, nueva, vacia = base / "a_1", base / "b_2", base / "c_3"
            for carpeta in (vieja, nueva, vacia):
                carpeta.mkdir()
            (vieja / "short_1.mp4").write_bytes(b"x")
            (nueva / "short_1.mp4").write_bytes(b"x")
            import os
            os.utime(vieja, (1_000, 1_000))
            os.utime(nueva, (2_000, 2_000))
            os.utime(vacia, (3_000, 3_000))
            self.assertEqual(gui._latest_run_dir(base), nueva)

    def test_sin_subcarpetas_se_queda_con_la_base(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(gui._latest_run_dir(Path(tmp)), Path(tmp))
            (Path(tmp) / "short_1.mp4").write_bytes(b"x")
            self.assertEqual(gui._latest_run_dir(Path(tmp)), Path(tmp))

    def test_carpeta_inexistente(self):
        self.assertEqual(gui._latest_run_dir(Path("no/existe")), Path("no/existe"))


class LineaDeComandos(unittest.TestCase):
    base = {"podcast": "https://youtu.be/abc", "background": "juego.mp4"}

    def comando(self, **extra):
        return gui._build_command({**self.base, **extra})

    def pares(self, args):
        return [args[i + 1] for i, a in enumerate(args) if a == "--set"]

    def test_lo_minimo(self):
        args = self.comando()
        self.assertEqual(args[args.index("--podcast") + 1], self.base["podcast"])
        self.assertEqual(args[args.index("--background") + 1], "juego.mp4")
        self.assertNotIn("--no-llm", args)

    def test_los_interruptores_apagados_pasan_su_bandera(self):
        args = self.comando(llm=False, reframe=False, censor=False, prescan=False)
        for bandera in ("--no-llm", "--no-reframe", "--no-censor", "--full-transcript"):
            self.assertIn(bandera, args)

    def test_los_numeros_llegan_como_numeros(self):
        args = self.comando(clips="5", min_duration="20", head_skip=0, beam="1")
        self.assertEqual(args[args.index("--clips") + 1], "5")
        self.assertEqual(args[args.index("--min-duration") + 1], "20.0")
        self.assertEqual(args[args.index("--head-skip") + 1], "0.0")

    def test_estilo_valido(self):
        args = self.comando(style={"font_size": "80", "color_active": "#ffcc00",
                                   "position": 1.7, "words": 3})
        pares = self.pares(args)
        self.assertIn("subtitles.font_size=80", pares)
        self.assertIn("subtitles.color_active=#FFCC00", pares)
        self.assertIn("subtitles.vertical_position_ratio=1.0", pares)
        self.assertIn("subtitles.max_words_per_group=3", pares)

    def test_estilo_invalido_se_ignora(self):
        args = self.comando(style={"color_active": "rojo", "font_size": "mucho",
                                   "ajuste_inventado": "x", "font_name": ""})
        self.assertEqual(self.pares(args), [])

    def test_el_navegador_no_elige_el_ajuste(self):
        # Solo cuentan las claves de la lista cerrada, aunque traigan puntos.
        args = self.comando(style={"llm.model": "otro", "output.dir": "/"})
        self.assertEqual(self.pares(args), [])

    def test_plan_en_dos_tiempos(self):
        args = self.comando(plan_out="work/plans/p.json", propose=9)
        self.assertEqual(args[args.index("--plan-out") + 1], "work/plans/p.json")
        self.assertEqual(args[args.index("--propose") + 1], "9")
        self.assertNotIn("--dry-run", args)

    def test_montar_desde_plan(self):
        args = gui._plan_command({"from_plan": "p.json", "only": [1, "4", 7.0],
                                  "output": "salida"})
        self.assertEqual(args[args.index("--only") + 1], "1,4,7")
        self.assertEqual(args[args.index("--output") + 1], "salida")


if __name__ == "__main__":
    unittest.main()
