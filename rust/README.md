# viralshorts en Rust

Sustituye al programa Python por un unico ejecutable, sin Python ni `.venv`.
Las herramientas externas siguen siendo ejecutables que descarga el instalador:
`ffmpeg`/`ffprobe`, `yt-dlp` y `whisper-cli` (whisper.cpp). Las caras se
detectaran con `ort` + YuNet (el mismo modelo `.onnx` que ya usa la version
Python).

```bash
cd rust
cargo test                 # unitarias + render real con el ffmpeg instalado
cargo run -- gui           # interfaz visual (--no-browser, --port)
cargo run -- doctor        # comprueba ffmpeg, whisper.cpp, codificador y config.yaml
cargo run --release -- --podcast <URL|fichero> --background <URL|fichero> --clips 3
cargo run --release -- --podcast X --background Y --plan-out work/plans/hoy.json   # elegir ahora
cargo run --release -- --from-plan work/plans/hoy.json --only 1,4                  # montar despues
cargo run -- --set subtitles.font_size=80 config   # config efectiva
```

Pruebas con red (yt-dlp real, whisper.cpp real con el modelo `tiny`): solo con
`VS_NET_TESTS=1`, y necesitan `yt-dlp` en el PATH y `bin/whisper/whisper-cli.exe`.

Disposicion de las herramientas que dejara el instalador (fase 4):

```
%LOCALAPPDATA%/viralshorts/   (o la carpeta del .exe si tiene config.yaml o bin/ al lado)
bin/ffmpeg/bin/    ffmpeg.exe, ffprobe.exe   (los descarga la pantalla de primer arranque)
bin/yt-dlp.exe
bin/whisper/       whisper-cli.exe + DLL (CPU)   whisper-cuda/ (opcional, NVIDIA)
bin/models/        ggml-<tamano>.bin y face_detection_yunet_2023mar.onnx (se descargan solos la primera vez)
(onnxruntime va enlazado dentro del .exe: no hay que repartir DLL)
```

## Fases

| Fase | Contenido | Estado |
|---|---|---|
| 1 | `config`, `progress`, `worklock`, `ffmpeg`, `jumpcuts`, `render` (filtergraph) | hecha |
| 2 | descarga (yt-dlp), energia de audio, preseleccion, transcripcion (whisper.cpp), puntuacion y contexto | hecha |
| 3 | Claude (HTTP), metadatos, plan, subtitulos ASS, fuentes, reencuadre con YuNet (`ort`), `pipeline` completo | hecha |
| 4 | servidor de la interfaz (`web/index.html` incrustado), instalador, empaquetado; Python retirado | hecha |

El programa Python ya no existe en el arbol (sigue en el historial de git).

## Aplicacion de escritorio

`viralshorts.exe` sin argumentos abre una ventana (tao + WebView2) que carga la
interfaz de `gui.rs`; con `--podcast ...` es la linea de comandos de siempre. Es
una aplicacion de ventana (sin consola); en una terminal se engancha a ella.
`install/crear-instalador.ps1` fabrica el instalador con Inno Setup.
