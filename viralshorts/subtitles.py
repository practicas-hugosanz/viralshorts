"""Generacion del fichero .ASS de subtitulos estilo TikTok / CapCut.

Dos diferencias importantes respecto a una implementacion ingenua:

1. NO hay parpadeo. Si cada linea dura exactamente lo que dura su palabra, en
   el hueco entre el final de una palabra y el comienzo de la siguiente (50-300
   ms) no se dibuja nada y el subtitulo titila. Aqui cada linea se extiende
   hasta el arranque de la siguiente, de modo que el bloque esta siempre en
   pantalla y lo unico que cambia es cual es la palabra resaltada.

2. La palabra activa hace "pop": entra un 18% mas grande y vuelve a su tamano
   en 120 ms. Es el detalle que separa un subtitulo automatico de uno editado.
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Optional, Sequence, Tuple

from .transcribe import Word

log = logging.getLogger(__name__)

# Si entre dos palabras hay mas silencio que esto, empieza un bloque nuevo:
# corresponde a una pausa real del que habla.
_GROUP_BREAK_GAP = 0.7
# Cuanto se mantiene en pantalla el ultimo bloque tras la ultima palabra.
_TAIL_HOLD = 0.40


@dataclass
class Group:
    words: List[Word] = field(default_factory=list)

    @property
    def start(self) -> float:
        return self.words[0].start

    @property
    def end(self) -> float:
        return self.words[-1].end


# ----------------------------------------------------------------------------
# Agrupacion
# ----------------------------------------------------------------------------

def group_words(words: Sequence[Word], cfg) -> List[Group]:
    """Parte la lista de palabras en bloques cortos que caben en pantalla."""
    groups: List[Group] = []
    current: List[Word] = []

    for index, word in enumerate(words):
        current.append(word)

        is_last = index == len(words) - 1
        gap_to_next = 0.0 if is_last else words[index + 1].start - word.end

        if (len(current) >= cfg.max_words_per_group
                or word.ends_sentence
                or gap_to_next >= _GROUP_BREAK_GAP
                or is_last):
            groups.append(Group(words=current))
            current = []

    if current:
        groups.append(Group(words=current))
    return groups


# ----------------------------------------------------------------------------
# Fichero .ASS
# ----------------------------------------------------------------------------

def build_ass(
    words: Sequence[Word],
    width: int,
    height: int,
    cfg_subs,
    cfg_hook=None,
    hook_text: Optional[str] = None,
    duration: float = 0.0,
    cfg_title=None,
    title_text: Optional[str] = None,
) -> str:
    """Construye el .ASS completo (cabecera + eventos).

    Arriba pueden aparecer dos rotulos, y NUNCA a la vez: primero la tarjeta de
    titulo (el contexto: de que va esto) y, cuando esa se va, el gancho.
    Solaparlos taparia media cara del que habla.
    """
    groups = group_words(words, cfg_subs) if (cfg_subs.enabled and words) else []
    events = _subtitle_events(groups, cfg_subs)

    card_end = 0.0
    overlays: List[str] = []
    if cfg_title is not None and cfg_title.enabled and title_text:
        card = _title_event(title_text, cfg_title, duration, cfg_subs)
        if card:
            overlays += card
            card_end = min(cfg_title.duration, duration) if duration else cfg_title.duration

    if cfg_hook is not None and cfg_hook.enabled and hook_text:
        overlays += _hook_event(hook_text, cfg_hook, duration,
                                start=card_end, cfg_subs=cfg_subs)

    events = overlays + events
    log.info("Subtitulos: %d bloques, %d lineas de dialogo.", len(groups), len(events))
    return (_header(width, height, cfg_subs, cfg_hook, cfg_title)
            + "\n".join(events) + "\n")


def write_ass(content: str, path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    # libass espera UTF-8; sin BOM para no confundir a versiones antiguas.
    path.write_text(content, encoding="utf-8")
    return path


# ----------------------------------------------------------------------------

def _header(width: int, height: int, cfg_subs, cfg_hook, cfg_title=None) -> str:
    primary = hex_to_ass_color(cfg_subs.color_normal)
    outline = hex_to_ass_color(cfg_subs.color_outline)
    shadow = hex_to_ass_color(cfg_subs.color_shadow)
    margin_v = int(height * cfg_subs.vertical_position_ratio)
    side_margin = int(width * 0.07)

    styles = [
        # Alignment 2 = abajo centrado; MarginV se mide desde el borde inferior.
        f"Style: Main,{cfg_subs.font_name},{cfg_subs.font_size},{primary},{primary},"
        f"{outline},{shadow},-1,0,0,0,100,100,0,0,1,{cfg_subs.outline_width},"
        f"{cfg_subs.shadow_depth},2,{side_margin},{side_margin},{margin_v},1"
    ]

    if cfg_hook is not None:
        hook_color = hex_to_ass_color(cfg_hook.color)
        hook_outline = hex_to_ass_color(cfg_hook.color_outline)
        hook_margin = int(height * cfg_hook.top_margin_ratio)
        # Alignment 8 = arriba centrado; aqui MarginV se mide desde arriba.
        styles.append(
            f"Style: Hook,{cfg_hook.font_name},{cfg_hook.font_size},{hook_color},"
            f"{hook_color},{hook_outline},{hook_outline},-1,0,0,0,100,100,0,0,1,"
            f"{max(3, cfg_subs.outline_width - 1)},2,8,{side_margin},{side_margin},"
            f"{hook_margin},1"
        )

    if cfg_title is not None and cfg_title.enabled:
        card_color = hex_to_ass_color(cfg_title.color)
        card_outline = hex_to_ass_color(cfg_title.color_outline)
        # Alignment 5 = centro exacto del fotograma; con MarginV a 0 el rotulo
        # cae en la costura entre el panel del podcast y el del gameplay, que
        # es el punto donde primero mira el ojo.
        # BorderStyle 1 (contorno, no caja) para que se vea como los
        # subtitulos y no como un cartel pegado encima.
        styles.append(
            f"Style: Titulo,{cfg_title.font_name or cfg_subs.font_name},"
            f"{cfg_title.font_size},{card_color},{card_color},"
            f"{card_outline},{card_outline},-1,0,0,0,100,100,0,0,1,"
            f"{cfg_title.outline_width},{cfg_title.shadow_depth},5,"
            f"{side_margin},{side_margin},0,1"
        )

    return (
        "[Script Info]\n"
        "Title: viralshorts\n"
        "ScriptType: v4.00+\n"
        "WrapStyle: 2\n"
        "ScaledBorderAndShadow: yes\n"
        "YCbCr Matrix: TV.709\n"
        f"PlayResX: {width}\n"
        f"PlayResY: {height}\n"
        "\n"
        "[V4+ Styles]\n"
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, "
        "OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, "
        "ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, "
        "MarginL, MarginR, MarginV, Encoding\n"
        + "\n".join(styles) + "\n"
        "\n"
        "[Events]\n"
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    )


def _subtitle_events(groups: Sequence[Group], cfg) -> List[str]:
    active_color = hex_to_ass_color(cfg.color_active)
    normal_color = hex_to_ass_color(cfg.color_normal)
    pop_ms = int(cfg.pop_duration * 1000)

    lines: List[str] = []
    for group_index, group in enumerate(groups):
        # Fin del bloque: enlazamos con el siguiente para que no haya un hueco
        # en negro, pero sin arrastrarlo si viene una pausa larga.
        if group_index + 1 < len(groups):
            next_start = groups[group_index + 1].start
            group_end = min(next_start, group.end + _TAIL_HOLD)
            group_end = max(group_end, group.end + 0.02)
        else:
            group_end = group.end + _TAIL_HOLD

        rendered = [_render_word(w, cfg) for w in group.words]

        for i, word in enumerate(group.words):
            start = word.start if i > 0 else group.start
            # Clave contra el parpadeo: la linea vive hasta que arranca la
            # siguiente palabra, no hasta que acaba la actual.
            end = group.words[i + 1].start if i + 1 < len(group.words) else group_end
            if end - start < 0.02:
                continue

            parts = []
            for j, text in enumerate(rendered):
                if j == i:
                    pop = ""
                    if cfg.pop_scale != 100 and pop_ms > 0:
                        pop = (f"\\fscx{cfg.pop_scale}\\fscy{cfg.pop_scale}"
                               f"\\t(0,{pop_ms},\\fscx100\\fscy100)")
                    parts.append(f"{{\\c{active_color}{pop}}}{text}")
                else:
                    parts.append(f"{{\\c{normal_color}}}{text}")

            lines.append(
                f"Dialogue: 0,{format_time(start)},{format_time(end)},Main,,0,0,0,,"
                + " ".join(parts)
            )

    return lines


def _title_event(text: str, cfg, duration: float, cfg_subs=None) -> List[str]:
    """Tarjeta de contexto al arrancar el video.

    Aparece en el centro, se lee, se va. Su unico trabajo es que el que llega
    de golpe sepa que esta viendo antes de decidir si sigue mirando.
    """
    end = min(cfg.duration, duration) if duration else cfg.duration
    if end <= 0:
        return []

    clean = censor_text(text.strip(), cfg_subs)[: cfg.max_chars]
    if cfg.uppercase:
        clean = clean.upper()
    body = escape_ass_text(_wrap(clean, cfg.wrap_chars))

    fade = int(max(0.0, cfg.fade) * 1000)
    # Entra con el mismo rebote que la palabra activa de los subtitulos, para
    # que se lea como parte del mismo diseno y no como un rotulo aparte.
    effect = ("{" + f"\\fscx88\\fscy88\\t(0,200,\\fscx103\\fscy103)"
              f"\\t(200,320,\\fscx100\\fscy100)\\fad({fade},{fade})" + "}")
    return [f"Dialogue: 2,{format_time(0)},{format_time(end)},Titulo,,0,0,0,,{effect}{body}"]


def _hook_event(text: str, cfg, duration: float, start: float = 0.0,
                cfg_subs=None) -> List[str]:
    end = min(start + cfg.duration, duration) if duration else start + cfg.duration
    if end <= start:
        return []

    clean = censor_text(text.strip(), cfg_subs)[: cfg.max_chars]
    if cfg.uppercase:
        clean = clean.upper()
    body = escape_ass_text(_wrap(clean, 24))

    # Entrada con un pequeno rebote y salida por desvanecido.
    effect = "{\\fscx70\\fscy70\\t(0,180,\\fscx104\\fscy104)\\t(180,300,\\fscx100\\fscy100)\\fad(0,350)}"
    return [f"Dialogue: 1,{format_time(start)},{format_time(end)},Hook,,0,0,0,,{effect}{body}"]


def _render_word(word: Word, cfg) -> str:
    text = word.text.strip()
    if cfg.remove_punctuation:
        text = text.translate(str.maketrans("", "", cfg.punctuation_chars)).strip()
    if not text:
        text = word.text.strip()
    if cfg.uppercase:
        text = text.upper()
    text = _censor(text, cfg.censor_words, cfg.censor_profanity, cfg.censor_char)
    return escape_ass_text(text)


# ----------------------------------------------------------------------------
# Censura de palabrotas
# ----------------------------------------------------------------------------
#
# Se tapa UNA letra, la primera vocal de la raiz malsonante: "puta" -> "p#ta",
# "gilipollas" -> "gilip#llas". La palabra se sigue leyendo de un vistazo (que
# es lo que se quiere: el clip no pierde gracia) pero deja de ser texto
# literal para los filtros automaticos de las plataformas.
#
# Solo afecta al subtitulo. El audio no se toca.

# Raices, no palabras: asi "puta", "putas", "putada" y "putazo" caen las cuatro
# con una sola entrada.
_PROFANITY_ROOTS = (
    "puta", "puto", "pute", "polla", "pollez", "mierd", "jod", "coñ",
    "cabron", "cabron", "hostia", "folla", "follar", "follo", "capull",
    "subnormal", "maric", "zorra", "cojon", "cag", "gilipoll", "pendejo",
    "chinga", "verga", "boludo", "mamon", "gonorrea", "carajo", "cornud",
    "fuck", "shit", "bitch", "asshole", "cunt", "dick",
)

# La raiz vale si la palabra empieza por ella o por uno de estos refuerzos.
# La lista es corta a proposito: con "re" dentro, "reputacion" se convertiria
# en "rep#tacion", que es justo lo que hay que evitar.
_PROFANITY_PREFIXES = ("hijo", "hija", "hijode", "gili", "mala", "malla")

# Palabras inocentes que empiezan por una de las raices. Se comprueban antes
# que nada: es mas facil de mantener y de leer que retorcer las raices para
# que no las cojan.
_NOT_PROFANITY = frozenset({
    "follaje", "follajes", "coñac", "coñacs",
})

_VOWELS = "aeiouáéíóúü"
_ACCENTS = str.maketrans("áéíóúüÁÉÍÓÚÜ", "aeiouuaeiouu")


def _fold_word(word: str) -> str:
    """Minusculas y sin tildes, pero CONSERVANDO la enye.

    Si se quitase, "coño" pasaria a "cono" y la raiz "coñ" empezaria a marcar
    "conocer", "con" y "cono".
    """
    return word.lower().translate(_ACCENTS)


def _find_root(word: str) -> Optional[Tuple[int, str]]:
    """Posicion y raiz malsonante dentro de la palabra, si la hay."""
    folded = _fold_word(word)
    if folded in _NOT_PROFANITY:
        return None
    for root in _PROFANITY_ROOTS:
        if folded.startswith(root):
            return 0, root
        for prefix in _PROFANITY_PREFIXES:
            if folded.startswith(prefix + root):
                return len(prefix), root
    return None


def _censor(text: str, extra: Sequence[str], enabled: bool = True,
            mask: str = "#") -> str:
    """Tapa la primera vocal de la raiz malsonante de la palabra."""
    bare = re.sub(r"\W", "", text, flags=re.UNICODE)
    if not bare:
        return text

    found = _find_root(bare) if enabled else None
    if found is None:
        # Las palabras que anada el usuario se tratan como raices tambien, para
        # que "cabronazo" caiga si ha escrito "cabron".
        folded = _fold_word(bare)
        for word in extra:
            root = _fold_word(re.sub(r"\W", "", word, flags=re.UNICODE))
            if root and folded.startswith(root):
                found = (0, root)
                break
    if found is None:
        return text

    offset, root = found
    # Primera vocal DENTRO de la raiz: es la silaba que hace reconocible la
    # palabrota, y taparla ahi es lo que se lee como censura deliberada.
    vowel = next((i for i, c in enumerate(root) if c in _VOWELS), 0)
    target = offset + vowel

    # `text` puede llevar signos de puntuacion delante ("¡puta"); hay que
    # traducir el indice de la palabra desnuda al indice del texto real.
    seen = -1
    for index, char in enumerate(text):
        if re.match(r"\w", char, flags=re.UNICODE):
            seen += 1
            if seen == target:
                return text[:index] + mask + text[index + 1:]
    return text


def censor_text(text: str, cfg_subs=None) -> str:
    """Aplica la censura a una frase entera, palabra por palabra.

    El rotulo y la tarjeta de titulo salen de la transcripcion igual que los
    subtitulos, asi que tienen exactamente el mismo problema.
    """
    if cfg_subs is None:
        return text
    return " ".join(
        _censor(chunk, cfg_subs.censor_words, cfg_subs.censor_profanity,
                cfg_subs.censor_char)
        for chunk in text.split(" ")
    )


def _wrap(text: str, width: int) -> str:
    """Parte el texto en lineas de como mucho `width` caracteres.

    Los saltos de linea que ya trae el texto se respetan: hay sitios donde el
    corte no es cuestion de anchura sino de sentido (el nombre del programa
    arriba y el invitado debajo, por ejemplo).
    """
    if "\n" in text:
        # Se devuelven con "\n" como el resto: es escape_ass_text quien los
        # traduce al "\N" del formato .ASS, y hacerlo aqui los rompe.
        parts = [p for p in text.split("\n") if p.strip()]
        return "\n".join(_wrap(part, width) for part in parts)
    words = text.split()
    lines: List[str] = []
    current = ""
    for word in words:
        candidate = f"{current} {word}".strip()
        if len(candidate) > width and current:
            lines.append(current)
            current = word
        else:
            current = candidate
    if current:
        lines.append(current)
    return "\n".join(lines)


# ----------------------------------------------------------------------------
# Utilidades de formato .ASS
# ----------------------------------------------------------------------------

def hex_to_ass_color(hex_color: str, alpha: int = 0) -> str:
    """#RRGGBB -> &HAABBGGRR& (el .ASS invierte el orden de los bytes).

    `alpha` va de 0 (opaco) a 255 (invisible), al reves que en CSS.
    """
    value = hex_color.lstrip("#")
    if len(value) != 6:
        raise ValueError(f"Color invalido: '{hex_color}'. Usa el formato #RRGGBB.")
    r, g, b = value[0:2], value[2:4], value[4:6]
    return f"&H{max(0, min(255, alpha)):02X}{b}{g}{r}&".upper()


def format_time(seconds: float) -> str:
    """Segundos -> H:MM:SS.CS, el formato de tiempo del .ASS."""
    centiseconds = max(0, round(seconds * 100))
    hours, centiseconds = divmod(centiseconds, 360000)
    minutes, centiseconds = divmod(centiseconds, 6000)
    secs, centis = divmod(centiseconds, 100)
    return f"{hours:d}:{minutes:02d}:{secs:02d}.{centis:02d}"


def escape_ass_text(text: str) -> str:
    """Neutraliza los caracteres que romperian la sintaxis del .ASS."""
    return (text.replace("\\", "")
                .replace("{", "(")
                .replace("}", ")")
                .replace("\r", "")
                .replace("\n", "\\N"))
