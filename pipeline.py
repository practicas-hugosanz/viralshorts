#!/usr/bin/env python3
"""viralshorts - de un podcast de YouTube a shorts verticales subtitulados.

Uso tipico:

    python pipeline.py --podcast <URL> --background <URL>
    python pipeline.py --podcast <URL> --background gameplay.mp4 --clips 5
    python pipeline.py --podcast <URL> --background <URL> --dry-run

Trabajo en dos tiempos (elegir ahora, montar despues de revisar):

    python pipeline.py --podcast <URL> --background <URL> \
                       --plan-out work/plans/plan.json --propose 9
    python pipeline.py --from-plan work/plans/plan.json --only 1,4,7

Todo lo ajustable esta en config.yaml (o en viralshorts/config.py, que son los
valores por defecto). Los argumentos de linea de comandos tienen prioridad, y
`--set clave=valor` llega a cualquier ajuste sin tocar el fichero.
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict, fields, is_dataclass
from pathlib import Path
from typing import Dict, List, Optional, Sequence

from viralshorts import (audio_energy, context, exports, facecrop, fonts,
                         jumpcuts, metadata, plan as planning, prescan,
                         progress, scoring, subtitles)
from viralshorts.config import Config
from viralshorts.download import SourceVideo, fetch
from viralshorts.ffmpeg_utils import FFmpegError, probe, require_ffmpeg
from viralshorts.llm import ClipJudge, ClipMetadata
from viralshorts.logging_setup import setup_logging
from viralshorts.render import RenderRequest, extract_thumbnail, render
from viralshorts.scoring import Window, hms
from viralshorts.transcribe import Transcriber
from viralshorts.worklock import WorkDirBusy, WorkLock

log = logging.getLogger("viralshorts")


# ----------------------------------------------------------------------------
# CLI
# ----------------------------------------------------------------------------

def parse_args(argv: Optional[List[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="pipeline.py",
        description="Genera shorts verticales subtitulados a partir de un podcast.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--podcast",
                        help="URL de YouTube o ruta a un fichero local.")
    parser.add_argument("--background",
                        help="Video de fondo (gameplay). URL o fichero local.")
    parser.add_argument("--config", default="config.yaml",
                        help="Fichero YAML de configuracion (por defecto: config.yaml).")
    parser.add_argument("--set", dest="overrides", action="append", default=[],
                        metavar="CLAVE=VALOR",
                        help="Cambia cualquier ajuste sin tocar el YAML. "
                             "Ej: --set subtitles.font_size=80")
    parser.add_argument("--clips", type=int, metavar="N",
                        help="Cuantos shorts generar.")
    parser.add_argument("--min-duration", type=float, metavar="SEG")
    parser.add_argument("--max-duration", type=float, metavar="SEG")
    parser.add_argument("--head-skip", type=float, metavar="SEG",
                        help="Segundos iniciales que se ignoran (intro, patrocinios).")
    parser.add_argument("--tail-skip", type=float, metavar="SEG",
                        help="Segundos finales que se ignoran (despedida).")
    parser.add_argument("--output", metavar="DIR", help="Carpeta de salida.")
    parser.add_argument("--model", metavar="TAMANO",
                        help="Modelo de Whisper: tiny, base, small, medium, large-v3.")
    parser.add_argument("--language", metavar="COD",
                        help="Idioma del podcast (es, en...). 'auto' para detectarlo.")
    parser.add_argument("--full-transcript", action="store_true",
                        help="Transcribir el video entero en vez de solo las zonas "
                             "calientes (mucho mas lento; solo para depurar).")
    parser.add_argument("--regions", type=int, metavar="N",
                        help="Zonas a preseleccionar y transcribir (por defecto: auto).")
    parser.add_argument("--beam", type=int, metavar="N",
                        help="beam_size de Whisper. 1 = greedy (rapido), 5 = mejor calidad.")
    parser.add_argument("--no-llm", action="store_true",
                        help="No usar Claude: puntuar solo con heatmap, energia y densidad.")
    parser.add_argument("--no-censor", action="store_true",
                        help="No tapar las palabrotas en los subtitulos.")
    parser.add_argument("--no-title-card", action="store_true",
                        help="No mostrar la tarjeta de contexto del principio.")
    parser.add_argument("--no-context", action="store_true",
                        help="No tener en cuenta si el clip se entiende sin contexto.")
    parser.add_argument("--no-metadata", action="store_true",
                        help="No escribir titulo, gancho ni hashtags.")
    parser.add_argument("--no-reframe", action="store_true",
                        help="Desactivar el seguimiento de caras (recorte centrado).")
    parser.add_argument("--no-jumpcuts", action="store_true",
                        help="No eliminar silencios.")
    parser.add_argument("--no-zoom", action="store_true",
                        help="Desactivar el punch-in (render mas rapido).")
    parser.add_argument("--dry-run", action="store_true",
                        help="Elegir y mostrar los clips sin renderizar nada.")

    group = parser.add_argument_group("montaje en dos tiempos")
    group.add_argument("--plan-out", metavar="FICHERO",
                       help="Elegir los momentos y guardarlos para revisarlos, "
                            "sin renderizar.")
    group.add_argument("--propose", type=int, metavar="N",
                       help="Cuantos candidatos proponer en el plan "
                            "(por defecto: el triple de los que se piden).")
    group.add_argument("--from-plan", metavar="FICHERO",
                       help="Renderizar los clips marcados en un plan guardado.")
    group.add_argument("--only", metavar="N,N",
                       help="Con --from-plan: renderizar solo estos numeros de clip.")

    parser.add_argument("--verbose", "-v", action="store_true")
    return parser.parse_args(argv)


def apply_overrides(cfg: Config, args: argparse.Namespace) -> None:
    if args.clips is not None:
        cfg.clips.num_clips = args.clips
    if args.min_duration is not None:
        cfg.clips.min_duration = args.min_duration
    if args.max_duration is not None:
        cfg.clips.max_duration = args.max_duration
    if args.head_skip is not None:
        cfg.clips.head_skip = args.head_skip
    if args.tail_skip is not None:
        cfg.clips.tail_skip = args.tail_skip
    if args.output:
        cfg.output.dir = args.output
    if args.model:
        cfg.transcribe.model_size = args.model
    if args.language:
        cfg.transcribe.language = None if args.language == "auto" else args.language
    if args.full_transcript:
        cfg.prescan.enabled = False
    if args.regions is not None:
        cfg.prescan.regions = args.regions
    if args.beam is not None:
        cfg.transcribe.beam_size = args.beam
    if args.no_llm:
        cfg.llm.enabled = False
    if args.no_metadata:
        cfg.metadata.enabled = False
    if args.no_context:
        cfg.context.enabled = False
    if args.no_title_card:
        cfg.title_card.enabled = False
    if args.no_censor:
        cfg.subtitles.censor_profanity = False
    if args.no_reframe:
        cfg.reframe.enabled = False
    if args.no_jumpcuts:
        cfg.jumpcuts.enabled = False
    if args.no_zoom:
        cfg.zoom.enabled = False

    for item in args.overrides or []:
        apply_setting(cfg, item)

    if cfg.clips.min_duration > cfg.clips.max_duration:
        raise ValueError("clips.min_duration no puede ser mayor que clips.max_duration.")


def apply_setting(cfg: Config, item: str) -> None:
    """`--set subtitles.font_size=80`, con el tipo del ajuste de destino."""
    if "=" not in item:
        raise ValueError(f"--set espera clave=valor, no '{item}'.")

    dotted, raw = item.split("=", 1)
    target = cfg
    parts = [p.strip() for p in dotted.strip().split(".") if p.strip()]
    if not parts:
        raise ValueError(f"--set: clave vacia en '{item}'.")

    for part in parts[:-1]:
        if not is_dataclass(target) or part not in {f.name for f in fields(target)}:
            raise ValueError(f"--set: no existe el ajuste '{dotted}'.")
        target = getattr(target, part)

    name = parts[-1]
    if not is_dataclass(target) or name not in {f.name for f in fields(target)}:
        raise ValueError(f"--set: no existe el ajuste '{dotted}'.")

    setattr(target, name, _coerce(getattr(target, name), raw.strip()))


def _coerce(current, raw: str):
    """Convierte el texto de la linea de comandos al tipo que ya tiene el campo."""
    lowered = raw.lower()
    if lowered in ("null", "none", ""):
        return None
    if isinstance(current, bool):
        if lowered in ("1", "true", "si", "sí", "yes", "on"):
            return True
        if lowered in ("0", "false", "no", "off"):
            return False
        raise ValueError(f"--set: '{raw}' no es si/no.")
    if isinstance(current, int) and not isinstance(current, bool):
        return int(float(raw))
    if isinstance(current, float):
        return float(raw)
    if isinstance(current, list):
        return [part.strip() for part in raw.split(",") if part.strip()]
    return raw


# ----------------------------------------------------------------------------
# Pipeline
# ----------------------------------------------------------------------------

def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv)
    setup_logging(args.verbose)

    if not args.from_plan and not (args.podcast and args.background):
        log.error("Hacen falta --podcast y --background (o --from-plan).")
        return 2

    try:
        config_path = Path(args.config)
        cfg = Config.load(config_path if config_path.is_file() else None)
        apply_overrides(cfg, args)
    except (ValueError, FileNotFoundError, ImportError) as exc:
        log.error("Configuracion invalida: %s", exc)
        return 2

    try:
        require_ffmpeg()
    except FFmpegError as exc:
        log.error("%s", exc)
        return 2

    started = time.perf_counter()
    work_dir = Path(cfg.paths.work_dir)
    output_dir = Path(cfg.output.dir)
    output_dir.mkdir(parents=True, exist_ok=True)

    # El cerrojo cubre toda la ejecucion: descargas, cache y ficheros
    # temporales del render viven en work_dir y no admiten dos duenos.
    try:
        lock = WorkLock(work_dir)
        lock.acquire()
    except WorkDirBusy as exc:
        log.error("%s", exc)
        return 2

    try:
        if args.from_plan:
            return _run_from_plan(args, cfg, work_dir, output_dir, started)
        return _run(args, cfg, work_dir, output_dir, started)
    finally:
        lock.release()


def _check_fonts(cfg: Config) -> None:
    """Avisa ANTES de renderizar si la fuente de los subtitulos no existe.

    libass no protesta: cuando no encuentra la fuente pone otra y sigue. El
    short sale entero con unos subtitulos que no son los configurados, y solo
    se descubre al verlo.
    """
    for label, name in (("subtitulos", cfg.subtitles.font_name),
                        ("gancho", cfg.hook.font_name),
                        ("tarjeta", cfg.title_card.font_name or cfg.subtitles.font_name)):
        if not cfg.subtitles.enabled or not name:
            continue
        if fonts.is_available(name, cfg.subtitles.fonts_dir):
            continue
        alternative = fonts.suggest_alternative(name)
        log.warning("La fuente '%s' (%s) no esta instalada en el sistema; ffmpeg "
                    "pondra otra. Instalala, o usa: --set subtitles.font_name=\"%s\"",
                    name, label, alternative)
        progress.result(
            "aviso",
            titulo=f"Falta la fuente «{name}»",
            texto=f"Los subtitulos saldran con otra fuente cualquiera. Instalala en "
                  f"el sistema (no basta con tener el .ttf suelto) o elige «{alternative}».",
        )
        break


def _run(args, cfg: Config, work_dir: Path, output_dir: Path, started: float) -> int:
    _check_fonts(cfg)

    making_plan = bool(args.plan_out)
    proposed = _proposed_count(args, cfg) if making_plan else cfg.clips.num_clips

    try:
        clips, podcast, background, judge, energy = select_clips(
            args, cfg, work_dir, limit=proposed)
    except Exception as exc:
        log.error("No se pudieron seleccionar los clips: %s", exc)
        if args.verbose:
            log.exception("Traza completa:")
        return 1

    _report_selection(clips)
    progress.result("clips", clips=[
        {"n": i, "inicio": round(c.start, 2), "fin": round(c.end, 2),
         "marca": hms(c.start), "duracion": round(c.duration, 1),
         "puntuacion": round(c.score, 3), "motivo": c.reason,
         "texto": c.text[:400]}
        for i, c in enumerate(clips, start=1)
    ])

    if args.dry_run and not making_plan:
        log.info("--dry-run: no se renderiza nada.")
        return 0

    if making_plan:
        # Los textos solo para los que vienen marcados: los demas se escriben
        # si el usuario acaba eligiendolos, y no antes.
        preselected = min(cfg.clips.num_clips, len(clips))
        progress.stage("metadatos", "Escribiendo titulos y hashtags")
        written = _metadata_for(judge, clips[:preselected], podcast.title, cfg)
        path = planning.save(Path(args.plan_out), cfg, podcast, background,
                             clips, written, selected=preselected)
        progress.result("plan", ruta=str(path.absolute()),
                        clips=_plan_events(planning.load(path)))
        log.info("Plan listo: %s. Revisalo y monta con --from-plan.", path)
        return 0

    progress.stage("metadatos", "Escribiendo titulos y hashtags")
    written = _metadata_for(judge, clips, podcast.title, cfg)
    output_dir = _run_output_dir(cfg, output_dir, podcast.title,
                                 time.strftime("%Y%m%d_%H%M%S"))
    results = _render_all(clips, written, podcast, background, cfg,
                          work_dir, output_dir, energy=energy)

    _finish(cfg, output_dir)
    return _report_results(results, len(clips), time.perf_counter() - started)


def _run_output_dir(cfg: Config, base: Path, title: str, tag: str) -> Path:
    """Carpeta donde caen los shorts de ESTA ejecucion, y se la dice a la GUI."""
    folder = base
    if getattr(cfg.output, "run_subfolder", True):
        folder = exports.run_folder(base, title, tag)
    folder.mkdir(parents=True, exist_ok=True)
    progress.result("salida", ruta=str(folder.absolute()))
    return folder


def _proposed_count(args, cfg: Config) -> int:
    """Cuantos candidatos se proponen para revisar a mano."""
    if args.propose:
        return max(cfg.clips.num_clips, args.propose)
    # El triple de los pedidos: suficiente para descartar los que no valen sin
    # convertir la revision en un trabajo de media hora.
    return min(max(cfg.clips.num_clips * 3, 6), 20)


def select_clips(args, cfg: Config, work_dir: Path, limit: Optional[int] = None):
    """Descarga, transcribe, puntua y devuelve los mejores clips."""
    progress.stage("descarga", "Descargando los videos")
    podcast = fetch(args.podcast, work_dir / "podcast", cfg.download)
    background = fetch(args.background, work_dir / "background", cfg.download)

    # El orden importa: la energia y el heatmap no necesitan texto y se
    # calculan en segundos, asi que sirven para decidir QUE hay que transcribir.
    progress.stage("analisis", "Analizando audio y buscando momentos")
    energy = audio_energy.analyse(podcast.path, podcast.duration)

    regions = None
    if cfg.prescan.enabled:
        regions = prescan.choose_regions(
            podcast.duration, podcast.heatmap, energy,
            cfg.prescan, cfg.clips, cfg.scoring,
        )

    progress.stage("transcripcion", "Transcribiendo el podcast")
    transcriber = Transcriber(cfg.transcribe, Path(cfg.paths.cache_dir))
    if regions:
        transcript = transcriber.transcribe_regions(podcast.path, podcast.duration, regions)
    else:
        transcript = transcriber.transcribe(podcast.path, podcast.duration)

    windows = scoring.build_windows(transcript, cfg.clips, regions)
    scoring.apply_cheap_signals(windows, podcast.heatmap, energy)

    if cfg.context.enabled:
        # El heatmap dice donde esta lo bueno; esto dice donde EMPIEZA, para que
        # el clip no arranque a mitad de una conversacion que nadie ha visto.
        topics = context.build_topic_map(transcript, cfg)
        context.apply(windows, topics, cfg)

    judge = ClipJudge(cfg.llm, Path(cfg.paths.cache_dir))
    if cfg.llm.enabled:
        shortlist = scoring.shortlist_for_llm(
            windows, cfg.scoring.llm_candidates, cfg.scoring
        )
        progress.stage("claude", "Claude esta puntuando los fragmentos")
        llm_scores = judge.score_windows(shortlist)
        scoring.apply_llm_scores(shortlist, llm_scores)
        # Solo compiten las ventanas que Claude ha visto: mezclarlas con las
        # miles que no ha puntuado haria ganar siempre a una sin puntuar.
        candidates = shortlist if llm_scores else windows
    else:
        log.info("Claude desactivado (--no-llm): puntuando solo con heuristicas.")
        candidates = windows

    clips = scoring.finalise(candidates, cfg.scoring, cfg.clips, limit=limit)
    if not clips:
        raise RuntimeError("No se selecciono ningun clip.")
    return clips, podcast, background, judge, energy


# ----------------------------------------------------------------------------
# Render a partir de un plan revisado
# ----------------------------------------------------------------------------

class _PlanSource:
    """Lo poco que el render necesita de un video ya descargado."""

    def __init__(self, data: dict) -> None:
        self.path = Path(data.get("ruta", ""))
        self.title = data.get("titulo", "")
        self.duration = float(data.get("duracion", 0.0))
        self.info = {"webpage_url": data.get("url", "")}


def _run_from_plan(args, cfg: Config, work_dir: Path, output_dir: Path,
                   started: float) -> int:
    try:
        saved = planning.load(Path(args.from_plan))
    except (FileNotFoundError, ValueError) as exc:
        log.error("%s", exc)
        return 2

    # La configuracion del plan manda: es con la que se eligieron los clips.
    # Encima se aplican los argumentos de esta llamada (carpeta de salida,
    # --set de estilo), que es lo que el usuario acaba de pedir.
    try:
        cfg = Config.load(None)
        _merge_dict(cfg, saved.config)
        apply_overrides(cfg, args)
    except ValueError as exc:
        log.error("El plan trae ajustes que esta version no entiende: %s", exc)
        return 2

    # La carpeta de salida sale de la configuracion ya fusionada: el plan
    # trae la suya, y --output de esta llamada manda sobre ella.
    output_dir = Path(cfg.output.dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    _check_fonts(cfg)

    wanted = _wanted_numbers(args, saved)
    if not wanted:
        log.error("No hay ningun clip marcado para montar en %s.", saved.path.name)
        return 2

    podcast = _PlanSource(saved.podcast)
    background = _PlanSource(saved.background)
    for source, label in ((podcast, "podcast"), (background, "video de fondo")):
        if not source.path.is_file():
            log.error("El %s del plan ya no esta en %s. Vuelve a buscar los "
                      "momentos para descargarlo otra vez.", label, source.path)
            return 2

    # Misma carpeta cada vez que se monta el mismo plan: volver a montarlo
    # tras corregir un titulo reemplaza sus shorts, no los duplica.
    output_dir = _run_output_dir(cfg, output_dir, podcast.title,
                                 saved.path.stem.removeprefix("plan_"))

    clips = [saved.clips[n - 1].to_window() for n in wanted]
    written: Dict[int, ClipMetadata] = {
        index: saved.clips[n - 1].meta
        for index, n in enumerate(wanted) if saved.clips[n - 1].meta
    }

    missing = [index for index in range(len(clips)) if index not in written]
    if missing:
        # Clips que el usuario ha marcado despues: no tenian textos escritos.
        progress.stage("metadatos", "Escribiendo titulos y hashtags")
        judge = ClipJudge(cfg.llm, Path(cfg.paths.cache_dir))
        extra = _metadata_for(judge, [clips[i] for i in missing], podcast.title, cfg)
        for position, index in enumerate(missing):
            if position in extra:
                written[index] = extra[position]

    log.info("Montando %d clip(s) del plan %s: %s",
             len(clips), saved.path.name, ", ".join(str(n) for n in wanted))
    results = _render_all(clips, written, podcast, background, cfg,
                          work_dir, output_dir, numbers=wanted)

    _finish(cfg, output_dir)
    return _report_results(results, len(clips), time.perf_counter() - started)


def _wanted_numbers(args, saved: "planning.Plan") -> List[int]:
    """Numeros de clip a montar: los de --only, o los marcados en el plan."""
    if args.only:
        wanted = []
        for chunk in str(args.only).replace(" ", "").split(","):
            if not chunk:
                continue
            try:
                number = int(chunk)
            except ValueError:
                log.warning("--only: '%s' no es un numero de clip.", chunk)
                continue
            if 1 <= number <= len(saved.clips):
                wanted.append(number)
            else:
                log.warning("--only: el plan no tiene ningun clip %d.", number)
        return sorted(set(wanted))
    return [i + 1 for i in saved.selected]


def _merge_dict(target, data: dict) -> None:
    """Vuelca el diccionario de configuracion del plan sobre un Config."""
    from viralshorts.config import _apply
    _apply(target, data, path="plan")


def _plan_events(saved: "planning.Plan") -> List[dict]:
    """Los candidatos del plan, en el formato que consume la interfaz."""
    events = []
    for index, clip in enumerate(saved.clips, start=1):
        meta = clip.meta
        events.append({
            "n": index,
            "inicio": round(clip.start, 2),
            "fin": round(clip.end, 2),
            "marca": hms(clip.start),
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
    return events


# ----------------------------------------------------------------------------
# Textos de publicacion
# ----------------------------------------------------------------------------

def _metadata_for(judge: ClipJudge, clips, title: str, cfg: Config) -> Dict[int, ClipMetadata]:
    """Textos de publicacion: Claude si esta disponible, si no por extraccion.

    Los dos caminos se combinan a proposito. Cuando Claude contesta a medias
    -- se deja un clip, o devuelve el titulo pero no los hashtags -- lo que
    falta se rellena con el metodo heuristico en vez de salir vacio.
    """
    if not clips:
        return {}

    written: Dict[int, ClipMetadata] = {}
    if cfg.llm.enabled:
        written = judge.write_metadata(clips, title)

    if not cfg.metadata.enabled:
        return written

    complete = sum(1 for m in written.values() if m.title and m.hook and m.hashtags)
    if complete < len(clips):
        if cfg.llm.enabled:
            log.info("Completando los textos de %d clips sin modelo de lenguaje.",
                     len(clips) - complete)
        else:
            log.info("Escribiendo titulos y hashtags por extraccion (sin Claude).")
        written = metadata.write_metadata(clips, title, cfg, existing=written)
    return written


# ----------------------------------------------------------------------------
# Render
# ----------------------------------------------------------------------------

def _render_workers(cfg: Config, total: int) -> int:
    """Cuantos shorts montar a la vez.

    ffmpeg reparte el filtrado y la codificacion entre varios hilos, pero hay
    tramos serie (el reencuadre con OpenCV, el arranque, la escritura) en los
    que la maquina se queda a medio gas. Con dos renders a la vez esos huecos
    los llena el otro clip.
    """
    import os

    configured = int(getattr(cfg.output, "render_workers", 0) or 0)
    if configured > 0:
        return max(1, min(configured, total))

    cores = os.cpu_count() or 4
    auto = 2 if cores >= 8 else 1
    return max(1, min(auto, total))


def _render_all(
    clips: List[Window],
    written: Dict[int, ClipMetadata],
    podcast,
    background,
    cfg: Config,
    work_dir: Path,
    output_dir: Path,
    numbers: Optional[Sequence[int]] = None,
    energy=None,
):
    background_duration = probe(background.path).duration
    numbers = list(numbers) if numbers else list(range(1, len(clips) + 1))
    workers = _render_workers(cfg, len(clips))

    if workers > 1:
        # Con varios ffmpeg a la vez, el porcentaje de cada uno pisa al de los
        # demas; informamos del avance por clips terminados.
        progress.suppress("render")
        log.info("Montando %d shorts, %d a la vez.", len(clips), workers)
        progress.stage("render", f"Montando {len(clips)} shorts ({workers} a la vez)")

    done = 0
    results: List[tuple] = []

    def build(index: int):
        clip = clips[index]
        number = numbers[index]
        if workers == 1:
            progress.stage("render", f"Renderizando clip {number} de {len(clips)}")
            log.info("=" * 68)
            log.info("CLIP %d/%d  %s", number, len(clips), clip.describe())
            if clip.reason:
                log.info("Motivo: %s", clip.reason)
        return _render_one(
            clip=clip,
            number=number,
            meta=written.get(index),
            podcast=podcast,
            background=background,
            background_duration=background_duration,
            cfg=cfg,
            work_dir=work_dir,
            output_dir=output_dir,
            energy=energy,
        )

    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(build, index): index for index in range(len(clips))}
        for future in as_completed(futures):
            index = futures[future]
            number = numbers[index]
            try:
                output = future.result()
                results.append((number, output, None))
                meta = written.get(index)
                progress.result(
                    "clip_done", n=number, ruta=str(output.absolute()),
                    fichero=output.name,
                    titulo=meta.title if meta else "",
                    gancho=meta.hook if meta else "",
                    pie=meta.caption() if meta else "",
                )
            except Exception as exc:
                # Un clip que falla no debe tirar los demas.
                log.error("El clip %d fallo: %s", number, exc)
                log.debug("Traza:", exc_info=True)
                results.append((number, None, str(exc)))
                progress.result("clip_failed", n=number, error=str(exc))
            finally:
                done += 1
                if workers > 1:
                    progress.progress("render_total", done / len(clips),
                                      f"Montando shorts ({done}/{len(clips)})",
                                      final=done == len(clips))

    progress.allow("render")
    results.sort(key=lambda row: row[0])
    return results


def _render_one(clip, number, meta, podcast, background, background_duration,
                cfg, work_dir, output_dir, energy=None) -> Path:
    # 1. Palabras del clip, con el tiempo relativo a su inicio.
    words = [w.shifted(-clip.start) for w in clip.words]

    # 2. Que silencios quitamos, y como afecta eso a los tiempos.
    plan = jumpcuts.plan(words, clip.duration, cfg.jumpcuts)
    words = plan.remap_words(words)

    # 3. Trayectoria del encuadre (sobre el clip sin cortar). La curva de
    #    energia le dice cuando hay alguien hablando y cuando no; si no viene
    #    dada (montaje desde un plan) se calcula solo la de este clip, que
    #    son unas decimas de segundo.
    if energy is None and cfg.reframe.enabled and cfg.reframe.speaker_tracking:
        try:
            energy = audio_energy.analyse_range(podcast.path, clip.start, clip.duration)
        except Exception as exc:
            log.debug("Sin energia para el clip %d (%s).", number, exc)
    track = facecrop.track_faces(podcast.path, clip.start, clip.duration,
                                 cfg.reframe, Path(cfg.paths.cache_dir),
                                 energy=energy)

    # 4. Subtitulos + rotulo de gancho.
    ass_name = None
    if cfg.subtitles.enabled:
        content = subtitles.build_ass(
            words=words,
            width=cfg.layout.width,
            height=cfg.layout.height,
            cfg_subs=cfg.subtitles,
            cfg_hook=cfg.hook,
            hook_text=meta.hook if meta else None,
            duration=plan.output_duration,
            cfg_title=cfg.title_card,
            title_text=metadata.card_text(
                podcast.title, meta.title if meta else "", cfg),
        )
        ass_name = f"short_{number}.ass"
        subtitles.write_ass(content, work_dir / ass_name)

    # 5. Punto de entrada del fondo, distinto para cada clip y con vuelta al
    #    principio si el gameplay se acaba.
    usable = max(background_duration - clip.duration, 1.0)
    offset = (number - 1) * clip.duration % usable

    output = output_dir / f"short_{number}.mp4"
    render(
        RenderRequest(
            podcast=podcast.path,
            background=background.path,
            start=clip.start,
            end=clip.end,
            background_offset=offset,
            plan=plan,
            track=track,
            ass_name=ass_name,
            output=output,
            work_dir=work_dir,
        ),
        cfg,
    )

    if getattr(cfg.output, "thumbnail", False):
        extract_thumbnail(output, getattr(cfg.output, "thumbnail_at", 1.2))

    _write_sidecar(output, clip, meta, podcast, plan)
    return output


def _write_sidecar(output: Path, clip: Window, meta: Optional[ClipMetadata],
                   podcast, plan) -> None:
    """Guarda los textos de publicacion junto al video."""
    data = {
        "video": output.name,
        "fuente": {
            "titulo": podcast.title,
            "url": podcast.info.get("webpage_url"),
            "inicio": round(clip.start, 2),
            "fin": round(clip.end, 2),
            "marca": hms(clip.start),
        },
        "duracion_final": round(plan.output_duration, 2),
        "silencio_eliminado": round(plan.removed, 2),
        "puntuacion": {
            "total": round(clip.score, 4),
            "heatmap": round(clip.heat, 4),
            "energia": round(clip.energy, 4),
            "densidad": round(clip.density, 4),
            "contexto": round(clip.context, 4),
            "contenido_llm": round(clip.llm_content, 4),
            "gancho_llm": round(clip.llm_hook, 4),
            "motivo": clip.reason,
        },
        "transcripcion": clip.text,
    }
    if meta:
        data["publicacion"] = asdict(meta)

    output.with_suffix(".json").write_text(
        json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    if meta:
        output.with_suffix(".txt").write_text(
            f"{meta.title}\n\n{meta.caption()}\n", encoding="utf-8"
        )


def _finish(cfg: Config, output_dir: Path) -> None:
    """Lo que se hace una vez, cuando ya estan todos los shorts."""
    if getattr(cfg.output, "csv", False):
        exports.write_csv(output_dir)


# ----------------------------------------------------------------------------
# Informes
# ----------------------------------------------------------------------------

def _report_selection(clips: List[Window]) -> None:
    log.info("=" * 68)
    log.info("%d momentos seleccionados:", len(clips))
    for index, clip in enumerate(clips, start=1):
        log.info("  %d. %s", index, clip.describe())
        if clip.reason:
            log.info("     %s", clip.reason)
        preview = clip.text[:110].replace("\n", " ")
        log.info("     \"%s...\"", preview)


def _report_results(results, total: int, elapsed: float) -> int:
    ok = [r for r in results if r[1] is not None]
    failed = [r for r in results if r[1] is None]

    log.info("=" * 68)
    log.info("RESUMEN: %d/%d shorts generados en %.0fs", len(ok), total, elapsed)
    for number, path, _ in ok:
        log.info("  OK  %s", Path(path).absolute())
    for number, _, error in failed:
        log.error("  XX  clip %d: %s", number, error)

    progress.result("finished", ok=len(ok), total=total)
    return 0 if ok else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\nInterrumpido.", file=sys.stderr)
        sys.exit(130)
