# Macsplorer installeren & bijwerken

Macsplorer wordt nu gebouwd als een **echte Windows-installer** die op een
vaste plek installeert en bij een nieuwe versie **in-place bijwerkt** — je hoeft
dus nooit handmatig bestanden of locaties te vervangen, en je instellingen
(labels, slogans, favorieten, enz.) blijven bewaard.

## 1. De installer bouwen

Open een terminal in de projectmap en draai:

```powershell
cargo tauri build
```

Klaar? Dan staat de installer hier:

```
src-tauri\target\release\bundle\nsis\Macsplorer_0.1.0_x64-setup.exe
```

(Het versienummer in de bestandsnaam volgt de versie uit `tauri.conf.json`.)

## 2. Installeren

Dubbelklik op die `..._x64-setup.exe`.

- Installeert **voor jouw gebruiker** (geen administrator nodig) in
  `%LOCALAPPDATA%\Programs\Macsplorer`.
- Maakt een snelkoppeling in het Startmenu (en optioneel op het bureaublad).
- Start daarna gewoon via het Startmenu — niet meer via `cargo tauri dev`.

## 3. Bijwerken (zonder locatie te vervangen)

Wanneer je een nieuwe versie wilt uitbrengen:

1. Verhoog het versienummer in **`src-tauri/tauri.conf.json`**, bijv.
   `"version": "0.1.0"` → `"version": "0.2.0"`.
2. Bouw opnieuw: `cargo tauri build`.
3. Draai de nieuwe `..._x64-setup.exe`.

De installer detecteert de bestaande installatie en **werkt die op dezelfde
plek bij** — geen tweede kopie, geen nieuwe locatie kiezen. Je gegevens blijven
staan omdat ze gekoppeld zijn aan de app-identifier (`com.macsplorer.app`), niet
aan de installatiemap.

## Automatische updates via GitHub (ingebouwd)

Macsplorer kan zichzelf bijwerken vanaf je GitHub-releases — geen
ondertekensleutel nodig.

### Eenmalig instellen
1. Maak een (publieke) GitHub-repo aan, bijv. `jouwnaam/macsplorer`.
2. Open Macsplorer → ⚙️ Instellingen → **Updates** en vul daar je repo in
   als `eigenaar/repo`.

### Een update uitbrengen
1. Verhoog de versie in `src-tauri/tauri.conf.json` (bijv. `0.2.0`).
2. Bouw: `cargo tauri build`.
3. Maak op GitHub een **nieuwe release**:
   - Tag: `v0.2.0` (mag met of zonder `v`).
   - Upload de installer `Macsplorer_0.2.0_x64-setup.exe` als asset.

### Wat de gebruiker ziet
Bij het opstarten (en via de knop "Controleer op updates") checkt Macsplorer de
nieuwste release. Is die nieuwer dan de geïnstalleerde versie, dan verschijnt
**"Update beschikbaar"** met de release-notes en een knop **"Nu bijwerken"**.
Eén klik: de nieuwe installer wordt gedownload en gestart, werkt in-place bij,
en je gegevens blijven bewaard.

> Wil je later ondertekende/geverifieerde updates (extra veiligheid via een
> sleutel), dan kan ik overstappen op de officiële Tauri-updater. Voor
> persoonlijk gebruik volstaat de GitHub-aanpak hierboven prima.

## Problemen?

- Bouwt het niet door de snelle thumbnail-code? Bouw dan met
  `cargo tauri build --no-default-features` (valt terug op de tragere maar
  altijd werkende thumbnails) en stuur me de foutmelding.
- WebView2 ontbreekt? Dat zit standaard op Windows 10/11; zo niet, dan
  installeert de setup het automatisch mee.
