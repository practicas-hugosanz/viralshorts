// Prueba de humo de la interfaz: abre la pagina en un Edge sin ventana, la usa
// como una persona (cambia de seccion, toca los controles, simula eventos del
// servidor) y comprueba lo que se ve y lo que se enviaria.
//
//   node web/tests/ui-smoke.mjs <url con token> [ruta de msedge.exe]
//
// Lo normal es lanzarla con install\probar-interfaz.ps1, que prepara un entorno
// de prueba, arranca el servidor y la ejecuta. Sale con codigo 1 si algo falla.

import { spawn } from 'node:child_process';

const [, , url, edge = 'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe'] = process.argv;
if (!url) { console.error('Falta la direccion (con ?t=token).'); process.exit(2); }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const profile = (process.env.TEMP || '.') + '\\vs-ui-smoke-' + Date.now();
const browser = spawn(edge, [
  '--headless=new', '--remote-debugging-port=9334', `--user-data-dir=${profile}`,
  '--hide-scrollbars', '--disable-gpu', '--no-first-run', 'about:blank',
], { stdio: 'ignore' });

let targets;
for (let i = 0; i < 60; i++) {
  try { targets = await (await fetch('http://127.0.0.1:9334/json')).json(); if (targets.length) break; } catch {}
  await sleep(250);
}
const ws = new WebSocket(targets.find((t) => t.type === 'page').webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
const consoleErrors = [];
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  if (d.id && pending.has(d.id)) { pending.get(d.id)(d); pending.delete(d.id); }
  if (d.method === 'Runtime.exceptionThrown') consoleErrors.push(d.params.exceptionDetails.exception?.description || d.params.exceptionDetails.text);
  if (d.method === 'Runtime.consoleAPICalled' && d.params.type === 'error') consoleErrors.push(d.params.args.map((a) => a.value || a.description).join(' '));
};
const send = (method, params = {}) => new Promise((res) => { const n = ++id; pending.set(n, res); ws.send(JSON.stringify({ id: n, method, params })); });
const run = async (expression) => {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description || 'error en la pagina');
  return r.result?.result?.value;
};

await send('Runtime.enable');
await send('Page.enable');
await send('Emulation.setDeviceMetricsOverride', { width: 1360, height: 880, deviceScaleFactor: 1, mobile: false });
await send('Page.navigate', { url });
await sleep(3000);

const results = [];
const check = (name, ok, detail = '') => { results.push({ name, ok: !!ok, detail }); };

// ------------------------------------------------------------ carga y secciones
check('la pagina carga y tiene titulo', (await run('document.title')) === 'Viral Shorts');
for (const view of ['crear', 'trabajo', 'biblioteca', 'ajustes']) {
  const ok = await run(`(() => { go(${JSON.stringify(view)}); return !document.getElementById("view-${view}").hidden &&
    document.querySelector('.nav-item[data-view="${view}"]').getAttribute("aria-current") === "page"; })()`);
  check(`se abre la seccion ${view}`, ok);
}
await run('go("crear")');

// ------------------------------------------------------------ composicion, formato, estilos
const look = await run(`(async () => {
  const click = (sel) => document.querySelector(sel).click();
  const out = {};
  click('input[name="comp"][value="center"]');
  out.centro = $("screen").classList.contains("mode-center") && $("comp-stack").hidden;
  click('input[name="comp"][value="full"]');
  out.completo = $("screen").classList.contains("mode-full");
  click('input[name="comp"][value="stack"]');
  click('input[name="ratio"][value="60"]');
  out.ratio = $("screen").style.getPropertyValue("--top") === "60%";
  $("comp-swap").click();
  out.swap = $("screen").classList.contains("swap");
  $("comp-swap").click();
  click('input[name="fmt"][value="1:1"]');
  out.formato = $("phone").style.aspectRatio.replace(/\\s/g, "") === "1/1";
  click('input[name="fmt"][value="9:16"]');
  for (const style of ["box", "marker", "neon", "minimal", "classic"]) {
    click('input[name="sstyle"][value="' + style + '"]');
    out["estilo_" + style] = $("ph_sub").classList.contains("s-" + style);
  }
  return out;
})()`);
check('composicion en el centro', look.centro);
check('composicion a pantalla completa', look.completo);
check('reparto 60 %', look.ratio);
check('pódcast abajo', look.swap);
check('formato 1:1 cambia la proporcion del monitor', look.formato);
for (const style of ['box', 'marker', 'neon', 'minimal', 'classic']) check(`estilo ${style} en el monitor`, look['estilo_' + style]);

