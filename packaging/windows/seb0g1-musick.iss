; Установщик Seb0g1 Musick (Inno Setup 6). Сборка:
;   cargo build --release
;   "%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe" packaging\windows\seb0g1-musick.iss
; Готовый файл: dist\Seb0g1-Musick-Setup-<версия>.exe

#define AppName "Seb0g1 Musick"
#define AppVersion "1.1.5.2"
#define AppExe "Seb0g1 Musick.exe"
#define Port "8787"

[Setup]
AppId={{6B1E0C7A-5D3F-4E21-9A64-5EB0611C0DE5}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Seb0g1
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} — установщик
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Администратор нужен для папки Program Files и правила брандмауэра (иначе телефон не подключится).
; Без прав администратора можно поставить только для себя: тогда Windows сама спросит про доступ к сети.
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\..\dist
OutputBaseFilename=Seb0g1-Musick-Setup-{#AppVersion}
SetupIconFile=..\..\icons\app.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
LicenseFile=..\..\LICENSE
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "desktopicon"; Description: "Ярлык на рабочем столе"; GroupDescription: "Дополнительно:"
Name: "firewall"; Description: "Разрешить подключение телефона по Wi-Fi (правило брандмауэра для частных сетей)"; GroupDescription: "Дополнительно:"; Check: IsAdminInstallMode

[Files]
Source: "..\..\target\release\yamusic-rs.exe"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "readme-ru.txt"; DestDir: "{app}"; DestName: "Как подключить телефон.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autoprograms}\Как подключить телефон"; Filename: "{app}\Как подключить телефон.txt"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; Сначала удалить старое правило (переустановка), потом добавить: входящие на порт пульта только в частных и доменных сетях.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; Flags: runhidden; Tasks: firewall
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#AppName}"" dir=in action=allow program=""{app}\{#AppExe}"" enable=yes profile=private,domain protocol=TCP localport={#Port}"; Flags: runhidden; Tasks: firewall
Filename: "{app}\{#AppExe}"; Description: "Запустить {#AppName} (откроется страница «Подключить телефон»)"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; Flags: runhidden; RunOnceId: "fwdel"; Check: IsAdminInstallMode

[Messages]
ru.WelcomeLabel2=Будет установлен [name/ver].%n%nМузыка Яндекса на компьютере и пульт на телефоне: пауза, переключение, громкость, перемотка, лайки, очередь и поиск. После установки откроется страница с QR-кодом — наведите на него камеру телефона.
