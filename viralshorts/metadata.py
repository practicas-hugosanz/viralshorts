"""Titulo, gancho, descripcion y hashtags SIN modelo de lenguaje.

Claude redacta. Esto no: esto *extrae*. Elige la mejor frase que ya ha dicho el
invitado y la usa tal cual, porque una frase real del clip siempre es
preferible a una frase inventada por una plantilla ("Descubre el secreto de
que...", que es lo que sale cuando se intenta redactar sin entender nada).

La consecuencia es honesta y conviene tenerla clara: el gancho sera bueno
cuando el invitado empiece el clip con una frase potente, y mediocre cuando
empiece con un "bueno, pues...". Un modelo reescribiria eso; aqui solo se puede
elegir la menos mala de las frases disponibles.

Se usa cuando el LLM esta desactivado, y tambien para rellenar los huecos que
deje el LLM si responde a medias.
"""

from __future__ import annotations

import logging
import re
import unicodedata
from collections import Counter
from typing import Dict, List, Optional, Sequence, Tuple

from .llm import ClipMetadata

log = logging.getLogger(__name__)


# ----------------------------------------------------------------------------
# Vocabulario
# ----------------------------------------------------------------------------

# Palabras que no dicen nada del tema y no pueden acabar en un hashtag.
_STOPWORDS = {
    # castellano
    "que", "de", "la", "el", "los", "las", "un", "una", "unos", "unas", "y", "o",
    "pero", "porque", "como", "para", "por", "con", "sin", "sobre", "entre",
    "cuando", "donde", "quien", "cual", "esto", "eso", "aquello", "este", "esta",
    "estos", "estas", "ese", "esa", "esos", "esas", "aqui", "alli", "ahi",
    "muy", "mas", "menos", "tan", "tanto", "poco", "mucho", "todo", "toda",
    "todos", "todas", "nada", "algo", "alguien", "nadie", "cada", "otro", "otra",
    "mismo", "misma", "ser", "estar", "haber", "tener", "hacer", "decir", "ver",
    "ir", "dar", "saber", "poder", "querer", "sabes", "digo", "dice", "dicen",
    "era", "eran", "fue", "fueron", "hay", "habia", "esta", "estan", "estaba",
    "tiene", "tienen", "tenia", "hace", "hacen", "hacia", "puede", "pueden",
    "vamos", "vale", "claro", "bueno", "entonces", "osea", "pues", "asi",
    "tambien", "siempre", "nunca", "ahora", "luego", "despues", "antes", "aun",
    "yo", "tu", "el", "ella", "nosotros", "vosotros", "ellos", "ellas", "me",
    "te", "se", "nos", "os", "le", "les", "lo", "mi", "su", "sus", "tus", "mis",
    "al", "del", "no", "si", "ya", "en", "es", "son", "soy", "eres", "hasta",
    "desde", "cosa", "cosas", "gente", "manera", "forma", "parte", "vez",
    "veces", "tema", "punto", "verdad", "momento", "dos", "tres", "anos", "ano",
    # Verbos corrientes conjugados. Sin esto, "hablo" y "gusta" se cuelan como
    # si fueran el asunto del clip, y el titulo se elige mirando hacia ellos.
    "hablo", "hablas", "habla", "hablan", "hablar", "hago", "haces", "hacemos",
    "doy", "das", "damos", "tengo", "tienes", "tenemos", "voy", "vas", "vamos",
    "creo", "crees", "cree", "creen", "quiero", "quieres", "quiere", "queremos",
    "puedo", "puedes", "podemos", "digo", "dices", "decimos", "veo", "ves",
    "vemos", "gusta", "gustan", "gustaba", "sabes", "sabe", "sabemos", "parece",
    "parecen", "pasa", "pasan", "paso", "pone", "ponen", "queda", "quedan",
    "mira", "miras", "oye", "venga", "anda", "salir", "sale", "salen", "llega",
    "llegan", "sigue", "siguen", "sido", "estado", "hecho", "dicho", "visto",
    "want", "wants", "think", "thinks", "make", "makes", "goes", "going",
    # ingles
    "the", "a", "an", "and", "or", "but", "because", "as", "of", "to", "in",
    "on", "for", "with", "without", "about", "that", "this", "these", "those",
    "is", "are", "was", "were", "be", "been", "have", "has", "had", "do", "does",
    "did", "will", "would", "can", "could", "should", "you", "i", "he", "she",
    "it", "we", "they", "them", "his", "her", "its", "our", "their", "not",
    "just", "like", "know", "think", "really", "very", "there", "here", "what",
    "when", "where", "who", "how", "why", "thing", "things", "people",
}

