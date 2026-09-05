"""Cuanto se entiende un clip sin haber visto el resto del podcast.

Un clip puede estar en el minuto mas rebobinado del video y aun asi ser
inservible, porque empieza en "...y entonces eso es justo lo que le pasó a él".
El heatmap dice DONDE esta lo bueno; no dice donde EMPIEZA.

Aqui se puntua lo autocontenido que es el arranque de cada ventana, con dos
ideas independientes:

1. Fronteras de tema (TextTiling). Dos tramos seguidos que comparten
   vocabulario hablan de lo mismo; cuando el vocabulario cambia de golpe, ahi
   ha empezado un tema nuevo. Un clip que arranca en una de esas fronteras
   arranca por el principio de algo.

2. Arranque anaforico. "Eso", "el", "por eso", "como te decia": palabras que
   se refieren a algo que el espectador no ha visto. Una frase que empieza asi
   deja al que mira preguntandose de que va, que es exactamente lo que hace que
   pase de largo.

Ninguna de las dos necesita un modelo de lenguaje: la primera es estadistica de
vocabulario y la segunda es una lista de marcadores del discurso.
"""

from __future__ import annotations

import logging
import math
import re
import unicodedata
from collections import Counter
from typing import Dict, List, Optional, Sequence, Tuple

log = logging.getLogger(__name__)


# Marcadores que se refieren a algo dicho ANTES. Empezar un clip por aqui es
# empezar por la mitad.
_ANAPHORIC = (
    "eso", "esos", "esas", "aquello", "aquel", "aquella", "ese", "esa",
    "entonces", "por eso", "por ello", "asi que", "y por eso", "o sea",
    "es decir", "total que", "en fin", "resumiendo", "como decia",
    "como te decia", "como deciamos", "lo que decia", "lo que te decia",
    "volviendo", "ademas", "tambien", "igualmente", "encima", "y claro",
    "claro que", "pero claro", "y eso", "y ahi", "ahi es donde", "ahi fue",
    "de ahi", "el mismo", "la misma", "lo mismo", "otra vez", "de nuevo",
    "este tio", "esta tia", "este hombre", "esta mujer", "el tio", "la tia",
    "y el", "y ella", "y ellos", "le dije", "le dijo", "me dijo", "me decia",
    "pero ", "y ", "porque ", "sino", "aunque", "es que", "si es que",
    "so ", "then ", "and then", "that guy", "that thing", "as i said",
    "like i said", "anyway", "also", "besides", "again",
)

# Empezar preguntando o presentando SI es un buen arranque: el que pregunta
# suele estar abriendo tema, y la pregunta le da el contexto al espectador.
_OPENING = (
    "que es", "que son", "cual es", "por que", "como es", "como se",
    "sabes que", "sabias que", "te acuerdas", "os acordais", "imagina",
    "hay una", "hay un", "existe", "el otro dia", "una vez", "cuando yo",
    "cuando era", "mi ", "en ", "la gente", "todo el mundo", "resulta que",
    "what is", "what are", "why do", "how do", "did you know", "there is",
    "there was", "one day", "when i",
)

_STOP = {
    "que", "de", "la", "el", "los", "las", "un", "una", "unos", "unas", "y",
    "o", "pero", "porque", "como", "para", "por", "con", "sin", "sobre",
    "entre", "cuando", "donde", "esto", "eso", "este", "esta", "estos",
    "estas", "muy", "mas", "menos", "tan", "todo", "toda", "todos", "todas",
    "nada", "algo", "cada", "otro", "otra", "ser", "estar", "haber", "tener",
    "hacer", "decir", "ver", "ir", "dar", "era", "eran", "fue", "fueron",
    "hay", "habia", "esta", "estan", "estaba", "tiene", "tienen", "tenia",
    "hace", "hacen", "puede", "pueden", "vamos", "vale", "claro", "bueno",
    "entonces", "pues", "asi", "tambien", "siempre", "nunca", "ahora", "luego",
    "yo", "tu", "el", "ella", "nosotros", "ellos", "me", "te", "se", "nos",
    "le", "les", "lo", "mi", "su", "sus", "al", "del", "no", "si", "ya", "en",
    "es", "son", "hasta", "desde", "cosa", "cosas", "gente", "vez", "veces",
    "the", "a", "of", "to", "in", "on", "for", "and", "or", "but", "is", "are",
    "was", "were", "be", "have", "has", "it", "that", "this", "you", "i",
}

_WORD = re.compile(r"[^\W\d_]{3,}", re.UNICODE)


# ----------------------------------------------------------------------------
# Fronteras de tema
# ----------------------------------------------------------------------------

class TopicMap:
    """Instantes del podcast en los que cambia el tema de conversacion."""

    def __init__(self, boundaries: Sequence[float]) -> None:
        self.boundaries = sorted(boundaries)

    def distance(self, moment: float) -> float:
        """Segundos desde la frontera de tema anterior mas cercana.

        Se mira solo hacia atras: empezar 5 s despues de que arranque un tema
        es correcto; empezar 5 s ANTES es empezar dentro del tema anterior.
        """
        if not self.boundaries:
            return math.inf
        best = math.inf
        for boundary in self.boundaries:
            if boundary > moment + 1.0:
                break
            best = moment - boundary
        return best if best >= 0 else math.inf


