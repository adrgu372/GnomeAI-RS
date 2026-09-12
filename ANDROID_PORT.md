> **Actualizare curentă: preview6.** Vezi [PREVIEW6_UPDATE.md](PREVIEW6_UPDATE.md) pentru UX, auto-grow și copy/paste. Notele de mai jos descriu versiunile anterioare.

> **Actualizare curentă: preview5.** Vezi [PREVIEW5_UPDATE.md](PREVIEW5_UPDATE.md) pentru modificările incluse, comenzile de build și verificările rămase. Notele despre preview-urile anterioare de mai jos sunt istorice.

> Interfața Android și camera foto sunt actualizate în **preview4**: vezi [ANDROID_PREVIEW4.md](ANDROID_PREVIEW4.md).

> Actualizare: această arhivă este **mesh preview3**. Instrucțiunile și limitele curente sunt în [MESH_PREVIEW3.md](MESH_PREVIEW3.md). Textul de mai jos documentează implementarea inițială.

# GnomeAI Android + dispozitive — source preview 1

Această arhivă conține prima implementare în surse, pornind de la GnomeAI-RS
2.4.0, commit `0a124da79fc2506e01b848e04c7f786ce0cb8bf2`.
Nu a fost compilată, conform cerinței. Nu include APK, biblioteci native
compilate sau un relay deja găzduit. Verificarea pe dispozitive rămâne necesară.

## Ce include

| Componentă | Implementare |
| --- | --- |
| Android ARM64 | Proiect .NET 10 Android, Avalonia 11.3.20, Android minimum API 26 |
| Nucleu local | Aceeași buclă Rust de agent, exportată și ca `libgnomeai_core.so`, cu SQLite în directorul privat al aplicației |
| Interfață comună | Conversații, streaming, reasoning, setări, atașamente, aprobări și dispozitive; pe desktop se deschide din butonul Devices |
| Providers | Furnizorii API existenți; configurare locală a cheilor, modelului și reasoning-ului |
| Web | Brave Search și Brave LLM Context, plus citirea paginilor HTTPS |
| Subagenți Android | Execuție în același proces, maximum patru per registru de unelte; fără delegare recursivă |
| Memorie | Memorie locală și combinare explicită în ambele direcții între dispozitive împerecheate |
| Pairing | Invitație temporară, cod QR și link `gnomeai://pair/…`, inițiate de oricare dispozitiv |
| Vizualizare remote | Listă, istoric, text live, reasoning, aprobări în așteptare, trimitere de mesaje și Stop pe sesiunea aleasă |
| Reconectare | Heartbeat, reconnect cu backoff, snapshot la schimbarea conexiunii sau la lipsuri în secvența evenimentelor |
| Cache | Ultima listă și ultimele snapshot-uri primite rămân disponibile offline |
| Mutare | Transfer al istoricului și al dreptului de execuție, cu etape persistente și reluare după întrerupere |
| Relay | Serviciu Rust separat, care transmite mesaje; necesită TLS printr-un reverse proxy |

Desktopul păstrează fereastra existentă. `src/agent_runtime.rs` conține codul
mutat din vechiul executabil; `src/bin/gnomef-agent.rs` este acum punctul de
intrare subțire către biblioteca comună.

## Build Android — de executat pe calculatorul tău

Pregătește Rust stabil cu suport pentru ediția 2024, .NET SDK 10, workload-ul
Android, Android SDK/JDK compatibile cu acesta și Android NDK r28 sau mai nou.
Instalează `cargo-ndk` și configurează locațiile SDK/NDK în mediul de build.

```bash
dotnet workload install android
cargo install cargo-ndk --locked
export ANDROID_NDK_HOME=/cale/catre/android-ndk
bash scripts/build-android.sh
```

Scriptul adaugă targetul Rust `aarch64-linux-android`, construiește biblioteca
în `ui/GnomeAI.UI.Android/native/arm64-v8a/`, apoi publică proiectul Android.
Folosește lockfile-ul Rust și solicită alinierea de 16 KiB a bibliotecii native.

APK-ul rezultat va fi în:

```text
ui/GnomeAI.UI.Android/bin/Release/net10.0-android/android-arm64/publish/
```

