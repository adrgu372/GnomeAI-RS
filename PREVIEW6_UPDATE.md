> Notă istorică: scannerul din preview6 nu a funcționat în proba pe telefon. Modificările și limitele iterației curente sunt în PREVIEW7_UPDATE.md.

# Preview 6 — UX rămas și copy/paste

Actualizare pornită din preview5, concentrată pe cerințele încă nerezolvate și pe clarificarea ulterioară despre auto-grow și copierea răspunsurilor. **Surse fără compilare și fără testare pe dispozitive.**

## Modificări noi

| Punct | Modificare în sursă |
| --- | --- |
| 3 — This PC ↔ Phone | Selectorul nu mai așteaptă încărcarea prin rețea a conversațiilor telefonului. Se poate reveni la PC și când se încarcă un snapshot remote. Cererile vechi sunt anulate pentru acel view, iar callback-urile întârziate nu modifică noul context. Ultima conversație remote este reținută separat pentru fiecare peer în fereastra curentă; ciornele rămân separate după peer și conversație. |
| 7 — auto-grow | Bara de input pornește cu un rând. TextBox măsoară rândurile reale, inclusiv wrap și paste, crește până la șase rânduri și apoi folosește scroll intern. Ștergerea textului permite micșorarea la un rând. |
| 9 — QR nedetectat | Rezoluție de preview mărită, dimensiuni citite din parametrii efectivi ai camerei, verificarea cadrelor NV21 și copierea planului Y înainte de reutilizarea bufferului. Două buffere, decodare serializată cu rotație și probă pentru contrast inversat, autofocus și buton Focus. Un cadru incomplet nu mai poate bloca decodarea. Camera este repornită dacă nu mai livrează callback-uri. Rezultatele unei instanțe închise sunt ignorate. QR-ul desktop este mai mare și are opțiunea Enlarge QR. |
| 10 — Files | Iconiță paperclip lângă cameră, etichetată Files / attach a file; păstrează selectorul existent pentru documente și imagini. |
| 12 — stări | Stări comune Connecting, Reconnecting, Connected, Offline cu retry automat, folosite în desktop și Android. Confirmarea pairing-ului și expirarea invitației au mesaje distincte. |
| 11 — API Android | Guards recunoscute de analizor pentru API 30 (window metrics), 33 (notificări) și 34 (foreground specialUse). Tipul serviciului rămâne declarat în manifest, iar apelul API 34 este făcut numai pe versiuni compatibile. |

## Copy/paste din răspuns — Android și desktop

- **Copy response** copiază sursa completă a răspunsului, păstrând Markdown-ul și delimitările rândurilor. Nu copiază titlurile/butoanele interfeței.
- **Select text** deschide răspunsul într-un singur câmp read-only, pentru selecție peste mai multe paragrafe și blocuri. **Copy selection** copiază numai intervalul selectat; selecția goală nu înlocuiește clipboard-ul.
- Selecția folosește un snapshot stabil. Textul nou continuă să fie primit, iar afișarea se actualizează la **Back to response**. Schimbarea explicită a conversației/dispozitivului încheie selecția veche.
- Refresh-urile automate ale transcriptului sunt amânate cât timp acest mod de selecție este activ. Notificările de stare ale peer-ului nu mai reconstruiesc periodic transcriptul Android.
- **Copy code** copiază numai codul, fără limba blocului sau delimitatorii Markdown. Sunt păstrate indentarea și rândurile goale, inclusiv cele de la început. Sunt recunoscute delimitatoare cu trei sau mai multe backtick-uri și cu tilde, fără a închide un bloc de patru backtick-uri la trei backtick-uri din interior.
- Titlurile, citatele, elementele de listă și celulele de tabel sunt și ele selectabile individual. Codul inline nu mai pierde caractere precum `__name__` prin eliminarea globală a underscore-urilor.
- Pe desktop, `/copy` include și textul din bufferul de streaming care încă nu fusese afișat.

Copierea răspunsului întreg și selecția continuă oferă textul Markdown original. Pentru un fragment de cod fără marcaje folosește **Copy code**.

## Păstrate din preview5

Shell-ul desktop unic, pagina Devices fără composer, integrarea conversațiilor remote, Move / Sync, insets Android și mecanismele de pairing/reconnect rămân în proiect. Corecțiile de compilare anterioare sunt păstrate. `Cargo.lock`, manifestul de dependențe Rust și transportul nativ Rust nu au fost modificate în această iterație.

Nu declarăm că probele Wi-Fi ↔ 5G sau pairing fără restart au trecut: aceste scenarii, deja implementate în sursă, încă necesită verificare reală.

## Verificări efectuate

- Parsare sintactică pentru 95 fișiere C#/Rust, plus XML/AXAML/csproj, TOML și shell.
- Contractul Avalonia: 33 handlers, 29 slash commands, 41 operații core.
- Verificarea separării UI/common logic și a păstrării fixurilor CameraFileProvider, RootMode și lockfile.

**Nu s-a executat build, publish, trimmerul, randarea UI sau camera.** Verificările statice nu confirmă rezoluția tuturor tipurilor/API-urilor, lipsa tuturor warning-urilor sau comportamentul pe telefon.

## Probe după build

1. Selectează telefonul, apoi revino imediat la This PC; repetă cu telefonul offline și cu un snapshot în așteptare. Revino la telefon și verifică conversația și draftul separat.
2. Scrie un rând lung până face wrap; adaugă rânduri până la șase; lipește 20 de rânduri; șterge înapoi la unul. Verifică și tastatura deschisă/închisă.
3. Scanează o invitație nouă cu QR normal și Enlarge QR. Probează Focus, refuzul permisiunii, anularea, redeschiderea și o invitație expirată. Mesajul „QR detected, but invitation rejected” distinge validarea payload-ului de lipsa detecției.
4. Copiază un răspuns cu titluri, liste, linkuri și cod. Verifică spațiile, rândurile goale, `__name__`, backslash-uri și delimitatoare Markdown imbricate.
5. Începe Select text în timpul streaming-ului și copiază peste mai multe paragrafe. Așteaptă terminarea răspunsului înainte să apeși Copy selection; selecția trebuie să rămână stabilă. Revino apoi la răspunsul actualizat.
6. Verifică stările la pierderea/revenirea rețelei, pairing fără restart și compatibilitatea pe versiunile Android disponibile.

## Build

Din rădăcina proiectului, folosind mediul pregătit pentru preview5:

```bash
bash scripts/build-android.sh
```

Desktop Linux:

```bash
bash scripts/build-deb.sh
```

Versiune Android: `2.4.0-preview6`, versionCode `6`. Arhiva conține surse, fără APK/DEB/.so precompilat.

## Referințe de implementare

- [Avalonia TextBox 11.3.20 — MinLines/MaxLines și măsurarea editorului](https://github.com/AvaloniaUI/Avalonia/blob/11.3.20/src/Avalonia.Controls/TextBox.cs)
- [ZXing.Net — RGBLuminanceSource](https://github.com/micjahn/ZXing.Net/blob/master/Source/lib/RGBLuminanceSource.cs)
- [ZXing.Net — rotația luminanței](https://github.com/micjahn/ZXing.Net/blob/master/Source/lib/BaseLuminanceSource.cs)
- [Microsoft — guards de compatibilitate a platformelor](https://learn.microsoft.com/en-us/dotnet/fundamentals/code-analysis/quality-rules/ca1416)
