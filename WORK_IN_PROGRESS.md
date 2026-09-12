# Corecții după preview7 — sursă în lucru, NU preview validat

Această arhivă pornește din preview7 și integrează lista din „Pasted markdown(1).md”. Nu este un release: gate-ul Android + desktop nu a trecut. Versiunea aplicației este `2.4.0-preview8-work`, versionCode 8.

## Stadiul listei primite

| Puncte | Modificare în sursă | Ce nu este încă verificat |
| --- | --- | --- |
| 1 | Desktop: Copy response, Select text și Copy selection sunt în meniul contextual; bara de butoane nu mai este adăugată bubble-ului. Selecția normală pe paragrafe rămâne. | Randare/clipboard în Avalonia desktop. |
| 2 | Android: comenzile de copiere/selecție sunt sub conținut. | Poziția și selecția pe telefon. |
| 3 | Revenirea de la un peer poate avea loc și când o cerere remote este în așteptare. Contextul este invalidat, `_selectedPeer` devine null, se cere lista locală și se restaurează sesiunea locală. Finalizarea vechii trimiteri nu mai rescrie noul composer/transcript. | Comutarea repetată cu un telefon offline, în timpul unei trimiteri și cu drafturi. |
| 4–5 | Forget are confirmare. Înainte de ștergerea locală se încearcă `revoke_pairing` pe canalul deja autentificat și criptat, cu acknowledgement. Reconnect-ul este oprit; se elimină peer-ul din store și binding-urile/baseline-urile de sync. Desktopul elimină starea runtime/drafturile cunoscute ale peer-ului și revine la contextul local. | Test pe desktop + telefon, inclusiv în timpul unei operații active. |
| 6 | Android: coadă mărginită, procesare în background și loturi de maximum 256 evenimente la 150 ms. Textul/reasoning-ul se acumulează înaintea unei singure actualizări pe lot. Streaming-ul folosește un control text reutilizat; Markdown-ul complet se reconstruiește la final. Mesajele core irelevante pentru shell nu mai generează Post-uri UI. Overflow-ul cere un snapshot. | Responsivitatea cu răspunsuri lungi pe telefon; limita cozii nu garantează singură un timp maxim de randare. |
| 7 | Confirmările nu se mai retransmit după pairing; duplicatele autentificate nu mai fac save/Changed. Heartbeat conectat: 30 s, timeout: 95 s; backoff până la 120 s. Așteptarea nativă a evenimentelor este blocantă, cu funcție explicită de întrerupere la shutdown. Refresh-ul comun este semnalizat. Auto-sync nu are timer activ fără binding-uri; cu sync activ verifică la 60 s. | Consumul real rămâne **release blocker** până la măsurare. Tor și runtime-ul AI pot avea propriile activități; nu promitem CPU zero sau o anumită autonomie. |
| 8–13 | Aliasurile Avalonia Button/CheckBox/Orientation și System.Environment sunt în sursă. RootMode=All și Android.Content/FileProvider sunt păstrate. | MSBuild Android, trimmer, API resolution pe întregul proiect. |
| 14–18 | Peer-ul/sesiunea sunt capturate în AddApprovalCard și transmise explicit tuturor acțiunilor. Captura accidentală din login handler este eliminată. SendAsync folosește Dictionary explicit. | Click pe approval după comutarea contextului în aplicația desktop. |
| 19 | Fixurile sunt în baza de sursă curentă. Arhivele vechi rămân istorice; nu sunt folosite ca template peste această bază. | Gate-ul complet la următoarea livrare. |
| 20 | `scripts/verify-preview.sh` creează o copie curată fără artefacte, execută ambele build-uri și testul de lifecycle. Creează opțional arhiva de preview numai dacă toate trec, din exact sursa verificată. | **Blocat în mediul de lucru curent**; vezi logurile. |
| 21 | Inventarul de mai jos separă warnings reale de zone ce necesită analizorul Android. | Inventar complet după cele două build-uri. |

**Forget offline:** un peer inaccesibil nu poate primi revocarea după ștergerea cheilor locale. Se șterge local și UI-ul explică faptul că trebuie șters și pe celălalt dispozitiv. Nu există încă o coadă persistentă de revocări offline. Pentru revocare sincronizată, ambele aplicații trebuie actualizate: preview7 nu implementează acest mesaj.

Conversațiile locale și documentele workspace nu sunt șterse prin Forget. Cache-ul de blob-uri comun mai multor peers nu se șterge global.

## Dovezi de verificare

- Codul **GnomeAI.Client** a fost compilat separat cu Roslyn 4.14 și assembly-urile de referință .NET 10.0.0. Compilarea a trecut fără diagnostice. Aceasta nu este o compilare MSBuild a UI-urilor și nu rulează analizorii platformei.
- Testul `tests/DeviceLifecycle` a executat codul comun real pe runtime .NET 10.0.0. Două hub-uri folosesc un relay WebSocket exclusiv loopback și persistență în directoare temporare. Au trecut: idle fără rescrierea identității la vechiul interval de 5 s; confirmare autentificată duplicată fără rescriere; revocare plaintext ignorată; revoke/ack autentificat cu eliminarea ambelor store-uri și a baseline-ului de sync.
- Parsare statică: 96 fișiere C#/Rust, plus XML/AXAML/csproj, TOML, shell. Contract Avalonia: 33 handlers, 29 slash commands, 41 operații core.
- Gate real din copia curată: Android a ieșit cu 1, desktop cu 127. Ambele s-au oprit deoarece `cargo` nu este disponibil în PATH-ul mediului de build. Nu au ajuns la compilarea UI-urilor. Logurile sunt în `verification/`.

Nu s-a instalat un APK, nu s-a rulat UI-ul desktop și nu s-a măsurat bateria. Scannerul rămâne implementarea preview7, cu aliasul Environment corectat; nu avem încă rezultatul probei sale pe telefon.

## Gate înainte de un preview

Pe un sistem cu toolchain-urile complete, din rădăcina proiectului:

```bash
bash scripts/verify-preview.sh /cale/absoluta/GnomeAI-RS-preview.zip
```

Scriptul trebuie să se încheie cu succes. Eșecul oricărui build/test oprește generarea arhivei. Fără argumentul final rulează numai verificările. Logurile sunt păstrate în `verification/`.

Testul comun se poate rula și separat:

```bash
dotnet run --project tests/DeviceLifecycle/DeviceLifecycle.csproj -c Release
```

**Biblioteca nativă trebuie reconstruită** prin scriptul Android: noua așteptare blocantă folosește exportul `gnomeai_interrupt_events`. Nu combina un APK nou cu `.so` din preview7.

## Inventar warnings / verificări rămase

| Zonă | Situație |
| --- | --- |
| GnomeAI.Client | Compilare C# separată fără warnings; analizorii MSBuild nu au fost executați. |
| PairingScanner | Folosește Camera1 depreciat pentru API 26; suprimarea CS0618 este explicită și rămâne de urmărit. |
| MainActivity / insets | Guards pentru window metrics și API 30: de verificat prin build Android și pe min API. |
| DeviceService / notificări | Guards pentru API 33/34 și foreground service: de verificat prin analizor și pe telefon. |
| Nullability Android | Necesită logul compilării reale; nu există încă un număr confirmat de warnings. |
| Rust/dependențe desktop | Build-ul s-a oprit înainte de compilare; warning-urile nu au fost evaluate. |

Pentru bateria în idle: compară aceeași stare Wi-Fi/5G, ecran oprit, fără generare și fără sync activ, pe cel puțin o oră. Înregistrează procentul și activitatea CPU/trezirile; testul de persistență nu este o măsurare energetică.
