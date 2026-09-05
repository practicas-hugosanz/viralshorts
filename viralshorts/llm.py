"""Puntuacion de contenido y generacion de metadatos con Claude.

La densidad de palabras mide quien habla mas rapido, no que momento engancha.
Aqui le pasamos a Claude las transcripciones de las ventanas candidatas y le
pedimos que las puntue como lo haria un editor de shorts: gancho, tension,
remate, y si se entiende sin contexto previo.

Todo el modulo degrada con elegancia: si no hay credenciales o falla la API, el
pipeline sigue con las senales heuristicas (heatmap + energia + densidad).
"""

from __future__ import annotations

import hashlib
import json
import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence

from . import progress

log = logging.getLogger(__name__)

# Errores que merecen otro intento: la API saturada, la conexion caida, o el
# limite de peticiones por minuto. Lo demas (credenciales, peticion invalida)
# no mejora por reintentarlo.
_RETRYABLE = ("rate limit", "rate_limit", "overloaded", "timeout", "timed out",
              "connection", "502", "503", "504", "529",
              "internal server error", "apiconnection", "apitimeout")


_SCORING_SYSTEM = """\
Eres editor jefe de un canal de shorts verticales (TikTok / Reels / YouTube \
Shorts) que recorta podcasts. Recibes fragmentos transcritos y los puntuas.

Para cada fragmento das dos notas de 0 a 10:

content_score - cuanto engancha el fragmento ENTERO:
  9-10  historia con giro, confesion, opinion muy polemica, dato que sorprende,
        discusion acalorada, consejo concreto y accionable.
  6-8   anecdota o idea con principio y final claros.
  3-5   conversacion normal, correcta pero olvidable.
  0-2   relleno, presentaciones, patrocinios, chistes internos, frases sueltas
        que no se entienden sin el contexto de antes.

hook_score - cuanto agarran los primeros 3 segundos (la primera frase):
  9-10  pregunta directa, afirmacion chocante, cifra concreta, "nunca te han
        contado que...".
  6-8   entra en materia sin rodeos.
  3-5   arranca a media frase pero se recupera.
  0-2   empieza con muletillas, "eh", "bueno", o respondiendo a algo que no se
        ha oido.

Penaliza sin piedad: nombres propios sin presentar, referencias a "lo que \
dijimos antes", y fragmentos cuya frase inicial esta cortada.

Se exigente: la mayoria de fragmentos de un podcast son un 4. Reserva el 9 y el \
10 para lo que de verdad pararia el scroll."""


_METADATA_SYSTEM = """\
Escribes los textos de publicacion de shorts verticales recortados de podcasts.
Para cada clip devuelves:

- title: titulo del video, maximo 70 caracteres, sin comillas ni emojis, en el
  idioma del clip. Concreto y con la tension del clip, nunca generico.
- hook: rotulo que aparece SOBRE el video los primeros segundos. Maximo 45
  caracteres, en MAYUSCULAS, formulado como pregunta o afirmacion chocante.
  Es lo que decide si alguien se queda: tiene que dar curiosidad sin resolverla.
- description: 1 o 2 frases para el pie de publicacion.
- hashtags: entre 4 y 7, sin la almohadilla, en minusculas, sin espacios,
  mezclando 2 muy genericos del nicho con otros mas especificos del tema.

Nunca inventes datos que no esten en el clip. No uses clickbait que el clip no
cumpla: si el clip no responde a la pregunta del gancho, cambia el gancho."""


_SCORE_SCHEMA = {
    "type": "object",
    "properties": {
        "clips": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer"},
                    "content_score": {"type": "number"},
                    "hook_score": {"type": "number"},
                    "reason": {"type": "string"},
                },
                "required": ["id", "content_score", "hook_score", "reason"],
                "additionalProperties": False,
            },
        }
    },
    "required": ["clips"],
    "additionalProperties": False,
}

