# Macsplorer

Een lichtgewicht, snelle file explorer voor Windows in **Apple Liquid Glass**-stijl.
Gebouwd met **Tauri** (Rust + de ingebouwde WebView2 van Windows), zodat de app
maar een paar MB groot is en direct opstart — geen zware Electron-runtime.

## Wat zit erin

- **Liquid Glass UI** — matglas, blur, vloeiende kleuren, eigen vensterbalk.
- **Echte bestanden** — lokale schijven, OneDrive en Google Drive (als die als map gesynct zijn) in de zijbalk.
- **Snel zoeken** — parallelle zoekmotor in Rust (de `ignore`-crate, dezelfde als ripgrep).
- **Synoniemen via Excel** — zoek op één term en vind alle gelijke termen (artikelnummer, serie, titel…).
- **Zoekbereik** — kies in welke mappen wél en welke mappen nooit gezocht wordt.
- **Slimme filters** — type, datum, grootte, sorteren; raster- of lijstweergave.
- **Aanpasbaar** — accentkleur, licht/donker, glass-intensiteit, blur, dichtheid, hoekafronding (worden onthouden).

## Eenmalig installeren (alleen Rust nodig)

1. **WebView2** — zit standaard op Windows 10/11. Niets te doen.
2. **Rust** — installeer via https://rustup.rs (download en draai `rustup-init.exe`, klik door met Enter).
3. **Tauri CLI** — open een nieuwe terminal (PowerShell) en draai éénmalig:
   ```powershell
   cargo install tauri-cli --version "^2.0"
   ```

> Geen Node.js nodig — de interface is een los HTML-bestand dat Tauri rechtstreeks laadt.

## Opstarten / bouwen

Ga in de terminal naar de projectmap (waar dit bestand staat):

```powershell
cd pad\naar\Macsplorer

# Ontwikkelen / direct uitproberen (opent het venster):
cargo tauri dev

# Definitieve app + installer bouwen:
cargo tauri build
```

Na `cargo tauri build` vind je het resultaat in:
`src-tauri/target/release/` (de `.exe`) en
`src-tauri/target/release/bundle/` (een MSI/NSIS-installer).

## Synoniemen-bestand (Excel)

Open **Instellingen** (tandwiel rechtsonder) → **Excel kiezen** en selecteer een `.xlsx`.

- **Elke rij** = termen die voor Macsplorer aan elkaar gelijk zijn.
- Zoek je één term, dan vindt Macsplorer bestanden met **élke** term uit die rij.

Voorbeeld (`Voorbeeld_synoniemen.xlsx` zit erbij):

| Term  | Synoniem 1 | Synoniem 2        | Synoniem 3 |
|-------|------------|-------------------|------------|
| Soho  | 1049605    | tvmeubel_160_cm   | SOHO160    |
| Bowie | 1051220    | eettafel_eiken    |            |

Typ je `Soho`, `1049605` óf `tvmeubel_160_cm`, dan komen telkens dezelfde bestanden naar boven.
Wijzig je het Excel-bestand? Macsplorer leest het automatisch opnieuw bij het opstarten.

## Zoekbereik

- Het keuzemenu naast de zoekbalk schakelt tussen **Deze map** en **Zoekmappen**.
- Bij **Zoekmappen** doorzoekt Macsplorer de mappen die je in Instellingen toevoegt (leeg = alle schijven).
- **Uitsluiten** zorgt dat mappen (zoals back-ups of `node_modules`) volledig worden overgeslagen — dat houdt het zoeken snel.

## Preview zonder bouwen

Je kunt `src/index.html` ook gewoon in een browser openen om de look & feel te bekijken.
Dan draait de app in **voorbeeldmodus** met testbestanden; échte bestandstoegang en het
Excel-kiezen werken alleen in de gebouwde app.

## Projectstructuur

```
Macsplorer/
├─ src/
│  └─ index.html          # De volledige Liquid Glass interface (UI + logica)
├─ src-tauri/
│  ├─ src/main.rs         # Rust backend: bestanden lezen, zoeken, Excel inlezen
│  ├─ Cargo.toml          # Rust-afhankelijkheden
│  ├─ tauri.conf.json     # App-instellingen (venster, bundel)
│  ├─ build.rs
│  ├─ capabilities/       # Rechten van het venster
│  └─ icons/              # App-icoon
├─ Voorbeeld_synoniemen.xlsx
└─ README.md
```