def build_topic_map(transcript, cfg) -> TopicMap:
    """Detecta cambios de tema por cohesion lexica (TextTiling)."""
    blocks = _blocks(transcript, cfg.context.block_seconds)
    if len(blocks) < 2 * cfg.context.compare_blocks + 2:
        log.debug("Transcripcion demasiado corta para detectar temas.")
        return TopicMap([])

    span = cfg.context.compare_blocks
    gaps: List[Tuple[int, float]] = []
    for i in range(span, len(blocks) - span):
        before = Counter()
        after = Counter()
        for b in blocks[i - span:i]:
            before.update(b.words)
        for b in blocks[i:i + span]:
            after.update(b.words)
        gaps.append((i, 1.0 - _cosine(before, after)))

    if not gaps:
        return TopicMap([])

    # Solo interesan los maximos locales: una zona uniformemente distinta no es
    # una frontera, lo es el punto exacto donde cambia.
    values = [g[1] for g in gaps]
    mean = sum(values) / len(values)
    deviation = math.sqrt(sum((v - mean) ** 2 for v in values) / len(values))
    threshold = mean + cfg.context.sensitivity * deviation

    boundaries: List[float] = []
    for position, (index, value) in enumerate(gaps):
        if value < threshold:
            continue
        neighbourhood = values[max(0, position - 2):position + 3]
        if value >= max(neighbourhood):
            boundaries.append(blocks[index].start)

    log.info("Temas: %d cambios de tema detectados en la transcripcion.",
             len(boundaries))
    return TopicMap(boundaries)


class _Block:
    __slots__ = ("start", "words")

    def __init__(self, start: float, words: List[str]) -> None:
        self.start = start
        self.words = words


def _blocks(transcript, seconds: float) -> List[_Block]:
    """Trocea la transcripcion en bloques de duracion fija."""
    blocks: List[_Block] = []
    current: List[str] = []
    anchor: Optional[float] = None

    for word in transcript.words:
        if anchor is None:
            anchor = word.start
        # Un salto grande en el tiempo significa que hay un hueco sin
        # transcribir (preescaneo): los bloques no pueden cruzarlo.
        if word.start - anchor > seconds or (current and word.start - anchor < 0):
            blocks.append(_Block(anchor, current))
            current, anchor = [], word.start
        token = _fold(word.text)
        if len(token) >= 3 and token not in _STOP:
            current.append(token)

    if current and anchor is not None:
        blocks.append(_Block(anchor, current))
    return [b for b in blocks if b.words]


def _cosine(a: Counter, b: Counter) -> float:
    if not a or not b:
        return 0.0
    shared = set(a) & set(b)
    if not shared:
        return 0.0
    dot = sum(a[w] * b[w] for w in shared)
    na = math.sqrt(sum(v * v for v in a.values()))
    nb = math.sqrt(sum(v * v for v in b.values()))
    return dot / (na * nb) if na and nb else 0.0


# ----------------------------------------------------------------------------
# Puntuacion del arranque de una ventana
# ----------------------------------------------------------------------------

def apply(windows: Sequence["object"], topics: TopicMap, cfg) -> None:
    """Rellena `window.context` (0..1) en cada ventana, in place."""
    for window in windows:
        window.context = _score(window, topics, cfg)


def _score(window, topics: TopicMap, cfg) -> float:
    opening = _first_sentence(window)
    if not opening:
        return 0.0

    score = 0.5   # punto de partida neutro

    # 0. La senal mas barata y mas fiable de todas: Whisper pone mayuscula al
    #    empezar frase. Si la primera palabra del clip va en minuscula, el clip
    #    arranca literalmente a mitad de una frase. Nada lo arregla despues.
    if _starts_mid_sentence(window):
        score -= 0.45

    # 1. Empezar donde empieza un tema.
    distance = topics.distance(window.start)
    if distance <= cfg.context.boundary_grace:
        score += 0.35
    elif distance <= cfg.context.boundary_grace * 3:
        score += 0.12

    # 2. Arranque anaforico: el espectador no ha visto aquello a lo que
    #    se refiere el que habla.
    lowered = _fold(opening)
    if lowered.startswith(_ANAPHORIC):
        score -= 0.40
    elif lowered.startswith(_OPENING) or opening.lstrip().startswith("¿"):
        score += 0.20

    # 3. El asunto del clip debe nombrarse pronto. Si las palabras que mas se
    #    repiten en todo el clip no aparecen hasta la mitad, el arranque no
    #    dice de que se esta hablando.
    score += 0.25 * _subject_introduced(window, cfg)

    return max(0.0, min(1.0, score))


def _starts_mid_sentence(window) -> bool:
    words = getattr(window, "words", None)
    if not words:
        return False
    first = words[0].text.strip()
    if not first or not first[:1].isalpha():
        return False
    # Un nombre propio tambien va en mayuscula, pero eso solo produce falsos
    # negativos (no marcar un corte malo), nunca falsos positivos.
    return first[:1].islower()


def _first_sentence(window) -> str:
    words = getattr(window, "words", None)
    if not words:
        return (getattr(window, "text", "") or "")[:120]
    parts = []
    for word in words:
        parts.append(word.text.strip())
        if word.ends_sentence and len(parts) >= 3:
            break
    return " ".join(parts)


def _subject_introduced(window, cfg) -> float:
    """Fraccion de los temas del clip que ya se nombran en su arranque."""
    words = getattr(window, "words", None)
    if not words:
        return 0.0

    tokens = [(w.start, _fold(w.text)) for w in words]
    content = [(t, tok) for t, tok in tokens if len(tok) >= 4 and tok not in _STOP]
    if len(content) < 6:
        return 0.0

    top = [word for word, _ in Counter(tok for _, tok in content).most_common(4)]
    if not top:
        return 0.0

    cutoff = window.start + cfg.context.intro_seconds
    early = {tok for t, tok in content if t <= cutoff}
    return sum(1 for word in top if word in early) / len(top)


def _fold(text: str) -> str:
    decomposed = unicodedata.normalize("NFD", text.strip().lower())
    return "".join(c for c in decomposed if unicodedata.category(c) != "Mn")