# Empezar por una de estas es empezar a media conversacion: mal gancho.
_FILLER_START = (
    "bueno", "o sea", "osea", "entonces", "pues", "y ", "pero ", "porque ",
    "eh ", "em ", "claro", "es decir", "vale", "digamos", "total", "en fin",
    "asi que", "así que", "de hecho", "por eso", "sea", "que ", "no ", "si ",
    "well ", "so ", "and ", "but ", "because ", "like ", "i mean", "yeah",
)

# Empezar por una de estas suele parar el scroll.
_STRONG_START = (
    "nunca", "jamas", "jamás", "nadie", "todo el mundo", "la verdad",
    "el problema", "resulta que", "imagina", "piensa", "escucha", "mira",
    "fijate", "fíjate", "lo que", "hay una", "hay un", "existe", "en realidad",
    "de verdad", "te juro", "te lo juro", "os juro", "os lo juro", "no te",
    "no me", "no hay", "nobody", "never",
    "everyone", "imagine", "the problem", "the truth", "listen", "look",
)

# Absolutos y superlativos: dan tension a una frase.
_INTENSE = (
    "nunca", "jamas", "jamás", "siempre", "nadie", "todo el mundo", "el mejor",
    "la mejor", "el peor", "la peor", "el unico", "el único", "imposible",
    "increible", "increíble", "brutal", "terrible", "flipante", "loco",
    "never", "always", "nobody", "everyone", "best", "worst", "only",
    "impossible", "incredible", "insane", "crazy",
)

# Cortar dejando una de estas al final deja la frase prometiendo algo que no
# llega: se quitan al truncar.
_DANGLING = {
    "el", "la", "los", "las", "un", "una", "unos", "unas", "del", "al", "de",
    "y", "o", "que", "en", "con", "por", "para", "sin", "sobre", "su", "sus",
    "mi", "tu", "lo", "se", "es", "muy", "mas", "tan", "como", "pero", "porque",
    "the", "a", "an", "of", "to", "in", "on", "for", "and", "or", "with", "my",
    "his", "her", "its", "very", "so", "but", "because", "that", "this",
}

# Arranques en primera persona: la frase va sobre quien habla, no sobre el tema.
_SELF_REFERENTIAL = (
    "hago", "hablo", "doy", "tengo", "voy", "quiero", "intento", "no voy",
    "yo hablo", "yo hago", "a mi me", "me da", "me gusta", "no me", "yo no",
    "cuidado, yo", "no, te doy", "aqui nadie", "i do", "i make", "i want",
)

_SENTENCE_SPLIT = re.compile(r"(?<=[.!?…])\s+|(?<=[.!?…])$")
_WORD = re.compile(r"[^\W\d_]{3,}", re.UNICODE)


# ----------------------------------------------------------------------------
# API
# ----------------------------------------------------------------------------

