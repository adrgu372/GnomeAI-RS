GnomeAI-RS — pachet Debian Linux x86_64
========================================

Instalare:

  sudo apt install ./gnomeai-rs_0.1.0-8_amd64.deb

Doar instalarea pachetului cere drepturi administrative. După instalare,
Agentul și WebTool rulează ca utilizatorul curent și nu trebuie pornite cu
sudo.

Agent:

  gnomeai-rs /cale/catre/proiect

WebTool:

  gnomeai-webtool

Ambele aplicații apar și în meniul desktop. Datele, cheile API, conversațiile
și memoria sunt salvate sub:

  ${XDG_STATE_HOME:-$HOME/.local/state}/gnomeai-rs

În Agent, /workspace CALE (sau /cd CALE) schimbă proiectul activ, sandboxul și
toate toolurile. Agentul recunoaște și cereri explicite precum „schimbă
folderul în /cale”. Comanda /provider deschide catalogul de provideri, iar
	/websearch activează sau dezactivează uneltele WebSearch/WebFetch. WebTool
	oferă aceleași setări API de provider, model și Web Search.

Modurile de execuție ale Agentului sunt:

- read-only: nu permite modificări;
- normal (implicit): comenzile pot accesa sistemul utilizatorului, nu doar
  workspace-ul, dar fiecare comandă executabilă sau modificatoare cere
  aprobare;
- full-access: același acces la nivelul utilizatorului, fără aprobări.

Niciun mod nu acordă privilegii root și Agentul nu trebuie pornit cu sudo.

Skilluri:

  /skills
  /skill install CALE_SAU_REPOSITORY_GIT
  /skill use NUME
  /skill inspect NUME
  /skill update NUME
  /skill verify NUME
  /skill remove NUME

WebTool are același manager în Setări, iar pe WhatsApp sunt disponibile
`/skills`, `/skill inspect NUME` și `/skill use NUME`. Skillurile folosesc
formatul declarativ SKILL.md, nu rulează automat scripturi și nu acordă
permisiuni suplimentare.

Transcriptul se poate derula cu rotița mouse-ului, PageUp/PageDown și
Ctrl+Up/Ctrl+Down. Ctrl+Home/Ctrl+End sar la început/sfârșit, iar poziția nu
este mutată de răspunsul care continuă să fie generat.
Cu mouse capture activ (implicit), ține apăsat click-stânga pentru selecție,
folosește rotița fără să eliberezi ca să extinzi selecția dincolo de ecran,
apoi eliberează: textul este copiat automat. Ctrl+Y sau /copy recopiază
selecția activă. /mouse off revine la selecția nativă a terminalului.

WebTool are memorie persistentă între conversații. Din Setări poate fi
dezactivată sau limitată la 7, 30, 90 ori 365 de zile; „Fără limită” permite
folosirea tuturor memoriilor. Filtrul nu șterge informațiile vechi, ci doar le
exclude din contextul trimis modelului.

Web Search folosește Firecrawl. Când switch-ul este oprit nu pornește nimic.
Prima căutare cu switch-ul activ pornește automat deployment-ul local prin
Podman rootless și descarcă imaginile oficiale fixate. Pentru diagnostic:

  gnomeai-firecrawl status
  gnomeai-firecrawl logs
  gnomeai-firecrawl stop

Pachetul include runtime-ul oficial Codex și rulează logarea OpenAI prin
`codex login --device-auth`. Logarea Anthropic cu contul necesită comanda
oficială `claude`; sunt detectate inclusiv instalările native din
`~/.local/bin/claude` și `~/.claude/bin/claude`. Pentru o cale personalizată
se poate seta `GNOMEF_CLAUDE_BIN`. WebTool folosește providerii pe bază de API,
inclusiv Anthropic Messages API.

Dependențele Node ale bridge-ului WhatsApp sunt deja incluse în pachet. Din
Setări, butonul „Pornește” așteaptă bridge-ul și afișează automat codul QR.
Erorile reale de pornire sau rețea apar în aceeași secțiune și în:

  ${XDG_STATE_HOME:-$HOME/.local/state}/gnomeai-rs/whatsapp_bridge.log

Cerințe:

- Linux x86_64 cu glibc 2.39 sau mai nou
- Node.js 20 sau mai nou (instalat automat ca dependență a pachetului)
- Podman pentru instanța Firecrawl locală
- xdg-utils pentru deschiderea automată a WebTool în browser
