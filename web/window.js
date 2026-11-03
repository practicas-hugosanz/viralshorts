// Los mandos de la ventana, ahora que no tiene la barra de titulo de Windows:
// minimizar, maximizar, cerrar, moverla y estirarla por arriba (los otros tres
// lados los resuelve Windows). Solo lo carga la ventana de la aplicacion, asi que
// en el navegador la pagina se queda como esta.
(() => {
  if (window.top !== window) return;

  const send = message => window.ipc && window.ipc.postMessage(message);
  const glyph = {
    min: '<path d="M0 5.5h10"/>',
    max: '<rect x=".5" y=".5" width="9" height="9"/>',
    restore: '<rect x=".5" y="2.5" width="7" height="7"/><path d="M2.5 2.5v-2h7v7h-2"/>',
    close: '<path d="M.5.5l9 9M9.5.5l-9 9"/>',
  };
  const icon = name => `<svg viewBox="0 0 10 10" width="10" height="10" fill="none" stroke="currentColor" aria-hidden="true">${glyph[name]}</svg>`;

  const css = `
    #vs-win { display: flex; flex: none; align-self: flex-start; margin: 0 -16px 0 6px; }
    #vs-win.loose { position: fixed; top: 0; right: 0; margin: 0; z-index: 2147482000; }
    #vs-win button {
      all: unset; box-sizing: border-box; width: 46px; height: 32px;
      display: grid; place-items: center; color: var(--text, #0f0f0f); cursor: default;
    }
    #vs-win button:hover { background: var(--hover, var(--raise, rgba(128,128,128,.2))); }
    #vs-win button:focus-visible { outline: 2px solid currentColor; outline-offset: -2px; }
    #vs-win #vs-close:hover { background: #e81123; color: #fff; }
    #vs-drag { position: fixed; inset: 0 0 auto 0; height: 36px; z-index: 2147481000; }
    .masthead { user-select: none; }
    .vs-edge { position: fixed; top: 0; height: 4px; z-index: 2147483000; }
    .vs-edge[data-edge="n"] { left: 0; right: 0; cursor: n-resize; }
    .vs-edge[data-edge="nw"] { left: 0; width: 10px; height: 8px; cursor: nw-resize; }
    .vs-edge[data-edge="ne"] { right: 0; width: 10px; height: 8px; cursor: ne-resize; }
    html.vs-max .vs-edge { display: none; }
  `;

  // Rust llama a esto cuando la ventana se maximiza o deja de estarlo.
  window.__vsWindow = maximized => {
    document.documentElement.classList.toggle("vs-max", maximized);
    const button = document.getElementById("vs-max");
    if (!button) return;
    const label = maximized ? "Restaurar" : "Maximizar";
    button.innerHTML = icon(maximized ? "restore" : "max");
    button.title = label;
    button.setAttribute("aria-label", label);
  };

  function mount() {
    if (!window.ipc || document.getElementById("vs-win")) return;
    const style = document.createElement("style");
    style.textContent = css;
    document.head.append(style);

    const controls = document.createElement("div");
    controls.id = "vs-win";
    controls.innerHTML = `
      <button type="button" id="vs-min" title="Minimizar" aria-label="Minimizar">${icon("min")}</button>
      <button type="button" id="vs-max"></button>
      <button type="button" id="vs-close" title="Cerrar" aria-label="Cerrar">${icon("close")}</button>`;
    const bar = document.querySelector(".mh-end");
    if (bar) {
      bar.append(controls);
    } else {
      // Paginas sin barra superior (la de la primera vez): franja para arrastrar.
      const band = document.createElement("div");
      band.id = "vs-drag";
      controls.className = "loose";
      document.body.append(band, controls);
    }
    for (const edge of ["n", "nw", "ne"]) {
      const handle = document.createElement("div");
      handle.className = "vs-edge";
      handle.dataset.edge = edge;
      handle.addEventListener("mousedown", event => {
        if (event.button === 0) send(`resize:${edge}`);
      });
      document.body.append(handle);
    }
    document.getElementById("vs-min").addEventListener("click", () => send("minimize"));
    document.getElementById("vs-max").addEventListener("click", () => send("maximize"));
    document.getElementById("vs-close").addEventListener("click", () => send("close"));

    document.addEventListener("mousedown", event => {
      if (event.button !== 0 || !(event.target instanceof Element)) return;
      if (!event.target.closest(".masthead, #vs-drag")) return;
      if (event.target.closest("button, input, a, select, textarea, label")) return;
      send(event.detail === 2 ? "maximize" : "drag");
    });

    window.__vsWindow(false);
    send("state");
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", mount);
  else mount();
})();