def write_metadata(
    clips: Sequence["object"],
    video_title: str,
    cfg,
    existing: Optional[Dict[int, ClipMetadata]] = None,
) -> Dict[int, ClipMetadata]:
    """Metadatos de cada clip. Rellena solo lo que falte en `existing`."""
    if not clips:
        return {}

    out: Dict[int, ClipMetadata] = dict(existing or {})
    source_tags = _tags_from_title(video_title)
    show_name = _show_name(video_title)

    for index, clip in enumerate(clips):
        if _is_complete(out.get(index)):
            continue
        built = _for_clip(clip, source_tags, cfg, show_name)
        current = out.get(index)
        # Si Claude escribio parte, se respeta lo suyo y solo se rellenan huecos.
        out[index] = ClipMetadata(
            title=(current.title if current and current.title else built.title),
            hook=(current.hook if current and current.hook else built.hook),
            description=(current.description if current and current.description
                         else built.description),
            hashtags=(current.hashtags if current and current.hashtags
                      else built.hashtags),
        )
    return out


def _is_complete(meta: Optional[ClipMetadata]) -> bool:
    # El gancho no entra: puede estar vacio a proposito (ver _for_clip).
    return bool(meta and meta.title and meta.description and meta.hashtags)


# ----------------------------------------------------------------------------
# Construccion de un clip
# ----------------------------------------------------------------------------

def _for_clip(clip, source_tags: List[str], cfg, show_name: str = "") -> ClipMetadata:
    sentences = _sentences(clip)
    if not sentences:
        return ClipMetadata(title=_clip_fallback(clip, cfg), hook="",
                            description="", hashtags=list(source_tags))

    # El gancho sale de las primeras frases: es el rotulo de los primeros
    # segundos, y prometer algo que se dice al final seria mentir.
    opening = [s for s in sentences if s.start <= cfg.metadata.hook_window] or sentences[:2]
    hook_sentence = max(opening, key=lambda s: _hook_score(s.text, cfg.metadata.max_hook))

    # Un rotulo malo es peor que ninguno: ocupa media pantalla para no decir
    # nada. Si la mejor frase disponible no llega al minimo, no se pone gancho
    # y la tarjeta de titulo se queda sola haciendo su trabajo.
    if _hook_score(hook_sentence.text, cfg.metadata.max_hook) < cfg.metadata.min_hook_score:
        hook = ""
    else:
        hook = _shorten(hook_sentence.text, cfg.metadata.max_hook).upper()

    # El titulo se usa como tarjeta de contexto al arrancar el video, asi que
    # tiene que decir DE QUE va el clip. Una frase suelta y bien construida no
    # basta: se premia la que nombra los asuntos de los que mas se habla.
    subjects = _key_terms(clip.text, 4)
    title_pool = [s for s in sentences if s is not hook_sentence] or sentences
    title_sentence = max(title_pool,
                         key=lambda s: _title_score(s.text, subjects, cfg.metadata.max_title))

    # Si ninguna frase del clip funciona como titulo, se usa la identidad del
    # programa. Una tarjeta que pone "The Wild Project - Xokas" no dice de que
    # va el clip, pero es informacion verdadera y se ve intencionada; una que
    # pone "Hago mis pinitos y lo que me gusta es aprender" parece un error.
    best = _title_score(title_sentence.text, subjects, cfg.metadata.max_title)
    if best >= cfg.metadata.min_title_score:
        title = _shorten(title_sentence.text, cfg.metadata.max_title)
    else:
        title = show_name or _shorten(title_sentence.text, cfg.metadata.max_title)

    used = {hook_sentence, title_sentence}
    rest = [s for s in sentences if s not in used]
    description = _description(rest or sentences, cfg.metadata.max_description)

    return ClipMetadata(
        title=title,
        hook=hook,
        description=description,
        hashtags=_hashtags(clip.text, source_tags, cfg.metadata.hashtags),
    )


class _Sentence:
    __slots__ = ("text", "start")

    def __init__(self, text: str, start: float) -> None:
        self.text = text
        self.start = start


