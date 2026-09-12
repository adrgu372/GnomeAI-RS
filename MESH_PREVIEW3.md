> **Actualizare curentă: preview6.** Vezi [PREVIEW6_UPDATE.md](PREVIEW6_UPDATE.md) pentru UX, auto-grow și copy/paste. Notele de mai jos descriu versiunile anterioare.

> **Actualizare curentă: preview5.** Vezi [PREVIEW5_UPDATE.md](PREVIEW5_UPDATE.md) pentru modificările incluse, comenzile de build și verificările rămase. Notele despre preview-urile anterioare de mai jos sunt istorice.

# GnomeAI 2.4 — mesh preview 3

Surse bazate pe arhiva Android preview2 furnizată de Adi. Include atât clientul
Android, cât și clientul desktop. Nu conține APK, pachet Debian sau biblioteci
native compilate. **Nu s-au executat builduri ori teste Rust/.NET.**

## Implementarea din acest preview

- **Desktop:** selectorul dispozitivului este în fereastra principală. „This PC”
  revine la interfața locală completă; un dispozitiv împerecheat deschide conversațiile
  sale în aceeași fereastră. „Devices” gestionează pairingul, iar „Move session”
  pornește mutarea conversației locale. Configurarea vechilor noduri Linux rămâne
  disponibilă separat în setări.
- **Android:** navigație compactă, mesaje în bule, Markdown pentru răspunsuri,
  reasoning pliabil, buton Send/Stop și meniu separat pentru mutare/sincronizare.
  Atașamentele sunt salvate în workspace-ul conversației. Uneltele `list_files`
  și `write_file` permit continuarea editării unui proiect pe telefon; înlocuirea
  unui fișier cere hash-ul versiunii citite. Nu se adaugă un shell Android.
- **Tor integrat:** Arti 0.46 în nucleul Rust. Fiecare relație primește propriul
  onion service; oricare dispozitiv poate iniția invitația și găzdui conexiunea.
  Pe aceeași conexiune, comenzile și evenimentele circulă în ambele direcții.
  Nu trebuie găzduit un relay GnomeAI sau deschis un port în router.
- **Pairing v2:** invitație de 10 minute, ECDH P-256 cu chei temporare, secret
  aleator din QR și confirmarea acelorași șase cifre pe ambele ecrane. Prima
  cerere autentificată fixează candidatul. Datele aplicației sunt acceptate abia
  după confirmarea ambelor dispozitive. Pairingurile WSS vechi deja confirmate
  sunt păstrate; invitațiile vechi neconfirmate trebuie regenerate.
- **Chei Android:** identitatea și cheile de pairing din `devices.json` sunt
  criptate folosind o cheie AES-GCM din Android Keystore. Datele preview2 se
  migrează la prima deschidere. Cheile de pairing există în memorie în timpul
  folosirii. Cheile Arti, conversațiile și cheile furnizorilor nu sunt mutate
  automat în Keystore prin această modificare.
- **Fișiere:** UUID de workspace, SHA-256, transfer în bucăți de 256 KiB,
  comparație înaintea înlocuirii, copii pentru conflicte și detectarea ștergerilor
  față de ultima sincronizare. Limită 64 MiB/fișier, 512 MiB/workspace, 10.000
  fișiere; cache-ul de transfer are limită de 1 GiB.
- **Mutare:** sursa se blochează, destinația persistă istoricul, se copiază și
  se verifică fișierele, apoi sursa cedează execuția și destinația se activează.
  Etapele și starea transferului sunt persistente; „Resume pending transfer”
  continuă după o întrerupere. Un transfer deja activat nu recopiază fișierele
  vechi la reluarea confirmării finale.
- **Sincronizare automată:** după prima mutare, dispozitivul care a inițiat-o
  verifică modificările la aproximativ 5 secunde, cât timp ambele dispozitive
  sunt conectate și agenții nu lucrează în workspace. Din meniul conversației
  se poate sincroniza manual sau opri sincronizarea. Nu este un flux de disc
  instantaneu; durata depinde de fișiere și de Tor.
- **Fundal Android:** serviciu foreground pornit din activitatea vizibilă,
  notificare persistentă și procesarea evenimentelor independentă de ecran.
  Închiderea activității nu mai distruge nucleul. Nu există restart automat
  după force-stop, pornire la boot sau garanție că Android/Oppo nu suspendă
  rețeaua/procesul. Oprirea aplicației se poate face din controalele Android.

## Build pe calculatorul tău

Reconstruiește **ambele aplicații din această arhivă**. Biblioteca `.so` din
preview2 a fost exclusă: nu este compatibilă cu funcțiile noi. Păstrează copia
veche a proiectului și datele aplicațiilor înainte să testezi preview-ul.

Cerințe: Rust >= 1.91, .NET SDK 10, workload Android, Android SDK și JDK
compatibile cu workload-ul, NDK r28 sau mai nou. Lockfile-ul Rust este actualizat
pentru Arti și rusqlite 0.39; prima compilare va descărca mai multe dependențe.

```bash
export PATH="$HOME/.dotnet:$HOME/.cargo/bin:$PATH"
dotnet --version
dotnet workload install android
cargo install cargo-ndk --locked
export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/30.0.16248370"
# Dacă lipsesc componentele SDK, rulează explicit pe proiectul Android:
dotnet build ui/GnomeAI.UI.Android/GnomeAI.UI.Android.csproj \
  -t:InstallAndroidDependencies -f net10.0-android \
  "-p:AndroidSdkDirectory=$HOME/Android/Sdk" \
  -p:AcceptAndroidSDKLicenses=true
bash scripts/build-android.sh
```