// ------------------------------------------------------------ lo que se envia al servidor
const sent = await run(`(() => {
  document.querySelector('input[name="comp"][value="center"]').click();
  document.querySelector('input[name="fmt"][value="4:5"]').click();
  document.querySelector('input[name="sstyle"][value="marker"]').click();
  $("st_handle").value = "@prueba"; $("st_handle").dispatchEvent(new Event("input", { bubbles: true }));
  $("bar-on").click(); $("fx-sfx").click(); $("fx-beep").click(); $("fx-end").click();
  $("st_bgstart").value = "random"; $("st_bgblur").value = 40; $("st_bgdim").value = 25; $("st_bgspeed").value = 1.5;
  $("st_refine").value = "large-v3-turbo";
  const form = collect(true);
  return { review: form.review, style: form.style };
})()`);
const st = sent.style;
check('la revision se pide con review:true', sent.review === true);
check('se envia la composicion y el formato', st.layout_mode === 'center' && st.format === '4:5');
check('se envia la plantilla de subtitulos', st.sub_style === 'marker');
check('se envia el usuario del canal', st.handle === '@prueba');
check('se envian barra, efectos y pantalla final', st.bar === 'true' && st.sfx === 'true' && st.censor_beep === 'true' && st.endcard === 'true');
check('el fondo se envia en sus unidades (0..1)', st.bg_start === 'random' && st.bg_blur === 0.4 && st.bg_dim === 0.25 && st.bg_speed === '1.5');
check('el modelo de afinado se envia', st.refine === 'large-v3-turbo');
// volver a lo de serie para no ensuciar lo que sigue
await run(`(() => {
  document.querySelector('input[name="comp"][value="stack"]').click();
  document.querySelector('input[name="fmt"][value="9:16"]').click();
  document.querySelector('input[name="sstyle"][value="classic"]').click();
  $("st_handle").value = ""; $("bar-on").click(); $("fx-sfx").click(); $("fx-beep").click(); $("fx-end").click();
  $("st_bgstart").value = "sequence"; $("st_bgblur").value = 0; $("st_bgdim").value = 0; $("st_bgspeed").value = 1;
  $("st_refine").value = ""; afterLook(); 0;
})()`);

// ------------------------------------------------------------ perfiles y kits (contra el servidor de verdad)
const profiles = await run(`(async () => {
  const out = {};
  document.querySelector('input[name="sstyle"][value="neon"]').click();
  $("st_handle").value = "@kit"; $("st_handle").dispatchEvent(new Event("input", { bubbles: true }));
  $("kit-save").click(); $("profile-name").value = "Kit de prueba"; $("profile-ok").click();
  await new Promise(r => setTimeout(r, 700));
  out.kitGuardado = [...$("kit").options].some(o => o.value === "Kit de prueba");
  out.kitNoEsPerfil = ![...$("profile").options].some(o => o.value === "Kit de prueba");
  document.querySelector('input[name="sstyle"][value="classic"]').click();
  $("st_handle").value = "";
  $("kit").value = "Kit de prueba"; $("kit").dispatchEvent(new Event("change"));
  out.kitAplicado = $("st_sstyle").value === "neon" && $("st_handle").value === "@kit";
  $("kit-del").click();
  await new Promise(r => setTimeout(r, 700));
  out.kitBorrado = ![...$("kit").options].some(o => o.value === "Kit de prueba");

  setClips(7);
  $("profile-save").click(); $("profile-name").value = "Perfil de prueba"; $("profile-ok").click();
  await new Promise(r => setTimeout(r, 700));
  out.perfilGuardado = [...$("profile").options].some(o => o.value === "Perfil de prueba");
  setClips(2);
  $("profile").value = "Perfil de prueba"; $("profile").dispatchEvent(new Event("change"));
  out.perfilAplicado = $("clips").value === "7";
  $("profile-del").click();
  await new Promise(r => setTimeout(r, 700));
  out.perfilBorrado = ![...$("profile").options].some(o => o.value === "Perfil de prueba");
  setClips(3); document.querySelector('input[name="sstyle"][value="classic"]').click(); $("st_handle").value = ""; afterLook();
  return out;
})()`);
check('un kit de marca se guarda y no aparece como perfil', profiles.kitGuardado && profiles.kitNoEsPerfil);
check('un kit de marca se aplica', profiles.kitAplicado);
check('un kit de marca se borra', profiles.kitBorrado);
check('un perfil se guarda, se aplica y se borra', profiles.perfilGuardado && profiles.perfilAplicado && profiles.perfilBorrado);