Pentru distribuție, configurează cheia ta de semnare Android. Arhiva nu include
keystore-uri sau chei. Buildul ARM64 pentru telefon este singurul target Android
configurat în acest preview; emulatorul x86_64 necesită un ABI suplimentar.

Desktopul trebuie reconstruit din aceeași arhivă pentru noul protocol Devices.
Pentru pachetul Debian se folosește în continuare `scripts/build-deb.sh` și
prerechizitele din README-ul principal.

## Relay pentru rețele diferite, inclusiv 4G/5G

Relay-ul trebuie să fie accesibil prin internet de pe ambele dispozitive.
Nu este suficientă o adresă `localhost` sau un IP privat al PC-ului.

Pe serverul tău:

```bash
cargo build --manifest-path relay/Cargo.toml --release --locked
GNOMEAI_RELAY_BIND=127.0.0.1:8787 relay/target/release/gnomeai-relay
```

Exemplu de configurație Caddy, cu un domeniu al tău și DNS îndreptat către server:

```caddyfile
relay.exemplul-tau.ro {
    reverse_proxy 127.0.0.1:8787
}
```

În Devices introduci `wss://relay.exemplul-tau.ro`. Clientul acceptă doar WSS.
Relay-ul oferă `/health` și `/ws/{channel}`. Are limite de conexiuni, dimensiune
a mesajelor și cozi; nu salvează conversațiile pe disc. Exemplul nu instalează
automat un serviciu de sistem și nu publică un server.

Pairing-ul folosește ECDH P-256 și o dovadă HMAC derivată din secretul invitației.
Mesajele aplicației folosesc AES-GCM și sunt legate de identități, provocări noi
de conexiune și contoare crescătoare. Relay-ul vede metadatele conexiunilor și
dimensiunile traficului. Acesta este un protocol nou, fără audit criptografic
independent; nu presupune forward secrecy. Invitația este o credențială
temporară și trebuie oferită numai dispozitivului pe care vrei să-l împerechezi.

## Utilizare

1. Deschide aplicația pe ambele dispozitive și configurează furnizorul API local.
2. În Devices, creează o invitație folosind adresa WSS a relay-ului.
3. Pe celălalt dispozitiv lipește codul. Pe Android poți deschide linkul QR cu
   aplicația de cameră/scanner a telefonului. Invitația expiră după 10 minute.
4. Așteaptă ca dispozitivul să apară online. Din selectorul de sursă alege
   dispozitivul local sau cel împerecheat, apoi deschide o conversație.
5. Când vizualizezi o conversație remote, Send, Stop și aprobările se referă la
   acea sesiune. Deschiderea ei nu schimbă sesiunea afișată local de gazdă.

Nu există scanner de cameră integrat. Codul text este alternativa disponibilă
în aplicație. Pe Android, rularea locală și conexiunea peer sunt legate de
procesul aplicației; acest preview nu implementează serviciu Android foreground
pentru funcționare continuă cu ecranul stins sau după închiderea aplicației.

## Mutarea execuției

Selectează același furnizor API pe destinație și configurează cheia acolo.
Modelul și nivelul de reasoning ale sesiunii sunt incluse în transfer.
Cheile API, configurațiile secrete MCP și identitatea dispozitivului nu sunt
sincronizate. Destinația trebuie să poată folosi modelul respectiv.

După terminarea turei curente, folosește Move. O conversație locală poate fi
trimisă către un peer; una remote poate fi adusă pe dispozitivul local.

| Etapă | Sursă | Destinație |
| --- | --- | --- |
| Înainte | `active` | Fără copie executabilă |
| Prepare | `transferring`, istoric înghețat | Fără copie executabilă |
| Stage confirmat | `transferring` | `staged`, salvată dar blocată |
| Commit confirmat | `remote`, execuție cedată | `staged` |
| Activate | `remote` | `active` |

Citirea istoricului și înghețarea sursei au loc în aceeași tranzacție SQLite.
O copie activă de pe destinație nu este suprascrisă. La o mutare înapoi, copia
`remote` anterioară poate fi înlocuită cu istoricul mai nou. Operațiile unei
mutări au ID-uri stabile pentru reluare fără repetarea efectelor.

