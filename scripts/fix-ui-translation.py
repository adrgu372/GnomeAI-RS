from pathlib import Path
import re

path = Path("src/gui.rs")
text = path.read_text(encoding="utf-8")

fixes = {
    "Analyze the current project și spune-mi ce ar trebui îmbunătățit.":
        "Analyze the current project and tell me what should be improved.",
    "Hub-ul este oprit. Activează-l în Settings și repornește aplicația.":
        "The Hub is disabled. Enable it in Settings and restart the application.",
    "Ask la fiecare comandă": "Ask for every command",
    "Servicel WhatsApp nu răspunde: {error}":
        "WhatsApp service is not responding: {error}",
    "Changes în patch · {} fișiere": "Patch changes · {} files",
    "Ștergi?": "Delete?",
    "Șterge": "Delete",
    "Anulează": "Cancel",
}

for old, new in fixes.items():
    text = text.replace(old, new)

# The one-shot replacement list may be executed more than once while the PR is
# being validated. Normalize English strings whose source is a prefix of the
# translated form so validation reruns remain idempotent.
text = re.sub(r"Diagnostics+", "Diagnostics", text)

# User-facing Romanian that must not survive the GUI translation. Romanian
# natural-language workspace detection and its tests intentionally remain.
forbidden = [
    "Se actualizează permisiunea",
    "Se aplică setările",
    "Se generează un cod QR",
    "Completează JID-ul",
    "Se trimite mesajul",
    "Autentificare finalizată",
    "Autentificare eșuată",
    "Conversațiile recente",
    "Toate conversațiile",
    "Cu ce lucrăm",
    "Căutare web",
    "Notificări desktop",
    "Activează integrarea WhatsApp",
    "Scanează codul QR",
    "CONECTEAZĂ UN CLIENT",
    "DISPOZITIVE ASOCIATE",
    "Politică root",
    "Se așteaptă rezultatul",
    "Modificări în patch",
    "Changes în patch",
    "Ask la fiecare comandă",
    "Servicel WhatsApp",
    "Analizează proiectul curent",
    "Hub-ul este oprit",
    "Ștergi?",
    "Șterge",
    "Anulează",
    "Diagnosticss",
]
leftovers = [item for item in forbidden if item in text]
if leftovers:
    raise SystemExit("Untranslated or malformed GUI strings remain: " + ", ".join(leftovers))

path.write_text(text, encoding="utf-8")
