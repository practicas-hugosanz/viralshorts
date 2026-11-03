; Instalador de viralshorts (Inno Setup 6).
;
; Se compila con install\crear-instalador.ps1, que compila antes el programa y le
; pasa la version y la ruta del .exe. Instala para el usuario actual, sin
; permisos de administrador: el programa en %LOCALAPPDATA%\Programs\viralshorts
; y, la primera vez que se abre, descarga sus herramientas a
; %LOCALAPPDATA%\viralshorts.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef ExePath
  #define ExePath "..\rust\target\release\viralshorts.exe"
#endif

[Setup]
AppId={{B2F0D1E4-6C1B-4B0B-9C8A-5A7B0F1E2D33}
AppName=Viral Shorts
AppVersion={#AppVersion}
AppPublisher=Viral Shorts
DefaultDirName={autopf}\viralshorts
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=viralshorts-setup-{#AppVersion}
SetupIconFile=..\rust\assets\icon.ico
UninstallDisplayIcon={app}\viralshorts.exe
UninstallDisplayName=Viral Shorts
Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"

[Tasks]
Name: "desktopicon"; Description: "Crear un acceso directo en el &Escritorio"; GroupDescription: "Accesos directos:"

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Viral Shorts"; Filename: "{app}\viralshorts.exe"; AppUserModelID: "ViralShorts.App"
Name: "{autodesktop}\Viral Shorts"; Filename: "{app}\viralshorts.exe"; AppUserModelID: "ViralShorts.App"; Tasks: desktopicon

[Registry]
; El nombre con el que salen los avisos de Windows (la aplicacion lo crea al avisar).
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\ViralShorts.App"; ValueType: string; ValueName: "DisplayName"; ValueData: "Viral Shorts"; Flags: uninsdeletekey

[InstallDelete]
; Hasta la 1.0.2 el acceso directo del menu Inicio se llamaba "viralshorts".
Type: files; Name: "{autoprograms}\viralshorts.lnk"

[Run]
; Sin skipifsilent: una actualizacion en silencio (la lanza la propia aplicacion)
; tiene que volver a abrirla al terminar.
Filename: "{app}\viralshorts.exe"; Description: "Abrir Viral Shorts"; Flags: nowait postinstall

[Code]
{ El acceso directo del Escritorio de las versiones anteriores se llamaba
  "viralshorts". Si el usuario lo tenia, se conserva con el nombre nuevo (y si
  esta instalacion ya ha creado el nuevo, se quita el viejo). }
procedure CurStepChanged(CurStep: TSetupStep);
var
  Viejo, Nuevo: String;
begin
  if CurStep <> ssPostInstall then Exit;
  Viejo := ExpandConstant('{autodesktop}\viralshorts.lnk');
  Nuevo := ExpandConstant('{autodesktop}\Viral Shorts.lnk');
  if not FileExists(Viejo) then Exit;
  if FileExists(Nuevo) then
    DeleteFile(Viejo)
  else
    RenameFile(Viejo, Nuevo);
end;

{ Al desinstalar se pregunta por lo descargado: las herramientas y los modelos
  ocupan cientos de MB. Los shorts (carpeta Videos\viralshorts) no se tocan. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Datos: String;
begin
  if CurUninstallStep <> usPostUninstall then Exit;
  Datos := ExpandConstant('{localappdata}\viralshorts');
  if (not DirExists(Datos)) or UninstallSilent then Exit;
  if MsgBox('¿Quieres borrar también las herramientas y los modelos que descargó Viral Shorts?' + #13#10 +
            'Ocupan varios cientos de MB. Tus shorts (Vídeos\viralshorts) no se tocan.',
            mbConfirmation, MB_YESNO or MB_DEFBUTTON1) = IDYES then
    DelTree(Datos, True, True, True);
end;
