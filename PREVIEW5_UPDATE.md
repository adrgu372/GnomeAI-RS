> **Actualizare curentă: preview6.** Vezi [PREVIEW6_UPDATE.md](PREVIEW6_UPDATE.md) pentru UX, auto-grow și copy/paste. Notele de mai jos descriu versiunile anterioare.

# Preview 5 — actualizare de surse, oprită la cererea utilizatorului

Această arhivă continuă preview4 și include modificările efectuate până la cererea de încheiere. Nu include APK, DEB sau biblioteci compilate. **Nu s-a compilat și nu s-au executat aplicațiile pe desktop sau Android.** Implementările de mai jos sunt în sursă; validarea funcțională rămâne de făcut după build.

## Ce s-a modificat

| Backlog | Modificări în sursă | Validare rămasă |
| --- | --- | --- |
| 5 — un singur shell desktop | `GnomeAI.Client` conține logică, fără referințe Avalonia. `MainView` și componentele sale vizuale au fost mutate în proiectul Android. Desktopul folosește sidebar-ul, transcriptul și composer-ul existente pentru conversații remote. Devices este o pagină de administrare, fără composer. Ciornele și starea conversațiilor sunt separate după dispozitiv. | Build desktop, schimbarea repetată local/remote, streaming, aprobări și ciorne. |
| 2 — activare după pairing | Confirmarea persistă pairing-ul, notifică starea și trimite imediat handshake-ul pentru conexiune. Confirmările se retransmit pentru a recupera ultimul mesaj pierdut. Expirarea invitației nu mai anulează o conexiune devenită trusted. | Pairing în ambele sensuri fără restart, inclusiv confirmări decalate. |
| 1 — reconnect | EOF/timeout eliberează conexiunea; heartbeat are termen finit; retry cu backoff și jitter. O conexiune Tor nouă trebuie să dovedească posesia cheii pairing-ului, printr-un challenge nou, înainte să înlocuiască socketul vechi. Schimbările de rețea declanșează reconnect. Snapshot-urile și lista remote se reîncarcă la reconectare. | Wi-Fi → 5G → Wi-Fi, conexiune întreruptă în timpul unui răspuns, restart separat pe fiecare dispozitiv. |
| 3 — tastatură Android | Insets calculate din zona nativă efectiv disponibilă, ținând cont de AdjustResize. Navigarea de jos se ascunde cât timp tastatura este vizibilă. Padding-ul se recalculează la închiderea tastaturii. | Telefon real, Android 8–16, portrait/landscape, tastaturi diferite și mod multi-window. |
| 4 — QR integrat | Devices → Pair new device → Scan QR deschide o cameră nativă în aplicație. ZXing decodează local doar QR; invitația GnomeAI, expirarea și cheile sunt validate înainte de pairing. Paste pairing code rămâne disponibil. Camera se închide la ieșire/pauză. | Permisiune acordată/refuzată, QR invalid/expirat, autofocus și revenire din scanner. |
| 6 — navigație | Un selector This PC / peers / Manage devices; Devices în sidebar; Move / Sync în bara conversației, vizibil contextual. Execution nodes sunt administrate pe aceeași pagină Devices și diferențiate prin capabilități. | Aspect, dimensiuni mici de fereastră, Move / Sync și administrarea nodurilor. |
| 7–9 — build | `Android.Content` pentru provider; `RootMode="All"`; `MobileApp : Avalonia.Application`; tema AppCompat `GnomeAITheme`; preflight JDK complet, .NET/workload, SDK/API și NDK. | Publish Release și verificarea rezultatului trimmerului pe mediul utilizatorului. |

În Rust s-au adăugat dependențele directe `sha2` și `safelog`, s-a corectat conversia `HsId` prin `display_unredacted()` și s-a actualizat `Cargo.lock` fără actualizarea versiunilor dependențelor existente. Comanda cargo-ndk folosește `--platform 26`, nu `-p 26`.

## Camera foto și compatibilitate

Permisiunea CAMERA nu este cerută la pornirea aplicației sau la simpla deschidere a paginii Devices. În fluxul de pairing este cerută numai după Scan QR. Deoarece CAMERA este acum declarată pentru scanner, Android cere grant și înaintea capturii foto delegate: butonul existent Take a photo poate cere aceeași permisiune la prima utilizare. Refuzul permite în continuare alegerea unei imagini și lipirea invitației.

Actualizează **atât desktopul, cât și Androidul** pentru noul handshake de transport și înlocuirea conexiunii. Identitățile și pairing-urile salvate rămân în directoarele de date existente; nu le șterge și nu dezinstala aplicația Android pentru update. Compatibilitatea unei perechi mixte preview4/preview5 nu este validată.

Setările locale, workspace-ul local și atașamentele de pe calculator se folosesc din This PC. Vizualizarea remote include conversații, mesaje text, Stop, aprobări, redenumire/ștergere și Move / Sync; nu adaugă upload remote de atașamente sau administrarea de la distanță a cheilor providerului.

## Build

Android, din rădăcina proiectului, cu mediul SDK/JDK/NDK deja instalat:

```bash
export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/30.0.16248370"
export ANDROID_HOME="$HOME/Android/Sdk"
# JAVA_HOME trebuie să indice JDK-ul complet instalat; scriptul îl poate deduce din javac.
bash scripts/build-android.sh
```

Dacă versiunea NDK instalată diferă, ajustează calea. Scriptul verifică versiunea .NET selectată, workload-ul Android și platforma Android cerută de acesta înainte de compilarea Rust. Nu acceptă automat licențe SDK.

Desktop Linux, pachetul existent:

```bash
bash scripts/build-deb.sh
```

Aceste comenzi **nu au fost executate aici**. Versiunea Android din sursă este `2.4.0-preview5`, versionCode `5`.

## Verificări efectuate

- Parsare sintactică: 95 fișiere C#/Rust; verificare XML/AXAML/csproj, TOML și `bash -n` pentru scripturi.
- `scripts/check-avalonia-ui.py`: 33 handlers, 29 slash commands și 41 operații core.
- `cargo update -p gnomef-rs`, apoi `cargo update --locked --offline -p gnomef-rs`: lockfile coerent; zero versiuni de pachete actualizate. Numai rezoluție de dependențe, fără build.
- Verificare că biblioteca comună nu mai conține UI Avalonia și că desktopul nu mai încorporează shell-ul mobil.

Aceste verificări nu validează rezoluția tuturor tipurilor C#, linkerul Android, randarea, permisiunile sau reconectarea reală. Prima etapă rămasă este compilarea și verificarea pe dispozitive; eventualele erori se corectează pornind de la această arhivă.

## Referințe de implementare

- [Avalonia Android 11.3.20 — insets și IME](https://github.com/AvaloniaUI/Avalonia/blob/11.3.20/src/Android/Avalonia.Android/Platform/AndroidInsetsManager.cs)
- [ZXing.Net — decoder](https://github.com/micjahn/ZXing.Net)
- [API Camera preview Android/.NET](https://learn.microsoft.com/en-us/dotnet/api/android.hardware.camera.parameters.previewformat?view=net-android-35.0)
- [MSBuild: evaluarea proprietăților fără build](https://learn.microsoft.com/en-us/visualstudio/msbuild/evaluate-items-and-properties?view=vs-2022)
