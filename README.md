# viralshorts

De un podcast de YouTube a shorts verticales subtitulados, en un solo comando.

```
podcast (YouTube)  ─┐   preselección ─► transcripción ─► puntuación ─► corte ─► render ─► short_N.mp4
                    ├─► (heatmap +      (solo las                                         short_N.txt
gameplay (YouTube) ─┘    energía)        zonas calientes)
```

Se puede usar con **interfaz visual** (`viralshorts.bat`) o por línea de
comandos. Las dos hacen exactamente lo mismo.

## Instalación

**Windows: doble clic en `viralshorts.bat`.** No hay más. La primera vez busca
(o instala) Python, crea un entorno virtual dentro de la carpeta, instala las
dependencias, descarga una copia portable de ffmpeg y abre la interfaz; las
siguientes veces arranca directamente. No necesita permisos de administrador y
no toca el Python ni el PATH del sistema.

Si algo falla, `install\Reparar instalacion.bat` lo rehace desde cero y
`install\log-instalacion.txt` dice qué pasó.

**GPU (opcional).** La transcripción usa la tarjeta NVIDIA si puede, y el
procesador si no. Para usar la tarjeta hacen falta cuBLAS y cuDNN, que no
vienen con `faster-whisper`: `install\Activar GPU NVIDIA.bat` los instala
(~600 MB) y `install\Comprobar GPU.bat` dice en cuál de los dos casos estás.
Sin ellos el pipeline no falla, solo va más lento.

Para **mandárselo a otra persona**: `Crear paquete para enviar.bat` deja en el
Escritorio un ZIP con solo lo necesario (sin `.venv`, sin vídeos, sin caché, y
sin tu clave de API). El que lo recibe descomprime y hace doble clic en
`viralshorts.bat`.

En macOS o Linux, a mano:

```bash
pip install -r requirements.txt
brew install ffmpeg                 # Linux: sudo apt install ffmpeg
```

Para la puntuación con Claude (opcional pero recomendada):

```bash
set ANTHROPIC_API_KEY=sk-ant-...    # PowerShell: $env:ANTHROPIC_API_KEY="sk-ant-..."
```

Sin clave el pipeline sigue funcionando: puntúa con heatmap + energía + densidad
y no genera títulos ni hashtags.

## Uso: interfaz visual (recomendado)

Doble clic en **`viralshorts.bat`**, o desde la consola:

```bash
python gui.py
```

Se abre el navegador con la interfaz, pensada para que se pueda usar sin haber
visto nunca el código. Son tres pasos numerados: el pódcast, el vídeo de fondo y
cómo lo quieres. Pulsas **Generar mis shorts** y ya está.

- **Nada de jerga en la superficie.** Eliges entre *Cortos / Normales / Largos*
  y entre *Rápido / Equilibrado / Máxima calidad*, con el tiempo que tarda cada
  uno escrito al lado. Detrás, esas dos elecciones rellenan el modelo de
  Whisper, el `beam_size`, las zonas del preescaneo y las duraciones. Todo eso
  sigue estando, pero en **Ajustes avanzados**, plegado.
- **Confirma lo que has pegado**: si el enlace es de YouTube aparecen la
  miniatura, **el título real del vídeo**, el canal y la duración, así que ves
  que es el que querías antes de esperar veinte minutos. El título lo consulta
  el servidor con yt-dlp sin descargar nada (`/api/title`), con medio segundo
  de espera para no preguntar en cada pulsación y una caché en memoria.
- **Avisa antes de fallar.** Si falta ffmpeg, la clave de Claude o la fuente de
  los subtítulos, lo dice nada más abrir, no a mitad de la transcripción. Y si
  Claude se cae a media ejecución, sale un aviso explicando que la selección
  continúa con el heatmap y la energía.
- **Los errores están traducidos.** «Sign in to confirm you are not a bot» se
  convierte en *YouTube no deja descargar ese vídeo* y te dice qué hacer. El
  texto crudo sigue disponible en «Ver el detalle técnico».