// ------------------------------------------------------------ revision de momentos (edicion por clip)
const review = await run(`(async () => {
  const fire = (e) => events.onmessage({ data: JSON.stringify(e) });
  fire({ type: "job", status: "done" });
  fire({ type: "plan", ruta: "no-existe.json", clips: [
    { n: 1, seleccionado: true, titulo: "Uno", gancho: "G", hashtags: ["#a"], motivo: "m", texto: "t", palabras: "hola esto es una prueba", marca: "01:00", duracion: 30, puntuacion: 0.9, recorte: { inicio: 0, fin: 0 }, editado: false, estilo: "" },
    { n: 2, seleccionado: false, titulo: "Dos", gancho: "G2", hashtags: [], motivo: "", texto: "t2", palabras: "otro texto", marca: "02:00", duracion: 28, puntuacion: 0.7, recorte: { inicio: 4, fin: 2 }, editado: true, estilo: "neon" },
  ] });
  await new Promise(r => setTimeout(r, 300));
  const card = (n) => document.querySelector('.review[data-n="' + n + '"]');
  const out = {};
  out.tarjetas = document.querySelectorAll(".review").length;
  out.textarea = card(1).querySelector('[data-role="palabras"]').value;
  out.recorteDos = [card(2).querySelector('[data-role="trim-start"]').value, card(2).querySelector('[data-role="trim-end"]').value];
  out.estiloDos = card(2).querySelector('[data-role="estilo"]').value;
  out.restaurar = !!card(2).querySelector('[data-role="restore"]') && !card(1).querySelector('[data-role="restore"]');
  out.desmarcada = card(2).classList.contains("off");
  // Si no se toca nada, no se manda texto; si se toca, si.
  const antes = reviewChanges();
  out.sinTexto = antes[0].texto_palabras === undefined;
  card(1).querySelector('[data-role="palabras"]').value = "hola esto es OTRA prueba";
  out.conTexto = reviewChanges()[0].texto_palabras === "hola esto es OTRA prueba";
  // Un cambio de recorte no manda el texto (ya no corresponde al tramo).
  card(1).querySelector('[data-role="trim-start"]').value = 3;
  out.recorteSinTexto = reviewChanges()[0].texto_palabras === undefined && reviewChanges()[0].recorte.inicio === 3;
  return out;
})()`);
check('la revision pinta una tarjeta por momento', review.tarjetas === 2);
check('el texto de los subtitulos se puede editar', review.textarea === 'hola esto es una prueba');
check('el recorte y el estilo de un clip editado se muestran', review.recorteDos?.join(',') === '4,2' && review.estiloDos === 'neon');
check('solo el clip editado ofrece deshacer', review.restaurar);
check('un momento sin marcar se ve apagado', review.desmarcada);
check('el texto solo se envia si se ha tocado', review.sinTexto && review.conTexto);
check('al recortar no se envia el texto', review.recorteSinTexto);