Comanda `InstallAndroidDependencies` de mai sus acceptă licențele SDK. Ruleaz-o
numai dacă le accepți. Pentru NDK folosește directorul instalat la tine.
Scriptul verifică SDK-ul .NET și workload-ul înainte de compilarea Rust și
folosește `cargo ndk ... --platform 26`, nu `-p 26`.

Desktop Linux/Debian, din același director:

```bash
bash scripts/build-deb.sh
```

Acesta reconstruiește executabilele Rust și interfața Avalonia. Nu seta
`GNOMEAI_SKIP_BUILD=1` și nu reutiliza `GNOMEAI_AVALONIA_PUBLISH_DIR` dintr-un build vechi pentru acest upgrade. Păstrează configurația furnizorului
pe fiecare dispozitiv: mutarea nu transferă cheile API. Destinația trebuie să
folosească providerul cerut de sesiune și să aibă modelul disponibil.

## Prima probă între PC și telefon

1. Instalează buildurile noi pe ambele dispozitive și deschide „Devices”.
2. Lasă câmpul WSS gol și apasă „Create invitation” pe unul dintre dispozitive.
   Prima inițializare Tor poate dura câteva minute. Dacă bootstrapul eșuează
   definitiv, închide complet și redeschide aplicația înainte să încerci din nou.
3. Copiază invitația pe celălalt dispozitiv sau deschide linkul QR pe Android.
   Nu există cameră/scanner QR integrat în această interfață; se poate folosi
   scannerul telefonului. Desktopul poate primi codul prin lipire.
4. Compară cele șase cifre afișate și confirmă pe **ambele** dispozitive.
5. Selectează celălalt dispozitiv, deschide un chat și verifică streamingul.
6. Pentru prima mutare folosește o conversație de probă și un proiect mic,
   așteaptă terminarea turei, apoi alege „Move execution and files”.
7. Editează un fișier pe dispozitivul activ, așteaptă sincronizarea, apoi mută
   sesiunea înapoi. Testează separat întreruperea rețelei și reluarea transferului.
8. Închide ecranul Android și verifică disponibilitatea de pe PC. Eventualele
   restricții de baterie trebuie evaluate pe telefonul tău, nu presupuse rezolvate.

## Limite și rezolvarea conflictelor

- Adresa onion este stabilă pentru fiecare pairing și supraviețuiește restartului.
  Tor poate schimba circuitele fără schimbarea acelei adrese. **Nu este implementată
  rotirea adresei onion la fiecare conectare**, nici descoperirea LAN automată.
  „Forget device” revocă relația și oprește serviciul acesteia; nu garantează
  ștergerea fizică a cheilor vechi de pe disc.
- Rețeaua Tor rămâne o infrastructură externă necesară. Traficul către API-urile
  LLM, Brave și pagini web folosește conexiunile HTTPS existente, nu Tor.
- Nu se promite anonimitate totală, audit criptografic sau forward secrecy
  pentru fiecare mesaj. Protocolul de pairing/transport este propriu și necesită
  verificare suplimentară înaintea unei lansări publice.
- `.git`, `.env*`, chei uzuale, directoare de build și linkuri simbolice sunt
  excluse. Aceasta nu detectează toate secretele posibile dintr-un proiect.
  Sincronizarea este pentru conținutul workspace-ului; nu transferă aplicații
  instalate, pachete de sistem, permisiuni POSIX sau procese aflate în execuție.
- Prima replică locală se creează prin mutare; nu se clonează automat toate
  conversațiile și toate workspace-urile dispozitivelor împerecheate.
- Conflictele păstrează originalul și copia primită în `.gnomeai-conflicts`,
  împreună cu JSON care indică numele fișierului. Rezolvarea în acest preview
  este manuală: păstrează copia dorită la calea originală pe ambele dispozitive,
  apoi sincronizează sau reia transferul. Nu există încă editor de conflicte.
  Nu șterge `pending-transfer.json` ca să forțezi activarea pe ambele dispozitive.
- Blocajul aplicării fișierelor coordonează agenții GnomeAI. Nu blochează un editor
  extern și nu este un snapshot atomic al întregului disc; oprește editările
  externe în timpul mutării. Un workspace foarte mare se verifică lent pe Tor.
- Sincronizarea presupune o singură instanță de nucleu pentru același director
  de stare. Dezactivarea sincronizării oprește verificările viitoare; o operație
  deja pornită poate termina fișierul curent.
- Android ARM64 este ABI-ul configurat. Integrarea desktop a fost modificată
  în sursele Avalonia; buildul desktop vizat aici este Linux/Debian. Acest
  preview nu certifică un nou port Windows al nucleului Rust.

## Verificări efectuate

Parsare sintactică Rust/C#, XML/Avalonia, TOML și verificarea shellurilor cu
`bash -n`; scriptul existent `check-avalonia-ui.py` a trecut. Lockfile-ul a fost
rezolvat fără compilare. Sunt adăugate teste Rust pentru blocarea workspace-ului
și păstrarea identității sale prin transfer, **neexecutate** conform cerinței.
Aceste verificări nu confirmă tipurile/API-urile la compilare, aspectul randat,
conectivitatea Tor sau funcționarea serviciului pe telefon.

Referințe de implementare: [Arti](https://docs.rs/arti-client/0.46.0/arti_client/),
[tipurile serviciilor Android](https://developer.android.com/develop/background-work/services/fgs/service-types),
[Android Keystore](https://developer.android.com/privacy-and-security/keystore).
