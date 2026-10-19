# Viral Shorts por dentro

> Esta es la documentación técnica: cómo elige los momentos, cómo monta el vídeo,
> la configuración y el rendimiento. Para instalar y usar la aplicación, ve al
> [README](../README.md).

De un podcast de YouTube a shorts verticales subtitulados, en un solo comando.

```
podcast (YouTube)  ─┐   preselección ─► transcripción ─► puntuación ─► corte ─► render ─► short_N.mp4
                    ├─► (heatmap +      (solo las                                         short_N.txt
gameplay (YouTube) ─┘    energía)        zonas calientes)
```

Es una **aplicación de escritorio** (ventana propia, icono, instalador) y
también se puede usar por línea de comandos. Las dos hacen exactamente lo mismo.

## Instalación

**Windows: ejecuta `viralshorts-setup-<versión>.exe`.** Se instala como
cualquier programa (menú Inicio, acceso directo en el Escritorio si quieres,
desinstalador en «Aplicaciones instaladas»), para tu usuario y sin permisos de
administrador. Después, abre **Viral Shorts** desde el menú Inicio o el
Escritorio: es una ventana de escritorio, sin consola ni ficheros `.bat`.

La **primera vez** que se abre, una pantalla de progreso descarga las tres
herramientas que usa como ejecutables —ffmpeg, yt-dlp y whisper.cpp, unos
120 MB— y después abre la interfaz; las siguientes veces abre al momento. Los
modelos (voz y caras) los baja el propio programa la primera vez que los
necesita. Todo se guarda en `%LOCALAPPDATA%\viralshorts`, los shorts en
`Vídeos\viralshorts` (se puede cambiar en `config.yaml`, `output.dir`), y no se
toca el PATH del sistema. Hace falta el *WebView2 Runtime* de Microsoft, que ya
trae Windows 11 y las versiones recientes de Windows 10; si falta, el programa
se abre en el navegador.

Al **desinstalar**, pregunta si quieres borrar también las herramientas y los
modelos descargados (varios cientos de MB). Tus shorts no se tocan.

**GPU (opcional).** La transcripción usa la tarjeta NVIDIA si puede, y el
procesador si no. Si el programa detecta una NVIDIA sin el motor de GPU,
muestra un aviso con el botón **Activar la GPU** (descarga ~700 MB, una sola
vez). Sin él todo funciona igual, solo más despacio. Si la tarjeta falla a mitad
de la transcripción, se repite entera en el procesador.

`viralshorts.exe doctor` (en una terminal) muestra qué herramientas ve el
programa, si hay GPU y la configuración.