_METADATA_SCHEMA = {
    "type": "object",
    "properties": {
        "clips": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer"},
                    "title": {"type": "string"},
                    "hook": {"type": "string"},
                    "description": {"type": "string"},
                    "hashtags": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["id", "title", "hook", "description", "hashtags"],
                "additionalProperties": False,
            },
        }
    },
    "required": ["clips"],
    "additionalProperties": False,
}


@dataclass
class LLMScore:
    content: float        # 0..1
    hook: float           # 0..1
    reason: str


@dataclass
class ClipMetadata:
    title: str
    hook: str
    description: str
    hashtags: List[str]

    def caption(self) -> str:
        tags = " ".join(f"#{t.lstrip('#')}" for t in self.hashtags)
        return f"{self.description}\n\n{tags}".strip()


class LLMUnavailable(RuntimeError):
    """No hay forma de llamar a la API (falta el SDK o las credenciales)."""


class ClipJudge:
    """Cliente de Claude para puntuar clips y escribir sus metadatos."""

    def __init__(self, cfg, cache_dir: Optional[Path] = None) -> None:
        self.cfg = cfg
        self._client = None
        self._disabled_reason: Optional[str] = None
        self._cache = _Cache(Path(cache_dir) if cache_dir else None,
                             enabled=getattr(cfg, "cache", True))

    # -- cliente --------------------------------------------------------
    @property
    def available(self) -> bool:
        if self._disabled_reason is not None:
            return False
        try:
            self._ensure_client()
        except LLMUnavailable as exc:
            self._disabled_reason = str(exc)
            log.warning("Claude no disponible (%s). Se puntuara solo con "
                        "heatmap + energia + densidad.", exc)
            return False
        return True

    def _ensure_client(self):
        if self._client is not None:
            return self._client
        try:
            import anthropic
        except ImportError as exc:
            raise LLMUnavailable("falta el paquete 'anthropic'") from exc

        try:
            # El SDK resuelve credenciales de ANTHROPIC_API_KEY, de
            # ANTHROPIC_AUTH_TOKEN o del perfil de `ant auth login`.
            self._client = anthropic.Anthropic()
        except Exception as exc:
            raise LLMUnavailable(f"no hay credenciales configuradas: {exc}") from exc
        return self._client

    def _call(self, system: str, user: str, schema: dict) -> dict:
        """Una peticion, reintentando los fallos que si se arreglan solos."""
        attempts = max(1, getattr(self.cfg, "max_retries", 3))
        delay = max(0.5, getattr(self.cfg, "retry_delay", 3.0))

        for attempt in range(1, attempts + 1):
            try:
                return self._call_once(system, user, schema)
            except Exception as exc:
                if attempt >= attempts or not _is_retryable(exc):
                    raise
                wait = delay * (2 ** (attempt - 1))
                log.warning("Claude fallo (%s). Reintento %d de %d en %.0fs.",
                            _short(exc), attempt + 1, attempts, wait)
                time.sleep(wait)
        raise RuntimeError("inalcanzable")

    def _call_once(self, system: str, user: str, schema: dict) -> dict:
        client = self._ensure_client()
        response = client.messages.create(
            model=self.cfg.model,
            max_tokens=self.cfg.max_tokens,
            system=system,
            thinking={"type": "adaptive"},
            output_config={
                "effort": self.cfg.effort,
                "format": {"type": "json_schema", "schema": schema},
            },
            messages=[{"role": "user", "content": user}],
        )

        if response.stop_reason == "refusal":
            detail = getattr(response.stop_details, "explanation", "") or ""
            raise RuntimeError(f"Claude declino la peticion. {detail}".strip())

        text = next((b.text for b in response.content if b.type == "text"), None)
        if not text:
            raise RuntimeError("Claude no devolvio contenido de texto.")

        usage = response.usage
        log.debug("Claude: %s tokens entrada / %s salida",
                  usage.input_tokens, usage.output_tokens)
        return json.loads(text)

    # -- puntuacion -----------------------------------------------------
    def score_windows(self, windows: Sequence["object"]) -> Dict[int, LLMScore]:
        """Puntua las ventanas candidatas. Devuelve {indice -> LLMScore}.

        Se manda por lotes, no todo de golpe: con dos docenas de fragmentos
        largos en una sola peticion la respuesta se corta por max_tokens y los
        ultimos se quedan sin nota (y sin nota no compiten con los demas). Lo
        ya puntuado en una ejecucion anterior sale de la cache y no se paga
        dos veces.
        """
        if not windows or not self.available:
            return {}

        scores: Dict[int, LLMScore] = {}
        pending: List[int] = []
        for index, window in enumerate(windows):
            cached = self._cache.get("score", self._key(window.text))
            if cached is not None:
                scores[index] = LLMScore(
                    content=float(cached["content"]), hook=float(cached["hook"]),
                    reason=str(cached.get("reason", "")),
                )
            else:
                pending.append(index)

        if scores:
            log.info("%d fragmentos ya puntuados antes; se reutilizan.", len(scores))
        if not pending:
            return scores

        batches = list(_chunks(pending, max(1, getattr(self.cfg, "batch_size", 10))))
        log.info("Pidiendo a %s que puntue %d fragmentos (%d peticion(es))...",
                 self.cfg.model, len(pending), len(batches))

        for number, batch in enumerate(batches, start=1):
            progress.progress("claude", (number - 1) / len(batches),
                              f"Claude puntuando ({number}/{len(batches)})")
            user = (
                f"Puntua estos {len(batch)} fragmentos de un podcast. "
                "Devuelve una entrada por fragmento, usando el mismo id.\n\n"
                + "\n\n".join(
                    f"<fragmento id=\"{i}\" duracion=\"{windows[i].duration:.0f}s\">\n"
                    f"{windows[i].text}\n"
                    f"</fragmento>"
                    for i in batch
                )
            )
            try:
                data = self._call(_SCORING_SYSTEM, user, _SCORE_SCHEMA)
            except Exception as exc:
                log.warning("Fallo la puntuacion con Claude (%s). Se sigue con "
                            "el heatmap y la energia del audio.", exc)
                self._disabled_reason = str(exc)
                progress.result(
                    "aviso",
                    titulo="Claude no ha podido puntuar los fragmentos",
                    texto=f"{_short(exc)}. Los momentos se eligen igualmente con "
                          "el heatmap de YouTube, la energia del audio y el "
                          "contexto, pero la seleccion es menos fina.",
                )
                break

            got = 0
            for item in data.get("clips", []):
                try:
                    index = int(item["id"])
                except (KeyError, TypeError, ValueError):
                    continue
                if index not in batch:
                    continue
                score = LLMScore(
                    content=_clamp01(float(item.get("content_score", 0)) / 10.0),
                    hook=_clamp01(float(item.get("hook_score", 0)) / 10.0),
                    reason=str(item.get("reason", "")).strip(),
                )
                scores[index] = score
                got += 1
                self._cache.put("score", self._key(windows[index].text),
                                {"content": score.content, "hook": score.hook,
                                 "reason": score.reason})
            if got < len(batch):
                log.warning("Claude no puntuo %d de %d fragmentos de este lote.",
                            len(batch) - got, len(batch))

        self._cache.flush()
        progress.progress("claude", 1.0, "Claude ha terminado", final=True)
        return scores

    # -- metadatos ------------------------------------------------------
    def write_metadata(
        self, clips: Sequence["object"], video_title: str
    ) -> Dict[int, ClipMetadata]:
        """Genera titulo, gancho, descripcion y hashtags para cada clip."""
        if not clips or not self.cfg.generate_metadata:
            return {}
        if not self.available:
            return {}

        out: Dict[int, ClipMetadata] = {}
        pending: List[int] = []
        for index, clip in enumerate(clips):
            cached = self._cache.get("meta", self._key(clip.text, video_title))
            if cached is not None:
                out[index] = ClipMetadata(**cached)
            else:
                pending.append(index)

        if not pending:
            return out

        batches = list(_chunks(pending,
                               max(1, getattr(self.cfg, "metadata_batch_size", 6))))
        log.info("Generando titulos, ganchos y hashtags para %d clips "
                 "(%d peticion(es))...", len(pending), len(batches))

        for batch in batches:
            user = (
                f"Podcast de origen: \"{video_title}\".\n\n"
                f"Escribe los textos de publicacion de estos {len(batch)} clips.\n\n"
                + "\n\n".join(f"<clip id=\"{i}\">\n{clips[i].text}\n</clip>"
                              for i in batch)
            )
            try:
                data = self._call(_METADATA_SYSTEM, user, _METADATA_SCHEMA)
            except Exception as exc:
                log.warning("Fallo la generacion de metadatos (%s). Se escriben "
                            "por extraccion.", exc)
                progress.result(
                    "aviso",
                    titulo="Claude no ha podido escribir los textos",
                    texto=f"{_short(exc)}. Los titulos, ganchos y hashtags se "
                          "sacan de las propias frases del clip.",
                )
                break

            for item in data.get("clips", []):
                try:
                    index = int(item["id"])
                except (KeyError, TypeError, ValueError):
                    continue
                if index not in batch:
                    continue
                meta = ClipMetadata(
                    title=str(item.get("title", "")).strip(),
                    hook=str(item.get("hook", "")).strip(),
                    description=str(item.get("description", "")).strip(),
                    hashtags=[str(t).lstrip("#").strip()
                              for t in item.get("hashtags", []) if str(t).strip()],
                )
                out[index] = meta
                self._cache.put("meta", self._key(clips[index].text, video_title),
                                {"title": meta.title, "hook": meta.hook,
                                 "description": meta.description,
                                 "hashtags": meta.hashtags})

        self._cache.flush()
        return out

    # -- cache ----------------------------------------------------------
    def _key(self, text: str, extra: str = "") -> str:
        """Identifica una peticion por su contenido Y por como se pidio.

        Cambiar de modelo o de esfuerzo invalida lo guardado: es otra
        respuesta, no la misma guardada.
        """
        payload = "|".join((str(self.cfg.model), str(self.cfg.effort), extra, text))
        return hashlib.sha1(payload.encode("utf-8")).hexdigest()[:20]