// ------------------------------------------------------------ aviso de version y cola
const misc = await run(`(() => {
  const out = {};
  const notes = "- **Uno** con \\\`codigo\\\`\\r\\n  partido en dos lineas\\n- Dos";
  renderUpdate({ current: "1.0.0", available: { version: "9.9.9", notes }, phase: "" });
  out.lista = document.querySelectorAll("#upd li").length === 2;
  out.negrita = !!document.querySelector("#upd li b");
  out.unida = document.querySelector("#upd li").textContent.includes("codigo partido en dos lineas");
  out.boton = !!document.getElementById("upd-go");
  clearBanner($("upd"));
  events.onmessage({ data: JSON.stringify({ type: "job", status: "running" }) });
  events.onmessage({ data: JSON.stringify({ type: "queue", queue: [{ n: 1, podcast: "a", clips: 1 }, { n: 2, podcast: "b", clips: 2 }] }) });
  const botones = [...document.querySelectorAll("#queue-list .ops button")];
  out.cola = botones.length === 6 && botones[0].disabled && botones[4].disabled && !botones[1].disabled;
  out.saltar = !$("skip-row").hidden;
  events.onmessage({ data: JSON.stringify({ type: "texto", t: 75, texto: "una frase de prueba" }) });
  out.directo = !$("live-text").hidden && $("live-lines").textContent.includes("1:15") && $("live-lines").textContent.includes("una frase");
  events.onmessage({ data: JSON.stringify({ type: "queue", queue: [] }) });
  events.onmessage({ data: JSON.stringify({ type: "job", status: "done" }) });
  return out;
})()`);
check('las notas de version salen como lista con negrita', misc.lista && misc.negrita && misc.unida && misc.boton);
check('la cola se puede reordenar y saltar', misc.cola && misc.saltar);
check('la transcripcion en directo se pinta con su minuto', misc.directo);

// ------------------------------------------------------------ biblioteca
const lib = await run(`(async () => {
  await loadLibrary();
  const out = { total: LIB.length, tarjetas: document.querySelectorAll("#results .result").length };
  if (LIB.length) {
    $("lib-search").value = "zzzz-no-existe"; $("lib-search").dispatchEvent(new Event("input"));
    out.filtra = [...document.querySelectorAll("#results .result")].every(c => c.hidden) && !$("lib-none").hidden;
    $("lib-search").value = ""; $("lib-search").dispatchEvent(new Event("input"));
    out.vuelve = [...document.querySelectorAll("#results .result")].every(c => !c.hidden);
    const botones = [...document.querySelector("#results .result .foot").querySelectorAll(".btn")];
    const tarjeta = document.querySelector("#results .result").getBoundingClientRect();
    out.caben = botones.every(b => b.getBoundingClientRect().right <= tarjeta.right + 1);
  }
  return out;
})()`);
check('la biblioteca pinta las tarjetas', lib.tarjetas === lib.total);
if (lib.total) {
  check('la busqueda filtra y se puede quitar', lib.filtra && lib.vuelve);
  check('los botones caben en la tarjeta', lib.caben);
}

// ------------------------------------------------------------ colores y accesibilidad basica
const a11y = await run(`(() => {
  const sinNombre = [...document.querySelectorAll("button, input, select, textarea")].filter(el => {
    if (el.type === "hidden" || el.closest("[hidden]")) return false;
    const label = el.getAttribute("aria-label") || el.title || el.textContent.trim() || (el.id && document.querySelector('label[for="' + el.id + '"]')) || el.closest("label") || el.placeholder;
    return !label;
  }).map(el => el.id || el.outerHTML.slice(0, 60));
  return { sinNombre };
})()`);
check('todos los controles visibles tienen nombre', a11y.sinNombre.length === 0, a11y.sinNombre.join(', '));

check('no hay errores de JavaScript en la consola', consoleErrors.length === 0, consoleErrors.join(' | ').slice(0, 300));

// ------------------------------------------------------------ informe
ws.close();
browser.kill();
const fallos = results.filter((r) => !r.ok);
for (const r of results) console.log(`${r.ok ? 'OK   ' : 'FALLA'} ${r.name}${r.ok || !r.detail ? '' : '  -> ' + r.detail}`);
console.log(`\n${results.length - fallos.length} de ${results.length} comprobaciones bien.`);
process.exit(fallos.length ? 1 : 0);
