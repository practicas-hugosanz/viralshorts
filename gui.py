#!/usr/bin/env python3
"""Interfaz visual de viralshorts.

Levanta un servidor local y abre el navegador. Se eligio navegador en vez de
una ventana de escritorio por una razon concreta: asi puedes VER los shorts
generados dentro de la propia interfaz, que es justo lo que quieres comprobar.
El dialogo para elegir ficheros si es nativo (tkinter), porque el navegador no
puede darle a la pagina la ruta real de un fichero local.

    python gui.py

El trabajo va por una cola: puedes dejar preparados varios podcast y se montan
uno detras de otro. Y hay dos formas de lanzarlo:

  - directa:  buscar los momentos y montar los shorts del tiron.
  - revisada: buscar los momentos, verlos, quitar los que no valgan, corregir
              titulos y ganchos, y montar solo los que has marcado.

No necesita ninguna dependencia extra: todo es biblioteca estandar.
"""

from __future__ import annotations

import json
import mimetypes
import os
import queue
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Dict, List, Optional
from urllib.parse import parse_qs, urlparse

ROOT = Path(__file__).parent.resolve()
WEB_DIR = ROOT / "web"
DEFAULT_PORT = 8760
TOKEN = secrets.token_urlsafe(16)

# Prefijo que el pipeline usa para las lineas de progreso legibles por maquina.
_EVENT_PREFIX = "@@VS "

# Ajustes que la interfaz puede tocar (estilo de los subtitulos y velocidad).
# La lista es cerrada a proposito: el navegador no decide que ajuste del
# pipeline se cambia, solo elige entre estos.
_STYLE_FIELDS = {
    "font_name":    ("subtitles.font_name", "text"),
    "font_size":    ("subtitles.font_size", "int"),
    "color_active": ("subtitles.color_active", "color"),
    "position":     ("subtitles.vertical_position_ratio", "ratio"),
    "words":        ("subtitles.max_words_per_group", "int"),
    "hook_size":    ("hook.font_size", "int"),
    "max_height":   ("download.max_height", "int"),
    "workers":      ("output.render_workers", "int"),
    "encoder":      ("output.hardware_encoder", "text"),
}

# Nombres con los que el navegador puede llegar legitimamente a este servidor.
# Cualquier otro (un dominio atacante que resuelva a 127.0.0.1: DNS rebinding)
# se rechaza antes de mirar nada mas.
_ALLOWED_HOSTS = {"127.0.0.1", "localhost", "[::1]"}


def _host_allowed(header: str) -> bool:
    host = (header or "").strip().lower()
    if host.startswith("["):
        host = host.split("]", 1)[0] + "]"
    elif ":" in host:
        host = host.rsplit(":", 1)[0]
    return host in _ALLOWED_HOSTS


def _has_shorts(folder: Path) -> bool:
    return folder.is_dir() and any(folder.glob("short_*.mp4"))


def _latest_run_dir(base: Path) -> Path:
    """Carpeta de la ultima ejecucion con shorts (o `base` si no hay ninguna)."""
    if _has_shorts(base) or not base.is_dir():
        return base
    runs = [d for d in base.iterdir() if _has_shorts(d)]
    return max(runs, key=lambda d: d.stat().st_mtime) if runs else base


# ============================================================================
# Estado del trabajo en curso
# ============================================================================

