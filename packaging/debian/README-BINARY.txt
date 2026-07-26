GnomeAI-RS — pachet Debian Linux x86_64
========================================

Instalare:

  sudo apt install ./gnomeai-rs_0.1.0-6_amd64.deb

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

Pachetul include runtime-ul oficial Codex app-server pentru logarea OpenAI cu
contul. Logarea Anthropic cu contul necesită comanda oficială `claude`.
WebTool folosește providerii pe bază de API, inclusiv Anthropic Messages API.

Cerințe:

- Linux x86_64 cu glibc 2.39 sau mai nou
- Podman pentru instanța Firecrawl locală
- xdg-utils pentru deschiderea automată a WebTool în browser
- opțional, Node.js 18+ și npm pentru integrarea WhatsApp