def _sentences(clip) -> List[_Sentence]:
    """Frases del clip con el segundo en el que empieza cada una.

    Se reconstruyen desde las palabras con marca de tiempo para poder saber
    cuales caen dentro de los primeros segundos.
    """
    words = getattr(clip, "words", None)
    if not words:
        return [_Sentence(t.strip(), 0.0)
                for t in _SENTENCE_SPLIT.split(getattr(clip, "text", "") or "")
                if len(t.strip()) > 2]

    base = clip.start
    sentences: List[_Sentence] = []
    buffer: List[str] = []
    started = words[0].start - base

    for word in words:
        if not buffer:
            started = word.start - base
        buffer.append(word.text.strip())
        if word.ends_sentence:
            text = " ".join(buffer).strip()
            if len(text) > 2:
                sentences.append(_Sentence(text, started))
            buffer = []

    if buffer:
        text = " ".join(buffer).strip()
        if len(text) > 2:
            sentences.append(_Sentence(text, started))
    return sentences


# ----------------------------------------------------------------------------
# Puntuacion de frases
# ----------------------------------------------------------------------------

def _hook_score(text: str, max_chars: int) -> float:
    lowered = _fold(text)
    score = 0.0

    # "Se pudo conquistar" no dice nada aunque no tenga muletillas: sin
    # sustantivos concretos no hay curiosidad que despertar. Las cifras cuentan
    # como contenido -- "190 y 128 kilos" es concretisimo con una sola palabra.
    meaningful = _substance(text)

    # Una frase que no cabe en el rotulo hay que cortarla, y un gancho cortado
    # ("A FINALES DE 1980 O 1970 SE QUITARON...") no promete nada. El castigo
    # es alto a proposito: casi cualquier frase entera es mejor que una a medias.
    if len(text) > max_chars:
        score -= 3.2

    if meaningful < 2:
        score -= 2.4
    elif meaningful >= 4:
        score += 0.6

    # Interrogacion, pero solo si hay algo que preguntar. "¿Eh?" tambien acaba
    # en interrogante y no es un gancho; la transcripcion produce bastantes.
    if (text.rstrip().endswith("?") or text.lstrip().startswith("¿")) and meaningful >= 3:
        score += 2.2
    if re.search(r"\d", text):
        score += 1.4
    if lowered.startswith(tuple(_fold(s) for s in _STRONG_START)):
        score += 1.6
    if lowered.startswith(tuple(_fold(s) for s in _FILLER_START)):
        score -= 2.6
    if any(word in lowered for word in (_fold(w) for w in _INTENSE)):
        score += 0.9

    length = len(text)
    if 14 <= length <= 65:
        score += 1.1
    elif length < 12:
        score -= 2.0
    elif length > 110:
        score -= 1.3

    # Una frase con sustantivos concretos promete algo; una de puro relleno no.
    score += min(_content_ratio(text) * 2.0, 1.4)
    return score


def _title_score(text: str, subjects: Sequence[str] = (), max_chars: int = 70) -> float:
    """Como de bueno es este texto como tarjeta de contexto.

    Lo que se busca NO es la frase mejor construida, es la que dice de que va
    el clip. Por eso manda la cobertura del tema por encima de todo lo demas:
    "La capital era espectacular" es una frase impecable que no informa de
    nada, y pierde contra una peor que nombre el asunto.
    """
    subjects = list(subjects)
    score = 0.0

    if subjects:
        hits = sum(1 for term in subjects if term in _fold(text))
        score += 3.5 * (hits / len(subjects))

    # Informacion en terminos absolutos, no en proporcion: medir la FRACCION de
    # palabras con significado premia a la frase corta y vacia, que tiene pocas
    # palabras y todas cuentan.
    score += min(_substance(text), 7) * 0.35

    if re.search(r"\d", text):
        score += 0.8
    if text.rstrip().endswith("?"):
        score += 0.9
    if any(word in _fold(text) for word in (_fold(w) for w in _INTENSE)):
        score += 0.6
    if _fold(text).startswith(tuple(_fold(s) for s in _FILLER_START)):
        score -= 1.6

    # Una tarjeta de contexto habla del TEMA, no de las costumbres del que
    # habla. "Hago mis pinitos y lo que me gusta es aprender" es una frase
    # correcta que no dice de que va el clip.
    if _fold(text).startswith(_SELF_REFERENTIAL):
        score -= 1.5

    length = len(text)
    if length > max_chars:
        score -= 2.0          # habria que cortarla, y una tarjeta a medias no vale
    elif 26 <= length <= max_chars:
        score += 1.0
    elif length < 18:
        score -= 1.8
    return score


