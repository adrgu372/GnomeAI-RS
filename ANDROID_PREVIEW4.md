> **Actualizare curentă: preview6.** Vezi [PREVIEW6_UPDATE.md](PREVIEW6_UPDATE.md) pentru UX, auto-grow și copy/paste. Notele de mai jos descriu versiunile anterioare.

> **Actualizare curentă: preview5.** Vezi [PREVIEW5_UPDATE.md](PREVIEW5_UPDATE.md) pentru modificările incluse, comenzile de build și verificările rămase. Notele despre preview-urile anterioare de mai jos sunt istorice.

# Android preview4 — interfață și cameră foto

Această versiune continuă mesh preview3. Arhiva include proiectul complet;
modificările noi sunt în interfața comună și în proiectul Android. Nucleul Rust
și protocolul Tor nu au fost schimbate în această iterație.

## Interfață

- Temă Android închisă, fundal petrol, accente verzi și font Inter.
- Antet compact, selector de dispozitiv și buton pentru conversație nouă.
- Navigare jos: Chat, Chats, Devices, Settings.
- Ecran de început cu acces la fotografie, conversație și dispozitive.
- Mesaje aerisite, bule verzi pentru utilizator, răspunsuri Markdown și reasoning pliabil.
- Editor de mesaj rotunjit, butoane distincte pentru cameră și fișiere,
  buton Send/Stop și previzualizarea atașamentului înainte de trimitere.
- Lista conversațiilor are acțiunile Rename/Delete în meniul fiecărui rând.
- Setările Android sunt grupate în Model & provider, Web & memory și Subagents.

Interfața rămâne în engleză, ca versiunea anterioară. Stilul Android este încărcat
numai în aplicația Android; clientul desktop păstrează tema sa.

## Camera foto

Într-o conversație locală, apasă iconul camerei. Se deschide aplicația de cameră
instalată pe telefon, cu comenzile sale pentru fotografie, cameră frontală/spate
și confirmare. Fotografia confirmată apare ca atașament în GnomeAI. Adaugă o
întrebare dacă dorești și apasă **Send**. Captura singură nu trimite nimic modelului.

- Fotografiile sunt capturate într-un fișier privat prin `FileProvider` și o
  permisiune temporară acordată numai URI-ului rezultat.
- Captura este inițiată prin butonul utilizatorului. Nu este o unealtă a agentului
  și nu se activează în fundal. Nu se cere acces general la stocare și nu se
  declară permisiunea CAMERA: captura este delegată aplicației camerei.
- Orientarea EXIF este aplicată înaintea trimiterii. Imaginea este pregătită ca
  JPEG, calitate 90, cu latura mare de aproximativ maximum 2560 px pentru consum
  rezonabil de memorie/date. Fișierul pregătit nu păstrează metadatele EXIF originale.
- Poți elimina atașamentul cu ×. Anularea camerei nu trimite o fotografie.
- Iconul imaginii păstrează accesul la selectorul Android pentru imagini și
  documente. Acestea sunt acum atașamente în așteptare, nu se trimit imediat.
- Este disponibil un atașament per mesaj; alegerea altuia îl înlocuiește.
  Schimbarea conversației/dispozitivului elimină atașamentul netrimis.
- Fotografierea/atașarea din vizualizarea unui dispozitiv remote cere mai întâi
  trecerea la „This phone”. Analiza fotografiei necesită un model cu vision.
- Capturile sunt păstrate în spațiul aplicației, nu publicate automat în galerie.
  O fotografie netrimisă nu este un draft durabil după oprirea procesului Android.

## Build

Din rădăcina proiectului, cu același mediu .NET 10 / Android / Rust ca preview3:

```bash
bash scripts/build-android.sh
```

Versiunea Android este `2.4.0-android-preview4`, versionCode `4`. Nu include APK
sau `.so` precompilat. Pentru această schimbare de interfață/cameră nu este
necesară actualizarea unui desktop care rulează deja mesh preview3.

## Verificare

Au trecut parsarea sintactică Rust/C#, verificarea XML/AXAML/TOML/shell și
scriptul existent de contract Avalonia. Configurația providerului a fost verificată
pentru autoritate, acces privat și directorul restrâns al capturii.

**Nu s-a compilat și nu s-a rulat aplicația sau camera pe Android.** Aspectul
randat și comportamentul pe telefon trebuie verificate după build. Probe utile:

1. Fotografie portret și peisaj, cu și fără text; Send numai după confirmare.
2. Anulare, eliminare și înlocuire a fotografiei; revenire din cameră.
3. Alegerea unei imagini/document din selector; schimbarea conversației înainte de Send.
4. Tastatura deschisă, scroll, ecran îngust și setările grupate.
5. Lipsa aplicației camerei / refuzul accesului în aplicația camerei; mesaj de eroare și revenire.

API-uri consultate: [captură delegată Android](https://developer.android.com/media/camera/camera-deprecated/photobasics),
[FileProvider](https://developer.android.com/reference/androidx/core/content/FileProvider),
[Avalonia.Android 11.3.20](https://www.nuget.org/packages/Avalonia.Android/11.3.20).
