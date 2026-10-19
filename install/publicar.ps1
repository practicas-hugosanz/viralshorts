<#
  Publica una version nueva de viralshorts.

  Es lo que hace llegar las actualizaciones a quien ya lo tiene instalado: la
  aplicacion mira la ultima release del repositorio publico de publicaciones y,
  si su version es mayor que la que tiene, ofrece actualizar.

      powershell -ExecutionPolicy Bypass -File install\publicar.ps1 -Version 0.2.0 -Notas "Mejoras de ..."

  Hace, por orden:
    1. pone la version en rust\Cargo.toml y compila el instalador
    2. escribe latest.json (version, notas, direccion y huella SHA-256); las notas
       son las de la entrada de la version en el "Historial de cambios" del README
       (que tiene que existir) salvo que se pasen con -Notas
    3. guarda el cambio en git, lo etiqueta (v0.2.0) y lo sube al repositorio de codigo
    4. crea la release en el repositorio de publicaciones con el instalador y latest.json

  Hace falta tener `gh` con sesion iniciada (gh auth login).
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version,
    [string]$Notas = '',
    # Repositorio publico donde se publican los instaladores (el mismo que lee la aplicacion).
    [string]$Publicaciones = 'practicas-hugosanz/viralshorts-releases',
    # Compilar y empaquetar, pero no subir nada.
    [switch]$SoloPreparar
)

$ErrorActionPreference = 'Stop'
$Raiz = Split-Path -Parent $PSScriptRoot
Set-Location $Raiz

if (-not $SoloPreparar -and -not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw 'Falta la herramienta gh (https://cli.github.com). Instalala y ejecuta: gh auth login'
}

# --- 0. historial de cambios ---------------------------------------------------------
# Cada version publicada tiene su entrada en el README ("### 1.0.3 (fecha)"): sin
# ella no se publica, y su texto es el de las notas de la release.
$readme = Get-Content -LiteralPath (Join-Path $Raiz 'README.md') -Raw -Encoding UTF8
$entrada = [regex]::Match($readme, "(?ms)^### $([regex]::Escape($Version)) [^\r\n]*\r?\n(.*?)(?=^#{2,3} )")
if (-not $entrada.Success -or [string]::IsNullOrWhiteSpace($entrada.Groups[1].Value)) {
    throw "Falta la entrada ### $Version en el 'Historial de cambios' del README. Escribe que cambia en esta version y vuelve a lanzarlo."
}
if (-not $Notas) { $Notas = $entrada.Groups[1].Value.Trim() }

# --- 1. version y compilacion ------------------------------------------------------
$toml = Join-Path $Raiz 'rust\Cargo.toml'
$texto = Get-Content -LiteralPath $toml -Raw
$actual = if ($texto -match '(?m)^version\s*=\s*"([^"]+)"') { $Matches[1] } else { '' }
if ($actual -eq $Version) {
    Write-Host "La version ya es $Version; se reutiliza." -ForegroundColor Yellow
} else {
    $nuevo = [regex]::Replace($texto, '(?m)^version\s*=\s*"[^"]+"', "version = `"$Version`"", 1)
    [System.IO.File]::WriteAllText($toml, $nuevo)
}
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'crear-instalador.ps1')
if ($LASTEXITCODE -ne 0) { throw 'No se pudo crear el instalador.' }

# --- 2. latest.json -------------------------------------------------------------------
$setup = Join-Path $Raiz "dist\viralshorts-setup-$Version.exe"
$huella = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLower()
$manifiesto = [ordered]@{
    version  = $Version
    notes    = $Notas
    pub_date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    url      = "https://github.com/$Publicaciones/releases/download/v$Version/viralshorts-setup-$Version.exe"
    sha256   = $huella
}
$latest = Join-Path $Raiz 'dist\latest.json'
[System.IO.File]::WriteAllText($latest, ($manifiesto | ConvertTo-Json))
Write-Host "latest.json escrito (sha256 $huella)" -ForegroundColor Cyan

if ($SoloPreparar) { Write-Host 'Listo (sin subir nada).' -ForegroundColor Green; return }

# --- 3. git ---------------------------------------------------------------------------
git add -A
if (git status --porcelain) { git commit -q -m "Version $Version" }
git tag "v$Version"
git push origin HEAD --tags
if ($LASTEXITCODE -ne 0) { throw 'No se pudo subir el codigo.' }

# --- 4. release publica ------------------------------------------------------------------
$notasRelease = if ($Notas) { $Notas } else { "viralshorts $Version" }
gh release create "v$Version" $setup $latest --repo $Publicaciones --title "viralshorts $Version" --notes $notasRelease
if ($LASTEXITCODE -ne 0) { throw 'No se pudo crear la release.' }

Write-Host ''
Write-Host "Publicada la version ${Version}: quien ya lo tenga instalado vera el aviso de actualizacion." -ForegroundColor Green