- **Progreso legible**: las cinco fases como una lista que se va marcando, no un
  nombre de etapa suelto.
- **Buscar y revisar antes de montar**: elige los momentos, te los enseña todos
  (el triple de los que pediste), y tú marcas cuáles se montan y corriges los
  títulos y los ganchos. Ver *[Revisar antes de montar](#revisar-antes-de-montar)*.
- **Cola**: si lanzas algo mientras hay otra cosa en marcha, se pone en cola en
  vez de fallar por el cerrojo de `work/`. Se montan uno detrás de otro.
- **Estilo de los subtítulos** con vista previa: color de la palabra activa,
  tamaño, altura en pantalla, palabras por línea y fuente, con una maqueta 9:16
  al lado que se actualiza al momento. Sin tocar `config.yaml`.
- **Velocidad**: calidad de descarga, shorts a la vez y uso de la tarjeta
  gráfica, en Ajustes avanzados.
- **Elegir archivo** abre el explorador de Windows para un fichero local.
- Lo que escribes se recuerda entre sesiones.

Al final aparecen **los vídeos reproducibles ahí mismo**, con su título y
hashtags y un botón para copiarlos.

Es un servidor local: solo escucha en `127.0.0.1` y exige un token que va en la
URL, así que ninguna otra página ni ningún otro equipo puede usarlo. Se apaga al
cerrar la ventana de consola.

## Uso: línea de comandos

```bash
# Lo normal
python pipeline.py --podcast <URL> --background <URL_gameplay>

# Ver qué momentos elegiría, sin renderizar (rápido y gratis)
python pipeline.py --podcast <URL> --background gameplay.mp4 --dry-run

# 5 clips, en inglés, sin Claude
python pipeline.py --podcast <URL> --background <URL> --clips 5 --language en --no-llm

# Cambiar cualquier ajuste sin tocar el YAML
python pipeline.py --podcast <URL> --background <URL> \
                   --set subtitles.font_size=80 --set output.hardware_encoder=off

# En dos tiempos: elegir ahora, revisar, y montar solo lo que valga
python pipeline.py --podcast <URL> --background <URL> --plan-out work/plans/hoy.json
python pipeline.py --from-plan work/plans/hoy.json --only 1,4,7
```

Tanto `--podcast` como `--background` aceptan una URL o un fichero local.
`python pipeline.py --help` lista todas las opciones.

Salida en `output/`, por cada clip:

| Fichero | Contenido |
|---|---|
| `short_N.mp4` | el vídeo listo para subir (1080x1920, H.264, audio a −14 LUFS) |
| `short_N.jpg` | fotograma de portada, para la miniatura |
| `short_N.txt` | título y pie con hashtags, para copiar y pegar |
| `short_N.json` | marca de tiempo en el original, puntuación de cada señal y transcripción |

Y uno para toda la carpeta:

| Fichero | Contenido |
|---|---|
| `publicaciones.csv` | título, gancho, descripción, hashtags, duración y minuto original de **todos** los shorts, para abrirlo en Excel y usarlo como calendario de publicación |

## Revisar antes de montar

Elegir los momentos cuesta minutos (descarga, transcripción, Claude) y
montarlos cuesta otros tantos. Antes las dos cosas iban pegadas: para descartar
un clip flojo o corregir un título había que repetirlo **todo**.

Ahora se pueden separar. **Buscar y revisar antes de montar** guarda un *plan*
en `work/plans/` con la selección: los candidatos —el triple de los que pediste,
por defecto—, sus transcripciones, las palabras con sus tiempos y la
configuración completa con la que se eligieron. Encima de eso trabaja la
interfaz:

- desmarcas los momentos que no te convencen,
- corriges el título y el gancho (con el contador de caracteres que de verdad
  admite cada plataforma: 70 y 45),
- cambias el estilo de los subtítulos,
- y **Montar los marcados** renderiza solo esos.

El montaje desde un plan no vuelve a descargar, ni a transcribir, ni a pagar a
Claude: todo eso ya está dentro. Cada short montado trae además un botón
**Rehacer este**, que vuelve a montar ese clip solo —con el título nuevo, o con
otro color de subtítulo— en el tiempo que tarda un render.

Por línea de comandos son las opciones `--plan-out`, `--propose`, `--from-plan`
y `--only`.

## Qué hace, y por qué

### 1. Elegir el momento

La densidad de palabras (lo que usaba el script anterior) mide **quién habla más
rápido**, no qué engancha. Se sustituye por cuatro señales combinadas y
normalizadas entre sí:

| Señal | Peso | De dónde sale |
|---|---|---|
| **Heatmap** | 0.30 | El *most replayed* de YouTube, vía yt-dlp. Es la audiencia real diciéndote qué rebobina. La mejor señal barata que existe. |
| **Claude** | 0.35 + 0.15 | Lee la transcripción y puntúa contenido y fuerza del gancho como un editor de shorts. **Opcional**: con `--no-llm` el resto de señales se reparten su peso. |
| **Energía** | 0.15 | Curva RMS del audio: risas, subidas de voz, discusiones. |
| **Contexto** | 0.25 | Si el clip **se entiende solo**, sin haber visto el resto. Ver abajo. |
| **Densidad** | 0.05 | Solo para descartar tramos con silencios largos. |

Los pesos están en `config.yaml`. Una señal que no aporta información (un vídeo
sin heatmap, o Claude desactivado) se ignora y su peso se reparte entre las
demás, en lugar de contar como cero.

Para no gastar tokens de más, Claude solo ve las ~24 mejores ventanas según las
señales baratas, y sin solaparse entre sí.

**Los cortes caen siempre en frontera de frase**, nunca a mitad de palabra.

**Y no salen dos clips del mismo tema.** `clips.min_gap` separa en el tiempo,
pero dos momentos a media hora de distancia pueden ser la misma anécdota
contada dos veces. Se comparan los vocabularios de los clips ya elegidos
(coseno sobre las palabras con contenido, sin muletillas) y el segundo que pase
de `scoring.max_similarity` se descarta.

La llamada a Claude va **en lotes** de 10 fragmentos, no todos de golpe: con
dos docenas de transcripciones en una sola petición la respuesta se corta por
`max_tokens` y los últimos fragmentos se quedan sin nota —y sin nota no
compiten—. Un 429 o un «overloaded» se reintenta con espera creciente, y si aun
así falla, la selección sigue con las demás señales en vez de abortar.

### 2. Montar el vídeo

Todo en **una sola pasada de ffmpeg** — el script anterior renderizaba con
moviepy y volvía a codificar para quemar los subtítulos, con doble pérdida:

- **Jump cuts**: se recortan los silencios de más de 0,35 s. Las marcas de
  tiempo de los subtítulos se recalculan para seguir cuadrando.
- **Reencuadre siguiendo la cara** (YuNet): en un podcast a dos cámaras, el
  recorte centrado parte a los invitados. Aquí se detecta la cara cada 0,5 s y
  se construye una trayectoria suavizada del encuadre.
- **Las caras que no son personas se descartan.** Un plató tiene pósters,
  cuadros y miniaturas con caras dentro, y el detector las ve igual que a un
  invitado: el encuadre se va a la foto y deja fuera a quien está hablando.
  Se separan por lo único que las distingue: **la persona se mueve, la foto
  no**. Se mide el cambio de luminosidad dentro de cada caja entre muestras y
  se agrupa por posición, mirando el movimiento *típico* de cada grupo a lo
  largo del clip y no el de un fotograma suelto — un invitado callado también
  se queda quieto un segundo, pero no un minuto. Medido en el pódcast de
  prueba, con una foto grande superpuesta: el encuadre pasa de `cx 0.11` (la
  foto) a `cx 0.58` (quien habla). Si TODAS las caras están quietas (el vídeo
  corta a una imagen a pantalla completa) no se descarta ninguna: mejor
  encuadrar la foto que perder el reencuadre entero.
- **Se sigue a quien habla, no a quien escucha.** Con dos personas en plano, ir
  a la cara más grande (o a la más cercana al encuadre actual) deja el vídeo
  clavado en el que está callado durante minutos. Se mide el movimiento del
  **tercio inferior** de cada cara —la cara entera se mueve también al asentir
  o al reírse; la boca solo al hablar— y se cruza con la curva de energía del
  audio: en los silencios el encuadre se queda donde está. Para cambiar de
  persona, la otra tiene que ganar por un margen (`switch_margin`) y
  mantenerlo varias muestras seguidas (`switch_hold`), que es lo que evita el
  ping-pong entre invitados. Se apaga con `reframe.speaker_tracking: false`.
- **Nada se estira nunca**: pasar de 16:9 a vertical se hace **recortando**, no
  encajando. Del original se toma la mayor ventana que ya tiene la proporción
  del panel, y el escalado posterior usa
  `force_original_aspect_ratio=increase` + `crop`, que cubre el panel sin
  deformar y se come el sobrante. Se contempla también el vídeo con píxeles no
  cuadrados (SAR ≠ 1): se normaliza a píxeles cuadrados antes de medir nada,
  porque si no la geometría del recorte no es la que se ve.
- **Tarjeta de contexto** al arrancar: los primeros 2,5 s aparece **en el
  centro del vertical** —justo en la costura entre el panel del pódcast y el
  del gameplay, que es donde primero mira el ojo— el nombre del programa y el
  invitado, con el mismo estilo que los subtítulos y el mismo rebote de
  entrada. Se lee y se va.

  El texto sale de `title_card.source`: `show` (por defecto) toma el título del
  vídeo de origen sin la lista de temas del episodio —«The Wild Project #375 -
  Xokas | ¿Por qué no peleó…» se queda en programa e invitado, en dos líneas—,
  `video` usa el título entero y `clip` vuelve a la frase extraída del clip.
  Se eligió el título del vídeo y no una frase del clip por lo dicho más abajo:
  una frase suelta de una conversación casi nunca funciona como titular,
  mientras que el nombre del programa siempre está bien escrito y dice al que
  llega de golpe qué está viendo.

  Nunca coincide en pantalla con el gancho: el gancho empieza justo cuando la
  tarjeta se va.
- **Punch-in**: zoom lento del 0 al 7 % para que el plano no se quede muerto.
  Va *después* del recorte, sobre una imagen que ya tiene la proporción buena.
  El orden importa: `zoompan` recorta con la proporción de su entrada y escala
  a la de su salida sin respetar ninguna de las dos, así que darle el
  fotograma 16:9 entero y pedirle un panel de 1080x960 aplasta la imagen al
  63 % de su anchura. Además `zoompan` fuerza `z >= 1`, de modo que en ese
  montaje el punch-in y el paneo de cara quedaban anulados sin avisar.
- **Fondo en bucle infinito**: da igual que el gameplay sea más corto que la
  suma de los clips, y cada clip entra por un punto distinto.
- **Audio a −14 LUFS** (`loudnorm`): sin esto tus clips suenan más bajos que el
  resto del feed y se perciben como peores sin que nadie sepa por qué.

### 1b. Que el clip se entienda solo

El heatmap dice **dónde** está lo bueno; no dice dónde **empieza**. Un recorte
puede caer en el minuto más rebobinado del pódcast y aun así ser inservible
porque arranca en «…y entonces eso es justo lo que le pasó a él».

`viralshorts/context.py` puntúa lo autocontenido que es el arranque de cada
ventana candidata, con tres señales y sin modelo de lenguaje:

- **La primera palabra en minúscula.** Whisper pone mayúscula al empezar frase,
  así que una minúscula inicial significa que el clip arranca literalmente a
  mitad de una frase. Es la señal más barata y la más fiable de las tres.
- **Fronteras de tema** (TextTiling). Dos tramos seguidos que comparten
  vocabulario hablan de lo mismo; cuando el vocabulario cambia de golpe, ahí ha
  empezado un tema nuevo. Se comparan bloques de 12 s a cada lado por
  similitud del coseno y se marcan los máximos locales.
- **Arranque anafórico.** «Eso», «por eso», «como te decía», «pero», «y
  entonces»: palabras que se refieren a algo que el espectador no ha visto.
  Empezar preguntando, en cambio, suma.
- Y se comprueba que **el asunto del clip se nombre pronto**: si las palabras
  de las que más se habla no aparecen hasta la mitad, el arranque no dice de
  qué va.

En el pódcast de prueba el efecto es directo:

```
antes   "la batalla de Trafalgar y todo eso ha sido hace relación..."   (a media frase)
ahora   "La gente tiene miedo a hablar. Claro. Por eso no hablan..."     (empieza por el principio)
```

Se paga con algo de heatmap (0.76 → 0.61 en ese clip): es el precio de que se
entienda. Ajustable con `scoring.weight_context`, o `--no-context` para volver
al criterio anterior.

### 2b. Los textos de publicación, con Claude o sin él

Título, gancho, descripción y hashtags salen siempre, haya o no modelo de
lenguaje. Son dos caminos con una diferencia importante:

- **Con Claude** los textos están **redactados**: lee el clip y escribe un
  titular nuevo.
- **Sin Claude** (`viralshorts/metadata.py`) están **extraídos**: se elige la
  mejor frase que el invitado ya ha dicho y se usa tal cual. Nada de
  plantillas tipo «Descubre el secreto de…», que es lo que sale al intentar
  redactar sin entender el contenido; una frase real del clip siempre gana.

> **El límite de esto, dicho claro.** La extracción ordena bien las frases —
> descarta las que divagan, las autorreferentes y las que no hablan del tema —
> pero no distingue «se entiende como titular» de «frase gramatical con
> palabras del tema». Elegirá una frase decente, no la mejor. Escribir un
> titular de verdad exige entender el clip, y eso es lo que hace Claude. Por
> eso hay una nota mínima y un respaldo: cuando ninguna frase da la talla,
> mejor el nombre del programa que una frase rara.

Cómo elige la frase, que es donde está todo el trabajo:

- El **gancho** solo puede salir de los primeros 12 segundos: prometer algo que
  se dice al final del clip es engañar al espectador. Entre esas frases gana la
  que tenga interrogación, cifras o un arranque fuerte («te lo juro», «nunca»),
  y pierde la que empiece con muletilla («bueno», «o sea», «entonces»). Se
  penaliza fuerte la que no quepa entera en el rótulo: un gancho cortado a
  media frase no promete nada.
- El **título** premia densidad de palabras con significado, cifras y preguntas.
- El **título** (el que va en la tarjeta de contexto) se elige sobre todo por
  **cobertura del tema**: cuántas de las palabras clave del clip contiene. Una
  frase impecable que no informa de nada («La capital era espectacular») pierde
  contra una peor que sí nombre el asunto. Medir la *proporción* de palabras
  con significado no sirve: premia a la frase corta y vacía, en la que hay
  pocas palabras y todas cuentan.
- Si **ninguna** frase del clip llega a la nota mínima como título
  (`metadata.min_title_score`), la tarjeta usa la identidad del programa
  —«The Wild Project · Xokas», sacada del título del vídeo de origen— en vez de
  una frase suelta. No dice de qué va el clip, pero es información verdadera y
  se ve intencionada; una tarjeta que pone «Hago mis pinitos y lo que me gusta
  es aprender» parece un error del programa.
- Si la mejor frase de apertura no llega a la nota mínima
  (`metadata.min_hook_score`), **no se pone gancho**: un rótulo malo tapa medio
  plano sin decir nada, y la tarjeta de título se queda sola haciendo el
  trabajo. La interrogación solo puntúa si hay algo que preguntar, porque las
  transcripciones producen bastantes «¿eh?».
- Los **hashtags** son las palabras más repetidas del clip quitando muletillas y
  formas verbales conjugadas (`#estaban` no lo busca nadie), más el nombre del
  programa como un único tag (`#thewildproject`, no `#wild` + `#project`).
- Al truncar nunca se deja un artículo colgando: «…se quitaron las…» se corta
  en «…se quitaron…».

Ejemplo real, sin tocar la API:

```
GANCHO  TE LO JURO, QUE LO MATABAN CON GUILLOTINA.
TÍTULO  A finales de 1980 o 1970 se quitaron las guillotinas.
TAGS    #wildproject #guillotina #imperio #romano
```

Los dos caminos se combinan: si Claude contesta a medias, lo que falte se
rellena por extracción en vez de salir vacío. Con `--no-metadata` no se escribe
ninguno de los dos.

### 3. Subtítulos

Formato .ASS quemado con libass, palabra por palabra:

- **Sin parpadeo.** Cada línea dura hasta que arranca la palabra siguiente, no
  hasta que acaba la actual. Con lo segundo, el hueco de 50-300 ms entre
  palabras deja la pantalla vacía y el subtítulo titila.
- La palabra activa entra un 18 % más grande y vuelve a su tamaño en 120 ms.
- **Rótulo de gancho** arriba durante los primeros 4 segundos, escrito por
  Claude a partir del contenido del clip.
- Censura opcional de palabrotas (`subtitles.censor_words`).

## Estructura

| Fichero | Qué es |
|---|---|
| `viralshorts.bat` | único punto de entrada: instala si hace falta y abre la interfaz |
| `install/setup.ps1` | el instalador (Python, entorno virtual, dependencias, ffmpeg) |
| `install/empaquetar.ps1` | genera el ZIP que se le manda a otra persona |
| `gui.py` | servidor local de la interfaz |
| `web/index.html` | la propia interfaz |
| `pipeline.py` | el pipeline por línea de comandos |
| `viralshorts/` | los módulos: descarga, preselección, transcripción, puntuación, textos, corte, render, plan, exportación |
| `tests/` | pruebas sin ffmpeg ni red (`python -m unittest discover -s tests -t .`) |
| `config.yaml` | todos los ajustes |

## Configuración

Todo está en `config.yaml`; solo hace falta escribir lo que quieras cambiar. Los
valores por defecto y su documentación están en `viralshorts/config.py`.

Ajustes que más se tocan:

```yaml
clips:
  num_clips: 3
  min_duration: 25       # 25-45s es el punto dulce actual de Shorts/Reels
  max_duration: 45
transcribe:
  model_size: medium     # large-v3 si tienes GPU
  device: auto           # detecta CUDA solo
  beam_size: 1           # greedy: el doble de rápido, calidad casi idéntica
prescan:
  enabled: true          # transcribir solo las zonas calientes (ver Rendimiento)
  regions: 0             # 0 = auto. Bájalo a 8 para ir aún más rápido
zoom:
  enabled: false         # desactívalo para renderizar bastante más rápido
output:
  hardware_encoder: auto # usa la GPU que haya (NVIDIA, Intel o AMD)
  render_workers: 0      # 0 = auto; shorts montándose a la vez
```

Cualquier ajuste se puede cambiar también desde la línea de comandos sin tocar
el fichero, con `--set clave=valor`:

```bash
python pipeline.py ... --set subtitles.color_active=#00E676 --set clips.min_gap=120
```

### Fuentes

libass usa las fuentes **instaladas en el sistema**, no un `.ttf` suelto. Si
pones `font_name: Montserrat Black`, instálala antes (doble clic → Instalar).
`Arial Black` viene con Windows y macOS; en Linux usa `DejaVu Sans Bold`.
También puedes apuntar `subtitles.fonts_dir` a una carpeta con tus `.ttf`.

### 4. Palabrotas

En los subtítulos se tapa **una letra** de cada palabrota: la primera vocal de
la raíz. «puta» → «p#ta», «gilipollas» → «gilip#llas». Se sigue leyendo de un
vistazo (el clip no pierde la gracia) pero el texto deja de ser literal para
los filtros automáticos de las plataformas. **El audio no se toca**, y la
tarjeta de título y el gancho pasan por el mismo filtro.

La lista son **raíces**, no palabras, así que «puta», «putas», «putada» y
«putazo» caen las cuatro con una sola entrada. Lo delicado es no pasarse:
buscar «put» dentro de la palabra convertiría «reputación» en «rep#tación», así
que una raíz solo cuenta si la palabra **empieza** por ella, o por uno de unos
pocos refuerzos (`hijo-`, `gili-`). Hay además una lista corta de excepciones
para lo que se cuela igual («follaje»). Con `censor_words` añades las tuyas, y
se tratan también como raíces.

## Una ejecución a la vez

`work/` está protegido con un cerrojo: si lanzas una segunda ejecución sobre la
misma carpeta (por ejemplo la interfaz y la consola a la vez) la segunda falla
en el primer segundo con un mensaje claro, en lugar de pelearse por los mismos
ficheros y romper una descarga a medias. Si de verdad quieres dos en paralelo,
dale a cada una su `paths.work_dir`.

Desde la interfaz no hace falta pensar en esto: lo que lances mientras hay algo
en marcha **se pone en cola** y arranca solo cuando la anterior suelta el
cerrojo. Y **Detener** mata el proceso entero, ffmpeg incluido: antes se
quedaba codificando de fondo.

## Caché

- Los vídeos descargados se reutilizan (`work/`). Si el vídeo está pero faltan
  sus metadatos, se vuelven a pedir **sin** volver a descargarlo.
- Las transcripciones se cachean por fichero + modelo + idioma + zonas
  preseleccionadas (`work/cache/`). Volver a lanzar el pipeline con otros pesos
  **no** vuelve a transcribir; cambiar las duraciones sí, porque cambian las
  zonas.
- **Las puntuaciones y los textos de Claude** también (`work/cache/llm.json`),
  con la clave del modelo y el esfuerzo: relanzar el mismo vídeo no vuelve a
  pagar por los mismos fragmentos, y cambiar de modelo sí los pide otra vez.
- **Las trayectorias del reencuadre** (`work/cache/reframe_*.json`). Es lo que
  hace que «Rehacer este short» tarde segundos: la detección de caras no se
  repite.
- Para forzar de nuevo todo, borra `work/`.

## Rendimiento

Lo más lento es transcribir, así que el pipeline **no transcribe el podcast
entero**. El heatmap de YouTube y la curva de energía del audio no necesitan
texto y se calculan sobre las tres horas en unos segundos: con esas dos señales
ya se sabe dónde están los momentos calientes, y Whisper solo se pasa por esas
zonas (`prescan`). El texto sigue estando donde importa —elegir el corte exacto
en frontera de frase y subtitular—, pero se transcribe un 15-20 % del vídeo en
vez del 100 %.

Medido en un i5-12400F (6 núcleos, sin GPU CUDA), podcast de 2h26m, modelo
`medium`:

| | Antes | Ahora |
|---|---|---|
| Audio transcrito | 2h26m (todo) | 22m15s (12 zonas) |
| Velocidad de Whisper | 1.35x tiempo real | 3.4x tiempo real |
| Transcripción | 108 min | 6.5 min |
| **Total del pipeline** | **115 min** | **10 min** |

Los tres clips elegidos son los mismos que con la transcripción completa
(mismos momentos ±6 s), que era el objetivo: ir 11 veces más rápido sin cambiar
lo que sale.

De dónde sale cada cosa:

- **`prescan`** (5x): solo se transcriben las zonas calientes. Es la mejora
  grande. Se desactiva con `--full-transcript` o el interruptor
  «Transcripción rápida» de la interfaz.
- **`beam_size: 1`** (2x): búsqueda greedy en vez de beam de 5. Para elegir
  momentos y poner subtítulos la diferencia de calidad es despreciable. Sube a
  5 con `--beam 5` si quieres la transcripción más fina posible.
- **`cpu_threads`** (1.4x): faster-whisper usa 4 hilos por defecto, lo que en
  un procesador de 6 núcleos deja media CPU parada. Ahora usa los núcleos
  físicos.

Y después de eso, lo que se ha ido ganando en el resto del recorrido:

- **Descarga con 8 fragmentos en paralelo** (`download.concurrent_fragments`).
  YouTube sirve los vídeos largos troceados y yt-dlp los pedía de uno en uno:
  la descarga tardaba mucho más que la conexión. Con `download.max_height: 720`
  se baja a la mitad de datos, a cambio de un pelo de nitidez en el panel de
  arriba.
- **Transcripción por lotes** (`transcribe.batched`, faster-whisper ≥ 1.1):
  2-4x más rápido con el mismo modelo y las mismas marcas por palabra.
- **Codificación por GPU detectada sola** (`output.hardware_encoder: auto`):
  prueba NVENC, QSV y AMF *codificando un fotograma de verdad*, porque ffmpeg
  los lista aunque no haya tarjeta. Si ninguno funciona, libx264.
- **Varios shorts a la vez** (`output.render_workers`, 0 = auto): ffmpeg deja
  la CPU a medio gas en las partes serie del filtrado, y ese hueco lo llena el
  otro clip.
- **Muestreo del reencuadre sin decodificar de más**: los fotogramas que no se
  miran se saltan con `grab()` en vez de decodificarlos enteros. Con una
  muestra cada medio segundo, eso son 14 de cada 15 fotogramas.
- **Cachés**: transcripción, puntuaciones de Claude y trayectorias de
  reencuadre. Volver a montar un clip ya elegido es solo el render.

Otras palancas si sigue siendo mucho:

- `--regions 8` (menos zonas que mirar, algo menos de variedad al elegir).
- `--model small` (3x más rápido que `medium`; en español se nota poco).
- `--no-zoom` acelera el render.
- Con GPU NVIDIA (`device: cuda`) la transcripción baja a segundos. Una AMD no
  sirve: ctranslate2 solo tiene CUDA.

El render son 1-3 minutos por clip con `zoom` activado, y bastante menos sin él.

## Pruebas

```bash
python -m unittest discover -s tests -t .
```

39 pruebas, menos de un segundo, sin tocar ffmpeg, YouTube ni la API. Cubren
justo lo que falla **en silencio**: los tiempos (jump cuts, remapeo de
palabras, tiempos del `.ASS`, expresiones de recorte de ffmpeg), la elección de
clips (ventanas, hueco mínimo, deduplicación por tema), la configuración y los
`--set`, el plan de montaje, y los lotes y la caché de las llamadas al modelo.
Un fallo ahí no rompe nada: saca un short con buena pinta y los subtítulos
medio segundo desplazados.

## Aviso legal

Montar un podcast ajeno con gameplay ajeno y subirlo entra de lleno en Content
ID y en las políticas de contenido no original de YouTube y TikTok. Si es tu
propio contenido, o tienes permiso, sin problema; si no, cuenta con
reclamaciones y con posibles retiradas.

## Historial

Este pipeline sustituye a los antiguos `main.py` (descarga + selección por
densidad de palabras + montaje con moviepy) y `sub.py` (subtítulos .ASS por
lotes), ya eliminados.
