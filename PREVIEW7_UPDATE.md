# Preview 7 — scanner Android

Preview6 afișa camera, dar pe telefonul testat QR-ul nu era detectat nici după Focus. Cauza exactă de pe acel telefon nu a fost confirmată: nu avem cadrele sau un diagnostic de execuție. În cod, rezultatul nul al decoderului nu producea feedback, iar Focus controla numai focalizarea.

## Modificări

- Preluarea continuă cu buffer pool este înlocuită cu `SetOneShotPreviewCallback`: scannerul cere un cadru nou după procesarea celui precedent. Driverul furnizează copia cadrului; aplicația nu mai returnează buffer-ele NV21 către cameră.
- Callback-urile sunt asociate unei generații de cameră. Cadrele întârziate ale unei sesiuni închise sunt ignorate fără comparații între wrapper-ele managed Camera. Dimensiunile sunt capturate împreună cu cadrul înainte de procesarea în background.
- Decoderul încearcă imaginea completă, inversarea culorilor și, dacă este necesar, un decupaj central pătrat. Sunt păstrate detectarea QR exclusivă, rotația automată și validarea completă a invitației GnomeAI.
- **Scan sharp capture** citește o fotografie JPEG direct din cameră, în memorie, printr-un callback separat de preview. Fotografia are o rezoluție mai mare, în limita dimensiunilor suportate. Nu deschide o aplicație externă și nu salvează fotografia. După un rezultat nereușit, preview-ul și scanarea automată continuă.
- Camera și decoderul au mesaje separate pentru așteptarea unui cadru, scanare activă, un model încă necitibil, QR străin, invitație invalidă/expirată și eroare de procesare.
- **Scanner details · preview7** afișează opțional dimensiunile cadrului, rotația, numărul de cadre, încercările decoderului, punctele detectate, calea preview/JPEG și tipul ultimei erori. Nu include payload-ul, cheile sau fotografia. Numărul punctelor este diagnostic, nu dovada unui QR valid.
- Camera este redeschisă dacă un callback de preview nu sosește în 5 secunde sau o captură nu sosește în 12 secunde. Închiderea scannerului/background-ul invalidează rezultatele în lucru.
- Focus nu mai suprascrie mesajele despre invitații respinse. După autofocus este eliberat lock-ul pentru a permite focalizării continue să reînceapă.

Permisiunea CAMERA rămâne cerută numai la deschiderea explicită a scannerului. Un QR valid se întoarce în fluxul existent: handshake, verificarea codului și confirmarea pairing-ului. Nu se face confirmare automată.

Versiune Android: `2.4.0-preview7`, versionCode `7`. Celelalte modificări din preview6, inclusiv autogrow și copy/paste, sunt păstrate. Desktopul, transportul Rust și Cargo.lock nu sunt modificate în această iterație.

## Verificare efectuată, fără compilarea proiectului

Am executat DLL-urile precompilate **ZXing.Net 0.16.11** și **QRCoder 1.6.0** prin Python.NET și un runtime .NET, separat de aplicație. Nu am compilat sau publicat proiectul și nu am instalat un APK.

Testul a generat un payload sintetic cu structura invitației aplicației (775 bytes, chei publice P-256, endpoint Tor, câmpurile și codificarea JSON/Base64URL), folosind QRCoder ECC Q și 8 pixeli/modul, ca desktopul. Payload-ul nu este o invitație către un dispozitiv real. Imaginea QR generată avea 1064×1064 pixeli. Citirea folosește Gray8 și aceleași încercări de decodare ca scannerul.

**11/11 cazuri au trecut:** QR de 440, 740 și 1000 pixeli într-un cadru de 1920×1080; rotații de 90/180/270 grade; culori inversate; interval de luminanță NV21 16–235; blur Gaussian ușor; JPEG la calitate 95; imagine fără QR (rezultat nul). Pentru cazurile pozitive s-a comparat exact textul decodat cu payload-ul generat.

Au trecut și parsarea statică C#/Rust/XML/AXAML/csproj/TOML/shell și verificarea contractului Avalonia. Aceste verificări nu confirmă compilarea C# Android, trimmerul, callback-urile JNI, camera fizică sau handshake-ul. **Nu declarăm scannerul rezolvat pe telefon înainte de proba reală.**

## Probă pe telefon

1. Build și instalează APK-ul nou. În scanner trebuie să apară **Scanner details · preview7**, ca să verifici că rulează versiunea nouă.
2. Creează o invitație nouă pe desktop, deschide Scan QR și urmărește mesajul de stare. Scanarea automată trebuie să înceapă fără apăsarea Focus.
3. Dacă nu detectează, apasă **Scan sharp capture**. Invitația validă trebuie să închidă scannerul și să înceapă handshake-ul.
4. Dacă nici captura nu detectează, deschide **Scanner details** și trimite textul sau o captură cu mesajul și contoarele, evitând QR-ul/codul invitației în imagine. Aceste date disting lipsa cadrelor de lipsa detecției și de o eroare a decoderului.
5. Verifică și un QR străin, o invitație expirată, anularea, trecerea în background și redeschiderea scannerului.

## Build

Din rădăcina sursei extrase, în mediul Android deja pregătit:

```bash
bash scripts/build-android.sh
```

Nu este necesară recompilarea desktopului pentru această modificare Android. Arhiva conține surse, fără APK sau biblioteci native precompilate.

## Referințe API

- [Microsoft — SetOneShotPreviewCallback](https://learn.microsoft.com/en-us/dotnet/api/android.hardware.camera.setoneshotpreviewcallback?view=net-android-35.0)
- [Microsoft — TakePicture](https://learn.microsoft.com/en-us/dotnet/api/android.hardware.camera.takepicture?view=net-android-35.0)
- [Microsoft — OnPictureTaken](https://learn.microsoft.com/en-us/dotnet/api/android.hardware.camera.ipicturecallback.onpicturetaken?view=net-android-34.0)
- [ZXing.Net 0.16.11](https://github.com/micjahn/ZXing.Net/releases/tag/v0.16.11.0)