def _clamp01(value: float) -> float:
    return max(0.0, min(1.0, value))


def _chunks(items: Sequence[int], size: int) -> Iterable[List[int]]:
    for start in range(0, len(items), size):
        yield list(items[start:start + size])


def _is_retryable(exc: Exception) -> bool:
    text = f"{type(exc).__name__} {exc}".lower()
    return any(needle in text for needle in _RETRYABLE)


def _short(exc: Exception) -> str:
    text = str(exc).strip()
    return text.splitlines()[0][:160] if text else type(exc).__name__


class _Cache:
    """Respuestas ya pagadas, guardadas en disco entre ejecuciones.

    Un fichero unico con todo dentro: son unos kilobytes por video y evita
    llenar la carpeta de cache de miles de ficheros diminutos.
    """

    def __init__(self, cache_dir: Optional[Path], enabled: bool = True) -> None:
        self.enabled = bool(enabled and cache_dir)
        self.path = (cache_dir / "llm.json") if cache_dir else None
        self._data: Dict[str, Dict[str, dict]] = {}
        self._dirty = False
        if self.enabled and self.path and self.path.is_file():
            try:
                self._data = json.loads(self.path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                log.debug("Cache del modelo ilegible; se empieza de cero.")
                self._data = {}

    def get(self, kind: str, key: str) -> Optional[dict]:
        if not self.enabled:
            return None
        return self._data.get(kind, {}).get(key)

    def put(self, kind: str, key: str, value: dict) -> None:
        if not self.enabled:
            return
        self._data.setdefault(kind, {})[key] = value
        self._dirty = True

    def flush(self) -> None:
        if not (self.enabled and self._dirty and self.path):
            return
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self.path.write_text(json.dumps(self._data, ensure_ascii=False),
                                 encoding="utf-8")
            self._dirty = False
        except OSError as exc:
            log.debug("No se pudo guardar la cache del modelo: %s", exc)