Dacă legătura cade, reconectează ambele dispozitive și apasă **Resume pending
transfer** pe dispozitivul care a inițiat mutarea. Nu șterge datele aplicației,
fișierul `pending-transfer.json` sau pairing-ul în timpul unei mutări.
Nu există deblocare automată prin timeout: ar putea permite executarea pe
ambele dispozitive. Recuperarea când unul dintre dispozitive este pierdut
definitiv nu are încă interfață automată.

Transferul include turele, referințele de compactare și rezultatele complete
ale uneltelor referite prin handle. Imaginile inline și textul extras din
documente rămân în istoricul transferat. Directorul proiectului, fișierele
arbitrare, pachetele de skills și datele de rollback pentru modificări de fișiere
nu se mută. Destinația folosește propriul director `workspace`.

## Limite concrete ale acestui preview

- Snapshot-ul pentru mutare este limitat la 16 MiB, cu rezultate de unelte
  incluse. Atașamentele locale pot avea până la 20 MiB; unele conversații cu
  imagini pot depăși limita de mutare după codarea lor în JSON/base64.
- Atașamentele se adaugă din file picker la conversații locale. Încărcarea unui
  fișier direct într-o sesiune remote nu este implementată.
- PDF-ul este extras local pe Android, fără `pdftotext`. PDF-urile scanate nu au
  OCR în această implementare. Răspunsul la imagini depinde de modelul ales.
- Vizualizarea mobilă folosește text simplu pentru istoric și reasoning;
  formatarea Markdown avansată a desktopului nu a fost portată aici.
- Skills din workspace pot fi citite ca instrucțiuni. Nu există manager mobil
  complet de instalare și actualizare. MCP HTTP rămâne în nucleu, dar editorul
  complet de configurare MCP nu este inclus în interfața mobilă compactă.
- Android nu înregistrează shell, sudo, automatizare desktop, runtimes Codex/
  Claude CLI, MCP stdio, WhatsApp sau Firecrawl local. Pentru astfel de acțiuni,
  sesiunea trebuie executată pe desktop.
- Combinarea memoriei este explicită, până la 1.000 de fapte active într-un lot,
  cu deduplicare de text. Nu este replicare continuă a bazei de memorie și nu
  transferă chei. Ștergerile de memorie nu sunt propagate către celălalt device.
- Mesajele incerte după o deconectare nu sunt retrimise automat. Verifică
  istoricul înainte de a repeta Send. Cozile de mesaje ale unei ture nu sunt
  migrate; mutarea se face când sesiunea nu mai rulează.
- Cache-ul remote este un snapshot, nu o copie executabilă și nici backup
  complet. Forget oprește legătura locală; cache-ul deja primit rămâne pe disc.

## Verificare efectuată

Au trecut verificări de sintaxă pentru 62 de fișiere Rust și 18 C#, parsarea a
7 fișiere XML și 4 fișiere TOML/lock, pregătirea a 48 de instrucțiuni SQL reale
față de schema combinată, verificarea shell și `git diff --check`.
Contractul UI desktop existent a trecut: 33 handlers, 29 slash commands,
41 operații core. Manifestul Cargo a fost citit cu `--locked --no-deps`.
Lockfile-urile au fost generate/actualizate fără build.

Aceste verificări nu validează tipurile Rust/C#, linkarea JNI/FFI, packaging-ul
Android sau comportamentul pe dispozitive. Nu s-au rulat `cargo check`,
`cargo test`, build/publish .NET, APK-uri sau teste de rețea între dispozitive.
În `src/device_store.rs` există teste pentru exclusivitatea execuției,
reluare după restart, mutare dus-întors și respingerea snapshot-urilor invalide;
ele sunt incluse în surse, dar nu au fost executate.

După build, verificarea practică necesară este: chat local cu streaming și Stop,
imagine/document/PDF, Brave în ambele moduri, subagent, pairing în ambele sensuri,
chat remote peste rețele diferite, reconnect în timpul streaming-ului și al unei
aprobări, mutare dus-întors și întreruperea conexiunii la fiecare etapă de mutare.

Referințe: [Avalonia Android](https://docs.avaloniaui.net/docs/platform-specific-guides/android)
și [Brave LLM Context](https://api-dashboard.search.brave.com/documentation/services/llm-context).