Para **fabricar el instalador** (hacen falta [Rust](https://rustup.rs) e
[Inno Setup](https://jrsoftware.org/isdl.php)):

```powershell
powershell -ExecutionPolicy Bypass -File install\crear-instalador.ps1
```

Compila el programa y deja `dist\viralshorts-setup-<versión>.exe` (~9 MB). Es lo
único que hay que pasarle a otra persona.

Para **compilar y probar sin instalador**:

```bash
cd rust
cargo build --release      # deja rust/target/release/viralshorts.exe
cargo test                 # pruebas
```

Ese `viralshorts.exe` funciona suelto: si al lado tiene `config.yaml` o `bin/`
(como en esta carpeta de desarrollo) usa esa carpeta como sitio de datos; si no,
usa `%LOCALAPPDATA%\viralshorts`.

En macOS o Linux el código compila igual, pero el instalador y la ventana son de
Windows: hay que dejar a mano `ffmpeg`, `yt-dlp` y `whisper-cli` en el PATH (o en
`bin/`) y usar `viralshorts gui`, que abre la interfaz en el navegador.

Para la puntuación con Claude (opcional pero recomendada):

```bash
set ANTHROPIC_API_KEY=sk-ant-...    # PowerShell: $env:ANTHROPIC_API_KEY="sk-ant-..."
```

Sin clave el pipeline sigue funcionando: puntúa con heatmap + energía + densidad
y no genera títulos ni hashtags.

## Actualizaciones

Quien ya lo tiene instalado recibe las versiones nuevas sin hacer nada: unos
segundos después de abrir la aplicación se mira si hay una versión mayor y, si
la hay, sale un aviso con sus notas y el botón **Actualizar ahora**. La
aplicación descarga el instalador, comprueba que no se ha corrompido (SHA-256),
lo ejecuta en silencio, se cierra, y el instalador la vuelve a abrir ya
actualizada. No se actualiza con un trabajo en marcha. Abajo a la izquierda, el
número de versión (`v1.0.0`) es un botón para **buscar actualizaciones** a mano
(también está en Ajustes).

Funciona igual que Arcia: el código vive en un repositorio privado
(`practicas-hugosanz/viralshorts`) y cada versión se publica como *release* en
uno público solo de publicaciones
([`viralshorts-releases`](https://github.com/practicas-hugosanz/viralshorts-releases)),
con el instalador y un `latest.json` (versión, notas, dirección y huella). La
aplicación lee ese `latest.json` de la última release.

**Para publicar una versión nueva** (hace falta `gh` con sesión iniciada):

```powershell
powershell -ExecutionPolicy Bypass -File install\publicar.ps1 -Version 0.2.0 -Notas "Qué cambia"
```

Pone la versión en `rust\Cargo.toml`, compila el instalador, escribe
`latest.json`, guarda y etiqueta el cambio (`v0.2.0`) en el repositorio de código
y crea la release en el de publicaciones. Con `-SoloPreparar` hace todo menos
subir nada. Para probar las actualizaciones sin publicar, `VIRALSHORTS_UPDATE_URL`
apunta la aplicación a otro `latest.json`.

## Uso: interfaz visual (recomendado)

Abre **Viral Shorts** (menú Inicio o Escritorio). Si prefieres el navegador:

```bash
viralshorts gui              # --port 8760  --no-browser
```

La ventana tiene una barra lateral con cuatro secciones:

- **Crear**: el pódcast, el vídeo de fondo, cuántos shorts quieres, su duración y
  la calidad del análisis, y el estilo de los subtítulos. A la derecha, un
  **monitor 9:16** que se actualiza al momento (color, tamaño, altura, palabras
  por línea y fuente; la palabra que suena va cambiando de color) y, debajo,
  **Generar mis shorts** o **Buscar y revisar antes de montar**.
- **En curso**: las cinco fases como una lista que se va marcando, con barra de
  progreso, la cola de trabajos, el detalle técnico plegado y, si pediste revisar,
  los momentos para elegir y corregir antes de montar.
- **Mis shorts**: todos los vídeos generados, reproducibles ahí mismo, con su
  gancho, título y texto, y botones para copiarlo, rehacer un short o abrir la
  carpeta.
- **Ajustes**: lo que normalmente no hace falta tocar (Claude, búsqueda rápida,
  seguir la cara, silencios, zoom, límites de tiempo, modelo de transcripción,
  rendimiento, carpeta de guardado) y la comprobación de actualizaciones. Sigue
  el tema claro u oscuro de Windows.

Detalles que ahorran sustos:

- **Nada de jerga en la superficie.** Eliges entre *Corto / Normal / Largo* y
  entre *Rápida / Equilibrada / Máxima*, con el tiempo que tarda cada una. Detrás,
  esas elecciones rellenan el modelo de Whisper, el `beam_size`, las zonas del
  preescaneo y las duraciones, que siguen visibles en Ajustes.
- **Confirma lo que has pegado**: si el enlace es de YouTube aparecen la
  miniatura, **el título real del vídeo**, el canal y la duración. El título lo
  consulta el servidor con yt-dlp sin descargar nada (`/api/title`).
- **Avisa antes de fallar.** Si falta ffmpeg, la clave de Claude o la fuente de
  los subtítulos, lo dice nada más abrir. Y si Claude se cae a media ejecución,
  sale un aviso explicando que la selección continúa con el heatmap y la energía.
- **Los errores están traducidos.** «Sign in to confirm you are not a bot» se
  convierte en *YouTube no deja descargar ese vídeo* y te dice qué hacer. El
  texto crudo sigue en «Ver el detalle técnico».
- **Revisar antes de montar**: te enseña todos los momentos (el triple de los que
  pediste) y tú marcas cuáles se montan y corriges títulos y ganchos. Ver
  *[Revisar antes de montar](#revisar-antes-de-montar)*.
- **Cola**: si lanzas algo mientras hay otra cosa en marcha, se pone en cola en
  vez de fallar por el cerrojo de `work/`.
- **Elegir archivo** abre el explorador de Windows. Lo que escribes se recuerda
  entre sesiones.
- **Vista previa real**: en cuanto eliges el pódcast y el fondo, el monitor
  muestra un fotograma de cada uno (la miniatura en YouTube, un fotograma sacado
  con ffmpeg si es un archivo).
- **Modelos de voz**: si el análisis elegido necesita un modelo que aún no está
  (`medium` pesa 1,5 GB), la pantalla *Crear* lo avisa y lo descarga con su
  barra de progreso *antes* de lanzar, no a mitad del trabajo.
- **Perfiles**: *Guardar como perfil* (arriba a la derecha de *Crear*) guarda
  cantidad, duración, calidad, subtítulos y todo lo de Ajustes con un nombre, y
  el desplegable los recupera. Se guardan en `profiles.json`.
- **Cola**: cada trabajo en espera se puede adelantar, atrasar o quitar, y
  *Saltar este* para solo el que está en marcha sin vaciar la cola.
- **Mis shorts** enseña todas las ejecuciones (cada una vive en su subcarpeta),
  con búsqueda por título o texto y filtro por ejecución. Cada short se puede
  copiar, rehacer, **cambiarle el título** (queda en el `.json` y en
  `publicaciones.csv`), abrir su carpeta o **borrar** (con confirmación).
- **Cookies del navegador**: si YouTube pide iniciar sesión, el aviso ofrece
  reintentar con las cookies de Firefox, Edge o Chrome (`download.cookies_from_browser`
  de yt-dlp). También está en Ajustes. Con Chrome y Edge puede fallar mientras
  están abiertos.
- **Acertar más con el texto** (Ajustes): *afinar los clips elegidos* vuelve a
  transcribir solo el trozo de cada clip con un modelo mayor (`large-v3-turbo`
  por ejemplo: son decenas de segundos por clip, no el pódcast entero; la calidad
  *Máxima* lo activa). Una prueba con `tiny` + afinado con `small` pasó de
  «cuando compártelo tiempo» a «cuando compartes tiempo». Además, *nombres y
  palabras difíciles* (va como `--prompt` de whisper) y *correcciones fijas*
  (`mal = bien`, una por línea, sin importar mayúsculas).
- **Transcripción en directo**: mientras whisper trabaja, *En curso* va enseñando
  cada frase con su minuto del pódcast (la lee del stdout de `whisper-cli`).
- **Actualizaciones**: además de al abrir, la aplicación vuelve a mirar cada 3 horas.
- **Subtítulos que caben**: un bloque de palabras largas se parte en dos líneas y,
  si ni así cabe (una sola palabra enorme), se reduce el tamaño solo en ese bloque.

Es un servidor local: solo escucha en `127.0.0.1` y exige un token que va en la
URL, así que ninguna otra página ni ningún otro equipo puede usarlo. Se apaga al
cerrar la ventana de consola.

## Uso: línea de comandos

```bash
# Lo normal
viralshorts --podcast <URL> --background <URL_gameplay>

# Ver qué momentos elegiría, sin renderizar (rápido y gratis)
viralshorts --podcast <URL> --background gameplay.mp4 --dry-run

# 5 clips, en inglés, sin Claude
viralshorts --podcast <URL> --background <URL> --clips 5 --language en --no-llm

# Cambiar cualquier ajuste sin tocar el YAML
viralshorts --podcast <URL> --background <URL> \
            --set subtitles.font_size=80 --set output.hardware_encoder=off

# En dos tiempos: elegir ahora, revisar, y montar solo lo que valga
viralshorts --podcast <URL> --background <URL> --plan-out work/plans/hoy.json
viralshorts --from-plan work/plans/hoy.json --only 1,4,7

# Utilidades
viralshorts doctor           # qué herramientas ve, GPU, configuración
viralshorts config           # la configuración efectiva (YAML + --set)
```

Tanto `--podcast` como `--background` aceptan una URL o un fichero local.
`viralshorts --help` lista todas las opciones.

Salida en `output/<titulo-del-podcast>_<fecha>/` (una carpeta por ejecución, así
un podcast nuevo no pisa los shorts del anterior; `--set output.run_subfolder=off`
lo deja todo en `output/`). Por cada clip:

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

`rust/src/context.rs` puntúa lo autocontenido que es el arranque de cada
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
- **Sin Claude** (`rust/src/metadata.rs`) están **extraídos**: se elige la
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
| `install/viralshorts.iss` | el instalador (Inno Setup): instala el `.exe`, accesos directos y desinstalador |
| `install/crear-instalador.ps1` | compila el programa y fabrica `dist\viralshorts-setup-<versión>.exe` |
| `rust/` | el programa: un crate de Rust que compila a `viralshorts.exe` |
| `rust/src/app.rs` | la ventana de escritorio (WebView2) |
| `rust/src/setup.rs` | el primer arranque: descarga ffmpeg, yt-dlp y whisper.cpp con una pantalla de progreso |
| `rust/src/gui.rs` | servidor local de la interfaz |
| `web/index.html` | la propia interfaz (se incrusta en el ejecutable al compilar) |
| `rust/src/pipeline.rs` | el pipeline y la línea de comandos |
| `rust/src/*.rs` | los módulos: descarga, preselección, transcripción, puntuación, textos, corte, render, plan, exportación |
| `rust/tests/` | pruebas con ffmpeg, yt-dlp, whisper.cpp y YuNet de verdad (se saltan si falta algo) |
| `config.yaml` | todos los ajustes |
| `bin/` | herramientas y modelos descargados (no se versiona) |

## Configuración

Todo está en `config.yaml`; solo hace falta escribir lo que quieras cambiar. Los
valores por defecto y su documentación están en `rust/src/config.rs`.

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
viralshorts ... --set subtitles.color_active=#00E676 --set clips.min_gap=120
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
- **`cpu_threads`** (1.4x): con el valor por defecto de la librería, un
  procesador de 6 núcleos queda a medias. Ahora se usan los núcleos físicos.

Y después de eso, lo que se ha ido ganando en el resto del recorrido:

- **Descarga con 8 fragmentos en paralelo** (`download.concurrent_fragments`).
  YouTube sirve los vídeos largos troceados y yt-dlp los pedía de uno en uno:
  la descarga tardaba mucho más que la conexión. Con `download.max_height: 720`
  se baja a la mitad de datos, a cambio de un pelo de nitidez en el panel de
  arriba.
- **Transcripción con whisper.cpp**: solo se transcriben las zonas calientes
  (con `tiny` en CPU, unos 14 minutos de audio tardan ~26 s). Los ajustes
  `transcribe.batched`, `batch_size`, `vad_filter` y `compute_type` se conservan
  en el YAML por compatibilidad pero whisper.cpp no los usa.
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
- Con GPU NVIDIA y el motor CUDA (botón «Activar la GPU» de la aplicación) la
  transcripción baja a segundos. Una AMD no sirve para transcribir: el motor
  que se instala solo tiene CUDA (para codificar vídeo sí vale, ver
  `hardware_encoder`).

El render son 1-3 minutos por clip con `zoom` activado, y bastante menos sin él.

## Migración de Python a Rust

El programa se escribió primero en Python (faster-whisper, OpenCV, el SDK de
Anthropic) y se ha reescrito entero en Rust para que sea un único ejecutable.
La línea de comandos, `config.yaml`, la interfaz (`web/index.html`), las rutas
`/api/*`, los planes de montaje (`work/plans/*.json`) y los ficheros que deja
cada short son los mismos. Lo que cambia por dentro:

| Antes | Ahora |
|---|---|
| faster-whisper (CTranslate2) | whisper.cpp (`whisper-cli`), modelos `ggml-<tamaño>.bin` |
| cuBLAS + cuDNN por `pip` | motor CUDA de whisper.cpp con sus librerías |
| OpenCV (`FaceDetectorYN`, Haar de respaldo) | YuNet con ONNX Runtime; fotogramas decodificados por ffmpeg; sin Haar |
| SDK `anthropic` | la API de mensajes por HTTP, mismas peticiones |
| yt-dlp como librería | yt-dlp como ejecutable |
| servidor `http.server` + tkinter | servidor propio sobre `std::net` + diálogo nativo (`rfd`) |

Diferencias que se notan: la transcripción sale de otro motor, así que las
palabras y sus tiempos no son idénticos (y `tiny` falla bastante más que
`medium`, que es el valor por defecto); y los tiempos de transcripción en CPU
son varias veces menores. Los shorts de la versión Python pueden convivir en
`output/`.

## Pruebas

```bash
cd rust
cargo test
```

Unas 300 pruebas unitarias (dos segundos, sin red ni API) y las de `rust/tests`,
que usan ffmpeg de verdad (renderizan un short y comprueban que sale de
1080x1920 con audio y con la duración del plan) y, si están a mano, yt-dlp,
whisper.cpp y YuNet. Las que necesitan red solo corren con `VS_NET_TESTS=1`.
Cubren justo lo que falla **en silencio**: los tiempos (jump cuts, remapeo de
palabras, tiempos del `.ASS`, expresiones de recorte de ffmpeg), la elección de
clips (ventanas, hueco mínimo, deduplicación por tema), la configuración y los
`--set`, el plan de montaje, la seguridad del servidor local y los lotes y la
caché de las llamadas al modelo. Un fallo ahí no rompe nada: saca un short con
buena pinta y los subtítulos medio segundo desplazados.

## Aviso legal

Montar un podcast ajeno con gameplay ajeno y subirlo entra de lleno en Content
ID y en las políticas de contenido no original de YouTube y TikTok. Si es tu
propio contenido, o tienes permiso, sin problema; si no, cuenta con
reclamaciones y con posibles retiradas.

## Origen del pipeline

Este pipeline sustituye a los antiguos `main.py` (descarga + selección por
densidad de palabras + montaje con moviepy) y `sub.py` (subtítulos .ASS por
lotes), ya eliminados.