class Job:
    """La ejecucion en marcha, mas la cola de las que esperan."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        # Estado de la ejecucion (status, process, pending). Distinto de
        # `_lock`, que solo protege la lista de suscriptores: aqui hay que
        # decidir "arranco o encolo" y "pasamos al siguiente" sin que se cuele
        # otra peticion en medio.
        self._run_lock = threading.RLock()
        self._subscribers: List[queue.Queue] = []
        self.process: Optional[subprocess.Popen] = None
        self.status = "idle"          # idle | running | done | error | stopped
        self.stage = ""
        self.log: List[str] = []
        self.clips: List[dict] = []
        self.results: List[dict] = []
        self.notices: List[dict] = []
        self.output_dir = _latest_run_dir(ROOT / "output")
        self.plan_path: Optional[Path] = None
        self.plan_clips: List[dict] = []
        self.pending: List[dict] = []   # cola de formularios por lanzar
        self.current: Optional[dict] = None

    # -- difusion de eventos --------------------------------------------
    def subscribe(self) -> queue.Queue:
        q: queue.Queue = queue.Queue(maxsize=1000)
        with self._lock:
            self._subscribers.append(q)
        return q

    def unsubscribe(self, q: queue.Queue) -> None:
        with self._lock:
            if q in self._subscribers:
                self._subscribers.remove(q)

    def publish(self, event: dict) -> None:
        with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except queue.Full:
                pass          # un cliente lento no debe frenar el pipeline

    def snapshot(self) -> dict:
        return {
            "status": self.status,
            "stage": self.stage,
            "log": self.log[-400:],
            "clips": self.clips,
            "results": self.results,
            "notices": self.notices,
            "output_dir": str(self.output_dir),
            "plan": self.plan_summary(),
            "queue": self.queue_summary(),
            "env": _environment(),
        }

    def plan_summary(self) -> Optional[dict]:
        if not self.plan_path:
            return None
        return {"ruta": str(self.plan_path), "clips": self.plan_clips}

    def queue_summary(self) -> List[dict]:
        return [{"n": i + 1, "podcast": item.get("podcast", ""),
                 "clips": item.get("clips", 0),
                 "revisar": bool(item.get("review"))}
                for i, item in enumerate(self.pending)]

    # -- ciclo de vida ---------------------------------------------------
    def enqueue(self, form: dict) -> dict:
        """Lanza el trabajo, o lo deja en la cola si hay otro en marcha."""
        with self._run_lock:
            if self.status == "running":
                self.pending.append(form)
                self.publish({"type": "queue", "queue": self.queue_summary()})
                return {"ok": True, "encolado": True, "posicion": len(self.pending)}

            self._start(form)
        return {"ok": True, "encolado": False}

    def _start(self, form: dict) -> None:
        self.current = form
        self.status = "running"
        self.stage = "preparando"
        self.log = []
        self.clips = []
        self.results = []
        self.notices = []
        self.output_dir = Path(form.get("output") or "output")
        if not self.output_dir.is_absolute():
            self.output_dir = ROOT / self.output_dir

        if form.get("from_plan"):
            args = _plan_command(form)
        else:
            # Un plan nuevo invalida el anterior: los clips que se ven en
            # pantalla tienen que ser los de esta ejecucion.
            self.plan_path = Path(form["plan_out"]) if form.get("plan_out") else None
            self.plan_clips = []
            args = _build_command(form)

        self.publish({"type": "job", "status": "running",
                      "queue": self.queue_summary()})
        self._append_log(f"$ {' '.join(args[1:])}")

        env = dict(os.environ)
        env["VIRALSHORTS_PROGRESS"] = "json"
        env["PYTHONIOENCODING"] = "utf-8"
        env["PYTHONUNBUFFERED"] = "1"

        try:
            self.process = _spawn(args, env)
        except OSError as exc:
            # Sin esto el estado se quedaba en "running" para siempre y la
            # cola no volvia a arrancar nada.
            self.status = "error"
            self._append_log(f"No se pudo lanzar el pipeline: {exc}")
            self.publish({"type": "job", "status": "error", "code": None,
                          "queue": self.queue_summary()})
            return
        threading.Thread(target=self._pump, args=(self.process,),
                         daemon=True).start()

    def stop(self, clear_queue: bool = True) -> None:
        """Para lo que haya en marcha, con todo lo que haya lanzado."""
        with self._run_lock:
            if clear_queue and self.pending:
                self.pending.clear()
                self.publish({"type": "queue", "queue": []})
            proc = self.process
            running = proc is not None and proc.poll() is None
            if running:
                self.status = "stopped"
        if running:
            _kill_tree(proc)
            self._append_log("-- detenido por el usuario --")

    def _pump(self, proc: subprocess.Popen) -> None:
        """Lee stderr del pipeline y lo reparte a los navegadores conectados."""
        assert proc.stderr is not None
        for raw in proc.stderr:
            line = raw.rstrip("\n")
            if line.startswith(_EVENT_PREFIX):
                self._handle_event(line[len(_EVENT_PREFIX):])
            elif line.strip():
                self._append_log(line)

        code = proc.wait()
        # Todo el relevo bajo el cerrojo: si entre "terminado" y "arranca el
        # siguiente" se colara un enqueue, veria el estado libre y lanzaria su
        # trabajo a la vez que el que sale de la cola.
        with self._run_lock:
            if self.status != "stopped":
                self.status = "done" if code == 0 else "error"
            self.publish({"type": "job", "status": self.status, "code": code,
                          "queue": self.queue_summary()})

            # Siguiente de la cola. Si el usuario ha pulsado Detener, la cola
            # ya esta vacia y aqui no arranca nada.
            if self.pending:
                time.sleep(0.4)           # que el cerrojo de work/ se suelte
                self._start(self.pending.pop(0))

    def _handle_event(self, payload: str) -> None:
        try:
            event = json.loads(payload)
        except json.JSONDecodeError:
            return

        kind = event.get("type")
        if kind == "stage":
            self.stage = event.get("label") or event.get("stage", "")
        elif kind == "clips":
            self.clips = event.get("clips", [])
        elif kind == "clip_done":
            self.results.append(event)
        elif kind == "aviso":
            self.notices.append(event)
        elif kind == "salida":
            # Carpeta de esta ejecucion: de ahi salen los videos que se ven.
            self.output_dir = Path(event.get("ruta", self.output_dir))
        elif kind == "plan":
            self.plan_path = Path(event.get("ruta", ""))
            self.plan_clips = event.get("clips", [])
        self.publish(event)

    def _append_log(self, line: str) -> None:
        self.log.append(line)
        if len(self.log) > 2000:
            del self.log[:500]
        self.publish({"type": "log", "line": line})


JOB = Job()


# ============================================================================
# Lanzar y matar procesos
# ============================================================================

def _spawn(args: List[str], env: dict) -> subprocess.Popen:
    """Arranca el pipeline en su propio grupo de procesos.

    Hace falta para poder pararlo entero: el pipeline lanza ffmpeg, y matar
    solo al Python deja a ffmpeg codificando de fondo, comiendose la maquina y
    escribiendo en la carpeta de trabajo que la siguiente ejecucion va a usar.
    """
    extra = {}
    if sys.platform == "win32":
        extra["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        extra["start_new_session"] = True

    return subprocess.Popen(
        args, cwd=str(ROOT), env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        text=True, encoding="utf-8", errors="replace", bufsize=1,
        **extra,
    )


def _kill_tree(proc: subprocess.Popen) -> None:
    """Mata el proceso y todos sus hijos (ffmpeg incluido)."""
    if proc.poll() is not None:
        return
    try:
        if sys.platform == "win32":
            # /T = arbol completo. Es la unica forma fiable en Windows: no hay
            # grupos de procesos que se puedan senalar como en Unix.
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                           capture_output=True, timeout=20)
        else:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (OSError, subprocess.SubprocessError):
        try:
            proc.kill()
        except OSError:
            pass


# Cache en memoria: el navegador pide el titulo cada vez que se toca la
# casilla, y no tiene sentido preguntarle a YouTube dos veces por lo mismo.
_TITLE_CACHE: Dict[str, dict] = {}


def _probe_title(url: str) -> dict:
    """Titulo y duracion de una URL, SIN descargar el video.

    Sirve para que la interfaz confirme que lo pegado es lo que se cree, antes
    de gastar veinte minutos en el video equivocado.
    """
    url = (url or "").strip()
    if not url:
        return {"ok": False, "error": "sin url"}
    if url in _TITLE_CACHE:
        return _TITLE_CACHE[url]

    local = Path(url)
    if local.is_file():
        answer = {"ok": True, "title": local.stem, "source": "archivo"}
        _TITLE_CACHE[url] = answer
        return answer

    try:
        import yt_dlp
    except ImportError:
        return {"ok": False, "error": "falta yt-dlp"}

    try:
        options = {"quiet": True, "no_warnings": True, "noplaylist": True,
                   "skip_download": True, "extract_flat": "in_playlist"}
        with yt_dlp.YoutubeDL(options) as ydl:
            info = ydl.extract_info(url, download=False)
        if info.get("_type") == "playlist":
            entries = [e for e in (info.get("entries") or []) if e]
            info = entries[0] if entries else {}
        answer = {
            "ok": True,
            "title": info.get("title") or "",
            "uploader": info.get("uploader") or info.get("channel") or "",
            "duration": info.get("duration") or 0,
            "source": "youtube",
        }
    except Exception as exc:
        # No es un error del que haya que avisar: la interfaz simplemente
        # se queda sin titulo y sigue mostrando el enlace.
        answer = {"ok": False, "error": str(exc)[:200]}

    _TITLE_CACHE[url] = answer
    return answer


def _configured_font() -> str:
    """Fuente de los subtitulos segun config.yaml (o la de serie)."""
    default = "Arial Black"
    path = ROOT / "config.yaml"
    if not path.is_file():
        return default
    try:
        import yaml
        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        return (data.get("subtitles") or {}).get("font_name") or default
    except Exception:
        return default


def _environment() -> dict:
    """Lo que la interfaz necesita para avisar ANTES de fallar a mitad.

    Enterarse de que falta ffmpeg, la clave de Claude o la fuente de los
    subtitulos cuando llevas veinte minutos transcribiendo es la peor forma
    posible de enterarse.
    """
    font = _configured_font()
    try:
        from viralshorts import fonts
        font_ok = fonts.is_available(font)
        alternative = "" if font_ok else fonts.suggest_alternative(font)
    except Exception:
        font_ok, alternative = True, ""

    return {
        "ffmpeg": shutil.which("ffmpeg") is not None,
        "has_api_key": bool(
            os.environ.get("ANTHROPIC_API_KEY")
            or os.environ.get("ANTHROPIC_AUTH_TOKEN")
        ),
        "font": font,
        "font_ok": font_ok,
        "font_alternative": alternative,
    }


# ============================================================================
# Construccion de la linea de comandos
# ============================================================================

def _style_pairs(form: dict) -> List[tuple]:
    """(ajuste, valor) del estilo de subtitulos que venga del formulario."""
    style = form.get("style") or {}
    pairs = []
    for key, (setting, kind) in _STYLE_FIELDS.items():
        if key not in style or style[key] in (None, ""):
            continue
        raw = style[key]
        try:
            if kind == "int":
                value = int(float(raw))
            elif kind == "ratio":
                value = round(min(max(float(raw), 0.0), 1.0), 3)
            elif kind == "color":
                text = str(raw).strip()
                if not re.fullmatch(r"#[0-9a-fA-F]{6}", text):
                    continue
                value = text.upper()
            else:
                value = str(raw).strip()[:60]
        except (TypeError, ValueError):
            continue
        pairs.append((setting, value))
    return pairs


def _build_command(form: dict) -> List[str]:
    args = [sys.executable, "-u", str(ROOT / "pipeline.py"),
            "--podcast", form["podcast"],
            "--background", form["background"]]

    if form.get("config"):
        args += ["--config", form["config"]]
    if form.get("clips"):
        args += ["--clips", str(int(form["clips"]))]
    if form.get("min_duration"):
        args += ["--min-duration", str(float(form["min_duration"]))]
    if form.get("max_duration"):
        args += ["--max-duration", str(float(form["max_duration"]))]
    if form.get("head_skip") not in (None, ""):
        args += ["--head-skip", str(float(form["head_skip"]))]
    if form.get("tail_skip") not in (None, ""):
        args += ["--tail-skip", str(float(form["tail_skip"]))]
    if form.get("output"):
        args += ["--output", form["output"]]
    if form.get("model"):
        args += ["--model", form["model"]]
    if form.get("language"):
        args += ["--language", form["language"]]
    if form.get("beam"):
        args += ["--beam", str(int(form["beam"]))]
    if form.get("regions"):
        args += ["--regions", str(int(form["regions"]))]

    for flag, key in (("--no-llm", "llm"), ("--no-reframe", "reframe"),
                      ("--no-zoom", "zoom"), ("--no-jumpcuts", "jumpcuts"),
                      ("--full-transcript", "prescan"),
                      ("--no-title-card", "title_card"),
                      ("--no-context", "context"),
                      ("--no-censor", "censor")):
        if not form.get(key, True):
            args.append(flag)

    for setting, value in _style_pairs(form):
        args += ["--set", f"{setting}={value}"]

    if form.get("plan_out"):
        args += ["--plan-out", str(form["plan_out"])]
        if form.get("propose"):
            args += ["--propose", str(int(form["propose"]))]
    elif form.get("dry_run"):
        args.append("--dry-run")

    return args


def _plan_command(form: dict) -> List[str]:
    """Montar los clips marcados de un plan ya revisado."""
    args = [sys.executable, "-u", str(ROOT / "pipeline.py"),
            "--from-plan", str(form["from_plan"])]
    if form.get("only"):
        args += ["--only", ",".join(str(int(n)) for n in form["only"])]
    if form.get("output"):
        args += ["--output", form["output"]]
    return args


def _new_plan_path() -> Path:
    folder = ROOT / "work" / "plans"
    folder.mkdir(parents=True, exist_ok=True)
    return folder / f"plan_{time.strftime('%Y%m%d_%H%M%S')}.json"


# ============================================================================
# Dialogo nativo de ficheros (tkinter, siempre en el hilo principal)
# ============================================================================

class FilePicker:
    """El navegador no puede darnos rutas locales; tkinter si.

    Tk exige vivir en el hilo principal, asi que el servidor encola peticiones
    aqui y este objeto las atiende desde el bucle principal.
    """

    def __init__(self) -> None:
        self.requests: queue.Queue = queue.Queue()
        self.available = False
        self._root = None

    def setup(self) -> bool:
        try:
            import tkinter as tk
        except ImportError:
            return False
        try:
            self._root = tk.Tk()
            self._root.withdraw()
            self.available = True
        except Exception:
            return False
        return True

    def ask(self, kind: str, timeout: float = 300.0) -> Optional[str]:
        """Llamado desde el hilo del servidor; espera la respuesta del dialogo."""
        if not self.available:
            return None
        answer: queue.Queue = queue.Queue(maxsize=1)
        self.requests.put((kind, answer))
        try:
            return answer.get(timeout=timeout)
        except queue.Empty:
            return None

    def pump(self) -> None:
        """Llamado periodicamente desde el bucle de Tk."""
        from tkinter import filedialog

        while True:
            try:
                kind, answer = self.requests.get_nowait()
            except queue.Empty:
                break

            try:
                self._root.deiconify()
                self._root.withdraw()
                if kind == "folder":
                    path = filedialog.askdirectory(title="Elige la carpeta de salida")
                else:
                    path = filedialog.askopenfilename(
                        title="Elige un video",
                        filetypes=[("Videos", "*.mp4 *.mkv *.webm *.mov *.m4v *.avi"),
                                   ("Todos", "*.*")],
                    )
            except Exception:
                path = ""
            answer.put(path or "")


PICKER = FilePicker()


# ============================================================================
# Servidor HTTP
# ============================================================================

class Handler(BaseHTTPRequestHandler):
    server_version = "viralshorts"

    def log_message(self, *args) -> None:      # silenciamos el log de acceso
        pass

    # -- utilidades -----------------------------------------------------
    def _authorised(self, params: Dict[str, List[str]]) -> bool:
        # El token evita que una pagina web cualquiera abierta en el navegador
        # pueda lanzar procesos contra este servidor local. Se compara en bytes
        # porque compare_digest revienta con texto no ASCII.
        expected = TOKEN.encode("utf-8")
        for given in (self.headers.get("X-VS-Token", ""),
                      (params.get("t") or [""])[0]):
            if secrets.compare_digest(given.encode("utf-8", "replace"), expected):
                return True
        return False

    def _allowed(self, params: Dict[str, List[str]]) -> bool:
        """Host de este equipo Y token, para TODO: paginas, datos y videos.

        Con solo el token en las peticiones POST, una web atacante que
        resolviera su dominio a 127.0.0.1 (DNS rebinding) podia pedir `/`,
        leer el token incrustado en la pagina y despues lanzar procesos.
        """
        if not _host_allowed(self.headers.get("Host", "")):
            self.send_error(403, "host no permitido")
            return False
        if not self._authorised(params):
            self.send_error(403, "falta el token: abre la direccion completa "
                                 "que sale en la consola")
            return False
        return True

    def _send_json(self, payload, status: int = 200) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _read_json(self) -> dict:
        length = int(self.headers.get("Content-Length", 0) or 0)
        if not length:
            return {}
        return json.loads(self.rfile.read(length).decode("utf-8"))

    # -- GET ------------------------------------------------------------
    def do_GET(self) -> None:
        url = urlparse(self.path)
        params = parse_qs(url.query)
        if not self._allowed(params):
            return

        if url.path == "/":
            self._serve_index()
        elif url.path == "/api/state":
            self._send_json(JOB.snapshot())
        elif url.path == "/api/events":
            self._serve_events()
        elif url.path == "/api/video":
            self._serve_file((params.get("f") or [""])[0])
        elif url.path == "/api/existing":
            self._send_json({"results": _scan_output(JOB.output_dir)})
        elif url.path == "/api/reveal":
            self._reveal()
        else:
            self.send_error(404)

    def _serve_index(self) -> None:
        page = (WEB_DIR / "index.html").read_text(encoding="utf-8")
        body = page.replace("__TOKEN__", TOKEN).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _serve_events(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "keep-alive")
        self.end_headers()

        q = JOB.subscribe()
        try:
            self._write_event({"type": "hello", **JOB.snapshot()})
            while True:
                try:
                    event = q.get(timeout=15)
                except queue.Empty:
                    self.wfile.write(b": ping\n\n")     # mantiene viva la conexion
                    self.wfile.flush()
                    continue
                self._write_event(event)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            JOB.unsubscribe(q)

    def _write_event(self, event: dict) -> None:
        data = json.dumps(event, ensure_ascii=False)
        self.wfile.write(f"data: {data}\n\n".encode("utf-8"))
        self.wfile.flush()

    def _serve_file(self, name: str) -> None:
        """Sirve un short (o su portada) de la carpeta de salida.

        Con soporte de Range: sin el, el navegador no puede saltar por la barra
        de tiempo del video.
        """
        if not name or not re.fullmatch(r"[\w.\- ]+\.(mp4|mkv|webm|jpg)", name):
            self.send_error(400, "nombre invalido")
            return

        path = (JOB.output_dir / name).resolve()
        try:
            path.relative_to(JOB.output_dir.resolve())
        except ValueError:
            self.send_error(403)
            return
        if not path.is_file():
            self.send_error(404)
            return

        size = path.stat().st_size
        mime = mimetypes.guess_type(str(path))[0] or "video/mp4"
        start, end = 0, size - 1
        status = 200

        header = self.headers.get("Range", "")
        match = re.match(r"bytes=(\d*)-(\d*)", header)
        if match:
            if match.group(1):
                start = int(match.group(1))
            if match.group(2):
                end = min(int(match.group(2)), size - 1)
            if start >= size:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{size}")
                self.end_headers()
                return
            status = 206

        length = end - start + 1
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()

        with path.open("rb") as handle:
            handle.seek(start)
            remaining = length
            while remaining > 0:
                chunk = handle.read(min(256 * 1024, remaining))
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                except (BrokenPipeError, ConnectionResetError):
                    return
                remaining -= len(chunk)

    def _reveal(self) -> None:
        folder = JOB.output_dir
        folder.mkdir(parents=True, exist_ok=True)
        try:
            if sys.platform == "win32":
                os.startfile(str(folder))                        # noqa: S606
            elif sys.platform == "darwin":
                subprocess.Popen(["open", str(folder)])
            else:
                subprocess.Popen(["xdg-open", str(folder)])
        except Exception as exc:
            self._send_json({"ok": False, "error": str(exc)})
            return
        self._send_json({"ok": True})

    # -- POST -----------------------------------------------------------
    def do_POST(self) -> None:
        url = urlparse(self.path)
        if not self._allowed(parse_qs(url.query)):
            return

        try:
            if url.path == "/api/run":
                self._run(self._read_json())

            elif url.path == "/api/stop":
                JOB.stop()
                self._send_json({"ok": True})

            elif url.path == "/api/queue/clear":
                with JOB._run_lock:
                    JOB.pending.clear()
                JOB.publish({"type": "queue", "queue": []})
                self._send_json({"ok": True})

            elif url.path == "/api/plan":
                self._send_json(_plan_state())

            elif url.path == "/api/plan/save":
                self._save_plan(self._read_json())

            elif url.path == "/api/plan/render":
                self._render_plan(self._read_json())

            elif url.path == "/api/title":
                self._send_json(_probe_title(self._read_json().get("url", "")))

            elif url.path == "/api/browse":
                kind = self._read_json().get("kind", "file")
                if not PICKER.available:
                    self._send_json({"ok": False,
                                     "error": "tkinter no esta disponible; "
                                              "pega la ruta a mano."}, 501)
                    return
                self._send_json({"ok": True, "path": PICKER.ask(kind) or ""})

            else:
                self.send_error(404)

        except Exception as exc:
            self._send_json({"ok": False, "error": str(exc)}, 500)

    # -- acciones -------------------------------------------------------
    def _run(self, form: dict) -> None:
        missing = [k for k in ("podcast", "background") if not form.get(k)]
        if missing:
            self._send_json({"ok": False,
                             "error": f"Falta rellenar: {', '.join(missing)}"}, 400)
            return

        if form.get("review"):
            # Revisar antes de montar: se guarda un plan y no se renderiza nada.
            form["plan_out"] = str(_new_plan_path())
            form.setdefault("propose", 0)

        self._send_json(JOB.enqueue(form))

    def _save_plan(self, body: dict) -> None:
        path = Path(body.get("ruta") or (JOB.plan_path or ""))
        if not path.is_file():
            self._send_json({"ok": False, "error": "no hay ningun plan guardado"}, 400)
            return

        from viralshorts import plan as planning

        changes = body.get("clips") or []
        if changes:
            planning.update(path, changes)

        overrides = {setting: value
                     for setting, value in _style_pairs({"style": body.get("style")})}
        if overrides:
            planning.apply_config(path, overrides)

        JOB.plan_path = path
        JOB.plan_clips = _plan_clips(path)
        JOB.publish({"type": "plan", "ruta": str(path), "clips": JOB.plan_clips})
        self._send_json({"ok": True, "clips": JOB.plan_clips})

    def _render_plan(self, body: dict) -> None:
        path = Path(body.get("ruta") or (JOB.plan_path or ""))
        if not path.is_file():
            self._send_json({"ok": False, "error": "no hay ningun plan guardado"}, 400)
            return

        only = [int(n) for n in (body.get("only") or [])]
        if not only:
            self._send_json({"ok": False,
                             "error": "no has marcado ningun momento"}, 400)
            return

        self._send_json(JOB.enqueue({
            "from_plan": str(path),
            "only": only,
            "output": body.get("output") or "",
            "podcast": body.get("podcast") or path.name,
            "clips": len(only),
        }))


def _plan_state() -> dict:
    if not JOB.plan_path or not Path(JOB.plan_path).is_file():
        return {"ok": True, "plan": None}
    return {"ok": True, "plan": {"ruta": str(JOB.plan_path),
                                 "clips": _plan_clips(JOB.plan_path)}}


def _plan_clips(path: Path) -> List[dict]:
    """Los candidatos de un plan, tal y como los pinta la interfaz."""
    try:
        from viralshorts import plan as planning
        saved = planning.load(Path(path))
    except Exception:
        return []

    clips = []
    for index, clip in enumerate(saved.clips, start=1):
        meta = clip.meta
        clips.append({
            "n": index,
            "inicio": round(clip.start, 2),
            "fin": round(clip.end, 2),
            "marca": _hms(clip.start),
            "duracion": round(clip.duration, 1),
            "puntuacion": round(clip.score, 3),
            "motivo": clip.reason,
            "texto": clip.text,
            "seleccionado": clip.selected,
            "titulo": meta.title if meta else "",
            "gancho": meta.hook if meta else "",
            "descripcion": meta.description if meta else "",
            "hashtags": list(meta.hashtags) if meta else [],
        })
    return clips


def _hms(seconds: float) -> str:
    minutes, secs = divmod(int(seconds), 60)
    hours, minutes = divmod(minutes, 60)
    return f"{hours}:{minutes:02d}:{secs:02d}" if hours else f"{minutes}:{secs:02d}"


def _scan_output(folder: Path) -> List[dict]:
    """Shorts ya generados en la carpeta de salida, con sus metadatos."""
    if not folder.is_dir():
        return []

    found = []
    for video in sorted(folder.glob("short_*.mp4")):
        entry = {"fichero": video.name, "ruta": str(video),
                 "titulo": "", "gancho": "", "pie": ""}
        sidecar = video.with_suffix(".json")
        if sidecar.is_file():
            try:
                data = json.loads(sidecar.read_text(encoding="utf-8"))
                publicacion = data.get("publicacion") or {}
                entry["titulo"] = publicacion.get("title", "")
                entry["gancho"] = publicacion.get("hook", "")
                tags = " ".join(f"#{t}" for t in publicacion.get("hashtags", []))
                entry["pie"] = (publicacion.get("description", "") +
                                ("\n\n" + tags if tags else "")).strip()
                entry["marca"] = data.get("fuente", {}).get("marca", "")
            except (OSError, json.JSONDecodeError):
                pass
        found.append(entry)
    return found


# ============================================================================
# Arranque
# ============================================================================

def _free_port(preferred: int) -> int:
    for port in range(preferred, preferred + 20):
        with socket.socket() as probe:
            if probe.connect_ex(("127.0.0.1", port)) != 0:
                return port
    raise RuntimeError("No hay ningun puerto libre entre "
                       f"{preferred} y {preferred + 19}.")


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Interfaz visual de viralshorts.")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--no-browser", action="store_true",
                        help="No abrir el navegador automaticamente.")
    options = parser.parse_args()

    if not (WEB_DIR / "index.html").is_file():
        print(f"Falta {WEB_DIR / 'index.html'}", file=sys.stderr)
        return 2

    port = _free_port(options.port)
    url = f"http://127.0.0.1:{port}/?t={TOKEN}"

    # Solo 127.0.0.1: esto abre procesos en tu maquina, no debe salir a la red.
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()

    print("=" * 62)
    print(" viralshorts - interfaz visual")
    print("=" * 62)
    print(f" Abre esta direccion si no se abre sola:\n   {url}")
    print(" Cierra esta ventana (o Ctrl+C) para apagar la interfaz.")
    print("=" * 62)
    sys.stdout.flush()
    if not options.no_browser:
        webbrowser.open(url)

    if PICKER.setup():
        # Tk manda en el hilo principal: su bucle atiende los dialogos.
        PICKER._root.after(100, _tk_loop)
        try:
            PICKER._root.mainloop()
        except KeyboardInterrupt:
            pass
    else:
        print("[aviso] tkinter no disponible: el boton 'Examinar' no funcionara.")
        try:
            while True:
                time.sleep(1)
        except KeyboardInterrupt:
            pass

    JOB.stop()
    return 0


def _tk_loop() -> None:
    PICKER.pump()
    PICKER._root.after(100, _tk_loop)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