def _substance(text: str) -> int:
    """Unidades de significado de la frase: palabras plenas y cifras.

    Las cifras cuentan: "190 y 128 kilos" es concretisimo con una sola palabra.
    """
    words = sum(1 for w in _WORD.findall(text.lower()) if _fold(w) not in _STOPWORDS)
    numbers = len(re.findall(r"\d+", text))
    return words + numbers


def _content_ratio(text: str) -> float:
    """Fraccion de palabras que aportan significado (no muletillas)."""
    words = _WORD.findall(text.lower())
    if not words:
        return 0.0
    meaningful = [w for w in words if _fold(w) not in _STOPWORDS]
    return len(meaningful) / len(words)


# ----------------------------------------------------------------------------
# Hashtags
# ----------------------------------------------------------------------------

def _key_terms(text: str, limit: int) -> List[str]:
    """Las palabras de las que mas se habla en el clip: su asunto."""
    counts = Counter()
    for raw in _WORD.findall(text.lower()):
        word = _fold(raw)
        if word in _STOPWORDS or len(word) < 4 or _looks_like_verb(word):
            continue
        counts[word] += 1
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], -len(kv[0])))
    return [word for word, _ in ranked[:limit]]


def _hashtags(text: str, source_tags: List[str], limit: int) -> List[str]:
    """Las palabras con mas peso del clip, mas las del podcast de origen."""
    counts = Counter()
    for raw in _WORD.findall(text.lower()):
        word = _fold(raw)
        if word in _STOPWORDS or len(word) < 4 or _looks_like_verb(word):
            continue
        counts[word] += 1

    # A igualdad de repeticiones gana la palabra mas larga: es la mas
    # especifica, y un hashtag generico no lo busca nadie.
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], -len(kv[0])))

    tags: List[str] = []
    for tag in source_tags + [word for word, _ in ranked]:
        if tag not in tags:
            tags.append(tag)
        if len(tags) >= limit:
            break

    for generic in ("podcast", "shorts"):
        if len(tags) < limit and generic not in tags:
            tags.append(generic)
    return tags[:limit]


# Terminaciones de conjugacion en castellano. "#estaban" o "#aceptan" no los
# busca nadie; "#hispanidad" o "#guillotina" si. Sin analisis morfologico esto
# es lo mas lejos que se puede llegar, y quita los peores casos.
_VERB_ENDINGS = (
    "aban", "ieron", "aron", "aran", "eran", "iran", "emos", "amos", "imos",
    "ando", "endo", "aria", "eria", "iria", "abas", "istes", "aste", "iste",
    "aran", "asen", "esen", "iesen", "aria", "ando", "arse", "erse", "irse",
    "arlo", "erlo", "irlo", "arte", "amos",
)


def _looks_like_verb(word: str) -> bool:
    return len(word) > 5 and word.endswith(_VERB_ENDINGS)


def card_text(video_title: str, clip_title: str, cfg) -> str:
    """Texto de la tarjeta de contexto, segun `title_card.source`.

    Por defecto es el titulo del video de origen sin la lista de temas del
    episodio: "The Wild Project #375 - Xokas | ¿Por que no pelea..." se queda
    en "The Wild Project - Xokas". Es informacion verdadera, siempre esta bien
    escrita, y dice al que llega de golpe QUE esta viendo -- que es justo lo
    que una frase suelta sacada del clip no consigue.
    """
    source = getattr(cfg.title_card, "source", "show")
    if source == "video":
        return video_title or clip_title
    if source == "clip":
        return clip_title
    return _show_name(video_title) or video_title or clip_title


def _show_name(video_title: str) -> str:
    """Programa e invitado, sacados del titulo del video de origen.

    "The Wild Project #375 - Xokas | ¿Por que no pelea..." -> "The Wild Project · Xokas".
    Es el respaldo del titulo: nunca queda raro y siempre es cierto.
    """
    if not video_title:
        return ""
    head = video_title.split("|")[0].strip()
    head = re.sub(r"#\s*\d+", "", head).strip(" -–—·|")
    parts = [p.strip() for p in re.split(r"\s[-–—]\s", head) if p.strip()]
    if not parts:
        return ""
    if len(parts) >= 2:
        # En dos lineas a proposito: el programa arriba y el invitado debajo se
        # lee mejor que una linea larga partida por donde caiga.
        return parts[0] + "\n" + parts[1]
    return parts[0]


def _tags_from_title(video_title: str) -> List[str]:
    """Un unico hashtag con el nombre del programa.

    Trocear "The Wild Project #375 - Xokas" en #wild y #project da dos hashtags
    que no busca nadie; "#thewildproject" si es el canal.
    """
    if not video_title:
        return []
    # El nombre del programa es lo que va antes del numero de episodio o del
    # primer separador.
    show = re.split(r"[|#–—]|\s-\s", video_title)[0]
    words = [_fold(w) for w in _WORD.findall(show.lower())]
    words = [w for w in words if w not in _STOPWORDS or len(words) <= 2]
    slug = "".join(words)[:24]
    return [slug] if len(slug) >= 5 else []


# ----------------------------------------------------------------------------
# Utilidades de texto
# ----------------------------------------------------------------------------

def _description(sentences: Sequence[_Sentence], max_chars: int) -> str:
    """Una o dos frases seguidas, sin pasarse de largo."""
    out = ""
    for sentence in sorted(sentences, key=lambda s: -_title_score(s.text))[:2]:
        candidate = (out + " " + sentence.text).strip()
        if len(candidate) > max_chars and out:
            break
        out = candidate
    return _shorten(out, max_chars)


def _shorten(text: str, limit: int) -> str:
    """Recorta por frontera de palabra, sin dejar articulos colgando.

    Cortar en "...se quitaron las" es peor que cortar en "...se quitaron": la
    palabra huerfana promete un sustantivo que nunca llega.
    """
    text = _tidy(text)
    if len(text) <= limit:
        return text

    cut = text[:limit + 1]
    space = cut.rfind(" ")
    if space > limit * 0.55:
        cut = cut[:space]

    words = cut.split()
    while len(words) > 3 and _fold(words[-1].strip(".,;:¿?¡!")) in _DANGLING:
        words.pop()
    return " ".join(words).rstrip(" ,;:.-") + "…"


def _tidy(text: str) -> str:
    """Limpia los tics de la transcripcion: espacios sueltos antes de coma."""
    text = re.sub(r"\s+", " ", text).strip()
    text = re.sub(r"\s+([,.;:!?…])", r"", text)
    text = re.sub(r"([¿¡])\s+", r"", text)
    return text.strip("-–—,;: ")


def _clip_fallback(clip, cfg) -> str:
    return _shorten(getattr(clip, "text", "") or "Clip", cfg.metadata.max_title)


def _fold(text: str) -> str:
    """Minusculas sin tildes: 'Increíble' y 'increible' son la misma palabra."""
    decomposed = unicodedata.normalize("NFD", text.lower())
    return "".join(c for c in decomposed if unicodedata.category(c) != "Mn")
