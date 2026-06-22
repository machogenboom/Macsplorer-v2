// Macsplorer - Rust backend
// Lichtgewicht, snel: parallelle bestandszoekfunctie + Excel-synoniemen.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use calamine::{open_workbook_auto, Data, Reader};
use ignore::{WalkBuilder, WalkState};
use serde::Serialize;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

#[derive(Serialize, Clone)]
struct Entry {
    name: String,
    path: String,
    #[serde(rename = "isDir")]
    is_dir: bool,
    size: u64,
    modified: u64, // ms sinds epoch
    created: u64,  // ms sinds epoch
    ext: String,
}

#[derive(Serialize, Clone)]
struct Location {
    name: String,
    path: String,
    kind: String, // "fav" | "drive" | "cloud"
    total: u64,   // bytes (0 = onbekend)
    free: u64,    // bytes (0 = onbekend)
}

fn disk_space(path: &str) -> (u64, u64) {
    let total = fs2::total_space(path).unwrap_or(0);
    let free = fs2::available_space(path).unwrap_or(0);
    (total, free)
}

#[cfg(windows)]
fn volume_label(root: &str) -> String {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let r: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut name = [0u16; 256];
    unsafe {
        let _ = GetVolumeInformationW(PCWSTR(r.as_ptr()), Some(&mut name), None, None, None, None);
    }
    String::from_utf16_lossy(&name)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

fn to_entry(path: &Path) -> Option<Entry> {
    let md = fs::symlink_metadata(path).ok()?;
    let name = path.file_name()?.to_string_lossy().to_string();
    let modified = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let created = md
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let is_dir = md.is_dir();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    Some(Entry {
        name,
        path: path.to_string_lossy().to_string(),
        is_dir,
        size: if is_dir { 0 } else { md.len() },
        modified,
        created,
        ext,
    })
}

/// Inhoud van een map opvragen (niet recursief).
#[tauri::command]
fn read_dir(path: String) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let rd = fs::read_dir(&path).map_err(|e| format!("Kan map niet openen: {e}"))?;
    for e in rd.flatten() {
        if let Some(en) = to_entry(&e.path()) {
            out.push(en);
        }
    }
    Ok(out)
}

/// Schijven en handige startlocaties (incl. OneDrive / Google Drive indien aanwezig).
#[tauri::command]
fn list_locations() -> Vec<Location> {
    let mut v = Vec::new();

    if let Some(home) = dirs::home_dir() {
        for (label, sub) in [
            ("Bureaublad", "Desktop"),
            ("Documenten", "Documents"),
            ("Downloads", "Downloads"),
            ("Afbeeldingen", "Pictures"),
        ] {
            let p = home.join(sub);
            if p.exists() {
                v.push(Location {
                    name: label.into(),
                    path: p.to_string_lossy().into(),
                    kind: "fav".into(),
                    total: 0,
                    free: 0,
                });
            }
        }
        // Cloud-mappen die als gewone map gesynct zijn
        for (label, sub) in [
            ("OneDrive", "OneDrive"),
            ("Google Drive", "Google Drive"),
            ("Google Drive", "My Drive"),
            ("iCloud Drive", "iCloudDrive"),
            ("iCloud Drive", "iCloud Drive"),
        ] {
            let p = home.join(sub);
            if p.exists() {
                // Cloud-mappen tonen GEEN schijfbalk (anders krijg je de
                // grootte van de lokale schijf, wat misleidend is).
                v.push(Location {
                    name: label.into(),
                    path: p.to_string_lossy().into(),
                    kind: "cloud".into(),
                    total: 0,
                    free: 0,
                });
            }
        }
    }

    #[cfg(windows)]
    {
        for c in b'A'..=b'Z' {
            let root = format!("{}:\\", c as char);
            if Path::new(&root).exists() {
                let lbl = volume_label(&root);
                let lower = lbl.to_ascii_lowercase();
                // Cloud-/streaming-schijven (Google Drive, OneDrive, Dropbox, iCloud)
                // rapporteren een virtuele/onjuiste schijfgrootte. Toon daarom geen
                // (misleidende) schijfbalk en markeer ze als 'cloud'.
                let is_cloud = lower.contains("google")
                    || lower.contains("onedrive")
                    || lower.contains("dropbox")
                    || lower.contains("icloud")
                    || lower.contains("drivefs");
                let (kind, total, free) = if is_cloud {
                    ("cloud", 0u64, 0u64)
                } else {
                    let (t, f) = disk_space(&root);
                    ("drive", t, f)
                };
                let name = if lbl.is_empty() {
                    format!("Schijf ({}:)", c as char)
                } else {
                    format!("{} ({}:)", lbl, c as char)
                };
                v.push(Location {
                    name,
                    path: root,
                    kind: kind.into(),
                    total,
                    free,
                });
            }
        }
    }

    #[cfg(not(windows))]
    {
        let (total, free) = disk_space("/");
        v.push(Location {
            name: "Hoofdmap".into(),
            path: "/".into(),
            kind: "drive".into(),
            total,
            free,
        });
    }

    v
}

/// Snelle, parallelle zoekfunctie.
/// - `roots`: mappen waarin gezocht mag worden
/// - `terms`: zoektermen (al uitgebreid met synoniemen door de frontend)
/// - `excludes`: padfragmenten die volledig overgeslagen worden
/// - `max`: maximaal aantal resultaten (0 = standaard 1000)
#[tauri::command]
fn search(roots: Vec<String>, terms: Vec<String>, excludes: Vec<String>, max: usize) -> Vec<Entry> {
    let terms_lc: Vec<String> = terms
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if terms_lc.is_empty() || roots.is_empty() {
        return Vec::new();
    }
    let excl: Vec<String> = excludes
        .iter()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    let limit = if max == 0 { 1000 } else { max };
    let results: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    let found = AtomicUsize::new(0);

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    for root in &roots {
        let mut wb = WalkBuilder::new(root);
        wb.hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .threads(threads);

        wb.build_parallel().run(|| {
            let results = &results;
            let terms_lc = &terms_lc;
            let excl = &excl;
            let found = &found;
            Box::new(move |res| {
                if found.load(Ordering::Relaxed) >= limit {
                    return WalkState::Quit;
                }
                let dent = match res {
                    Ok(d) => d,
                    Err(_) => return WalkState::Continue,
                };
                let p = dent.path();
                let path_lc = p.to_string_lossy().to_lowercase();

                // Uitgesloten map of bestand -> hele subtak overslaan
                if !excl.is_empty() && excl.iter().any(|e| path_lc.contains(e)) {
                    return WalkState::Skip;
                }

                if let Some(name) = p.file_name() {
                    let nl = name.to_string_lossy().to_lowercase();
                    if terms_lc.iter().any(|t| nl.contains(t)) {
                        if let Some(en) = to_entry(p) {
                            let mut g = results.lock().unwrap();
                            if g.len() < limit {
                                g.push(en);
                                found.fetch_add(1, Ordering::Relaxed);
                            } else {
                                return WalkState::Quit;
                            }
                        }
                    }
                }
                WalkState::Continue
            })
        });
    }

    results.into_inner().unwrap_or_default()
}

fn cell_to_string(c: &Data) -> String {
    match c {
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 {
                (*f as i64).to_string()
            } else {
                f.to_string()
            }
        }
        Data::String(s) => s.trim().to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        _ => String::new(),
    }
}

/// Excel-synoniemen inlezen. Elke rij = een groep gelijke termen.
#[tauri::command]
fn parse_aliases(path: String) -> Result<Vec<Vec<String>>, String> {
    let mut wb = open_workbook_auto(&path).map_err(|e| format!("Kan Excel niet openen: {e}"))?;
    let first = wb
        .sheet_names()
        .first()
        .cloned()
        .ok_or("Geen werkblad gevonden")?;
    let range = wb
        .worksheet_range(&first)
        .map_err(|e| format!("Kan werkblad niet lezen: {e}"))?;

    let mut groups: Vec<Vec<String>> = Vec::new();
    for row in range.rows() {
        let g: Vec<String> = row
            .iter()
            .map(cell_to_string)
            .filter(|s| !s.is_empty())
            .collect();
        if g.len() >= 2 {
            groups.push(g);
        }
    }
    Ok(groups)
}

/// Native mappenkiezer.
#[tauri::command]
fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .pick_folder()
        .map(|p| p.to_string_lossy().to_string())
}

/// Native afbeelding-kiezer (voor eigen favorieten-iconen).
#[tauri::command]
fn pick_image() -> Option<String> {
    rfd::FileDialog::new()
        .add_filter("Afbeelding", &["png", "jpg", "jpeg", "webp", "gif", "ico", "bmp"])
        .pick_file()
        .map(|p| p.to_string_lossy().to_string())
}

/// Native Excel-kiezer.
#[tauri::command]
fn pick_excel() -> Option<String> {
    rfd::FileDialog::new()
        .add_filter("Spreadsheet", &["xlsx", "xls", "xlsm", "csv", "ods"])
        .pick_file()
        .map(|p| p.to_string_lossy().to_string())
}

/// Bestand openen met de standaard-app (snel, via ShellExecute op Windows).
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    open::that_detached(&path).map_err(|e| e.to_string())
}

/// Valideert een losse bestands-/mapnaam (geen pad). Weigert padscheidings-
/// tekens, `..`, control-tekens, Windows-gereserveerde namen en afsluitende
/// punt/spatie.
fn valid_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Ongeldige naam".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("Naam mag geen padscheidingstekens bevatten".into());
    }
    if name == "." || name == ".." {
        return Err("Ongeldige naam".into());
    }
    // Reserveert Windows-tekens en control-tekens.
    if name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20) {
        return Err("Naam bevat ongeldige tekens".into());
    }
    // Afsluitende punt of spatie is op Windows problematisch.
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("Naam mag niet eindigen op een punt of spatie".into());
    }
    // Gereserveerde apparaatnamen (CON, PRN, AUX, NUL, COM1-9, LPT1-9),
    // ook met extensie (bv. "CON.txt").
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
    ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.len() == 4
        && stem.as_bytes()[3].is_ascii_digit()
        && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err("Gereserveerde naam".into());
    }
    Ok(())
}

/// Bestand of map hernoemen. Geeft het nieuwe pad terug.
#[tauri::command]
fn rename(path: String, new_name: String) -> Result<String, String> {
    let trimmed = new_name.trim();
    valid_entry_name(trimmed)?;
    let p = Path::new(&path);
    let parent = p.parent().ok_or("Geen bovenliggende map")?;
    let dest = parent.join(trimmed);
    if dest.exists() {
        return Err("Er bestaat al een bestand met die naam".into());
    }
    fs::rename(&path, &dest).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

/// Afmetingen (breedte/hoogte) van afbeeldingen ophalen.
/// Leest alleen de header -> snel. Voor filteren op resolutie.
#[tauri::command]
fn image_sizes(paths: Vec<String>) -> Vec<(String, u32, u32)> {
    paths
        .iter()
        .filter_map(|p| image::image_dimensions(p).ok().map(|(w, h)| (p.clone(), w, h)))
        .collect()
}

/// Haalt een thumbnail op via Windows' eigen Shell-cache (zoals Verkenner doet).
/// Veel sneller dan zelf decoderen en werkt voor afbeeldingen, video's, PDF's enz.
#[cfg(windows)]
fn shell_thumb_to_png(path: &str, edge: u32, out: &Path, thumb_only: bool) -> Result<(), String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
        SIIGBF_THUMBNAILONLY,
    };

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let work = (|| -> Result<(), String> {
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                .map_err(|e| e.to_string())?;
            let factory: IShellItemImageFactory = item.cast().map_err(|e| e.to_string())?;
            let size = SIZE {
                cx: edge as i32,
                cy: edge as i32,
            };
            let flags = if thumb_only {
                SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK
            } else {
                SIIGBF_BIGGERSIZEOK
            };
            let hbmp = factory.GetImage(size, flags).map_err(|e| e.to_string())?;

            let mut bm = BITMAP::default();
            GetObjectW(
                HGDIOBJ(hbmp.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut _),
            );
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            if w <= 0 || h <= 0 {
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
                return Err("lege thumbnail".into());
            }

            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h; // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = 0; // BI_RGB

            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            let hdc: HDC = CreateCompatibleDC(None);
            let lines = GetDIBits(
                hdc,
                hbmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(hdc);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            if lines == 0 {
                return Err("GetDIBits faalde".into());
            }
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2); // BGRA -> RGBA
            }
            let img = image::RgbaImage::from_raw(w as u32, h as u32, buf).ok_or("buffer")?;
            img.save(out).map_err(|e| e.to_string())?;
            Ok(())
        })();
        CoUninitialize();
        work
    }
}

/// Genereert (en cachet) een kleine thumbnail voor een afbeelding.
/// Retourneert het pad naar de gecachte thumbnail (PNG). De WebView laadt
/// dan alleen dit mini-bestand -> snel en zuinig, ook in grote mappen.
/// De ratio van de originele afbeelding blijft behouden.
#[tauri::command]
async fn thumbnail(path: String, max: u32) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let md = fs::metadata(&path).map_err(|e| e.to_string())?;
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let size = md.len();
        let edge = max.clamp(64, 512);

        let mut h = DefaultHasher::new();
        path.hash(&mut h);
        mtime.hash(&mut h);
        size.hash(&mut h);
        edge.hash(&mut h);
        let key = h.finish();

        let dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("macsplorer_thumbs");
        let _ = fs::create_dir_all(&dir);
        let out = dir.join(format!("{key:016x}.png"));

        if out.exists() {
            return Ok(out.to_string_lossy().to_string());
        }

        // 1) Snel: Windows' eigen thumbnail-cache (zoals Verkenner).
        #[cfg(all(windows, feature = "shellthumb"))]
        {
            if shell_thumb_to_png(&path, edge, &out, true).is_ok() && out.exists() {
                return Ok(out.to_string_lossy().to_string());
            }
        }

        // 2) Terugval: zelf decoderen en verkleinen.
        let img = image::open(&path).map_err(|e| e.to_string())?;
        let thumb = img.thumbnail(edge, edge);
        thumb.save(&out).map_err(|e| e.to_string())?;
        Ok(out.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- Bestandsbewerkingen (kopiëren / verplaatsen / zip / spiegelen / map) ----

fn unique_path(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = dest
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let mut i = 2;
    loop {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
        i += 1;
    }
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for e in fs::read_dir(src).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let p = e.path();
        let d = dest.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &d)?;
        } else {
            fs::copy(&p, &d).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn copy_into(src: &Path, dest_dir: &Path) -> Result<(), String> {
    let name = src.file_name().ok_or("Ongeldige bestandsnaam")?;
    let dest = unique_path(&dest_dir.join(name));
    if src.is_dir() {
        copy_dir(src, &dest)
    } else {
        fs::copy(src, &dest).map(|_| ()).map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn copy_paths(paths: Vec<String>, dest_dir: String) -> Result<(), String> {
    let dd = Path::new(&dest_dir);
    for p in &paths {
        copy_into(Path::new(p), dd)?;
    }
    Ok(())
}

#[tauri::command]
fn move_paths(paths: Vec<String>, dest_dir: String) -> Result<(), String> {
    let dd = Path::new(&dest_dir);
    for p in &paths {
        let src = Path::new(p);
        let name = src.file_name().ok_or("Ongeldige bestandsnaam")?;
        let dest = unique_path(&dd.join(name));
        if fs::rename(src, &dest).is_err() {
            // ander volume: kopieer + verwijder
            copy_into(src, dd)?;
            if src.is_dir() {
                fs::remove_dir_all(src).ok();
            } else {
                fs::remove_file(src).ok();
            }
        }
    }
    Ok(())
}

#[tauri::command]
fn create_folder(parent: String, name: String) -> Result<String, String> {
    let nm = name.trim();
    valid_entry_name(nm)?;
    let mut dest = Path::new(&parent).join(nm);
    let mut i = 2;
    while dest.exists() {
        dest = Path::new(&parent).join(format!("{nm} ({i})"));
        i += 1;
    }
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

/// Schrijft een ZIP-pakket (gebruikt voor de minimale Office-bestanden).
fn write_zip_package(dest: &Path, parts: &[(&str, &str)]) -> Result<(), String> {
    use std::io::Write;
    let f = fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut zw = zip::ZipWriter::new(f);
    let opt = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in parts {
        zw.start_file(*name, opt).map_err(|e| e.to_string())?;
        zw.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
    }
    zw.finish().map_err(|e| e.to_string())?;
    Ok(())
}

// Minimale, geldige OOXML-pakketten (openen schoon in Office).
const OOXML_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="TARGET"/></Relationships>"#;

fn docx_parts() -> Vec<(&'static str, String)> {
    vec![
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.into()),
        ("_rels/.rels", OOXML_RELS.replace("TARGET", "word/document.xml")),
        ("word/document.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p/><w:sectPr/></w:body></w:document>"#.into()),
    ]
}

fn xlsx_parts() -> Vec<(&'static str, String)> {
    vec![
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.into()),
        ("_rels/.rels", OOXML_RELS.replace("TARGET", "xl/workbook.xml")),
        ("xl/workbook.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Blad1" sheetId="1" r:id="rId1"/></sheets></workbook>"#.into()),
        ("xl/_rels/workbook.xml.rels", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.into()),
        ("xl/worksheets/sheet1.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#.into()),
    ]
}

fn pptx_parts() -> Vec<(&'static str, String)> {
    let rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    vec![
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/></Types>"#.into()),
        ("_rels/.rels", OOXML_RELS.replace("TARGET", "ppt/presentation.xml")),
        ("ppt/presentation.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#.into()),
        ("ppt/_rels/presentation.xml.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{rel}/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId2" Type="{rel}/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="{rel}/theme" Target="theme/theme1.xml"/></Relationships>"#)),
        ("ppt/slideMasters/slideMaster1.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#.into()),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{rel}/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="{rel}/theme" Target="../theme/theme1.xml"/></Relationships>"#)),
        ("ppt/slideLayouts/slideLayout1.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1"><p:cSld name="Leeg"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#.into()),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{rel}/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#)),
        ("ppt/slides/slide1.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#.into()),
        ("ppt/slides/_rels/slide1.xml.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{rel}/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#)),
        ("ppt/theme/theme1.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office"><a:themeElements><a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Office"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Office"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#.into()),
    ]
}

/// Maakt een nieuw bestand aan. Type wordt afgeleid uit de extensie van `name`.
/// docx/xlsx/pptx krijgen een geldig (leeg) Office-pakket; overige extensies een
/// leeg bestand. Geeft het uiteindelijke pad terug (uniek bij naamconflict).
#[tauri::command]
fn create_file(parent: String, name: String) -> Result<String, String> {
    let nm = name.trim();
    valid_entry_name(nm)?;
    // Uniek pad bepalen.
    let (stem, ext) = match nm.rfind('.') {
        Some(i) if i > 0 => (&nm[..i], &nm[i..]),
        _ => (nm, ""),
    };
    let mut dest = Path::new(&parent).join(nm);
    let mut i = 2;
    while dest.exists() {
        dest = Path::new(&parent).join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    let ext_lc = ext.trim_start_matches('.').to_ascii_lowercase();
    match ext_lc.as_str() {
        "docx" | "xlsx" | "pptx" => {
            let owned = match ext_lc.as_str() {
                "docx" => docx_parts(),
                "xlsx" => xlsx_parts(),
                _ => pptx_parts(),
            };
            let parts: Vec<(&str, &str)> = owned.iter().map(|(n, c)| (*n, c.as_str())).collect();
            write_zip_package(&dest, &parts)?;
        }
        _ => {
            // txt, md, csv, rtf, en alle overige: leeg bestand.
            fs::write(&dest, b"").map_err(|e| e.to_string())?;
        }
    }
    Ok(dest.to_string_lossy().to_string())
}

#[tauri::command]
fn delete_paths(paths: Vec<String>) -> Result<(), String> {
    trash::delete_all(paths.iter().map(|p| p.as_str())).map_err(|e| e.to_string())
}

fn add_to_zip<W: std::io::Write + std::io::Seek>(
    z: &mut zip::ZipWriter<W>,
    path: &Path,
    base: &Path,
    opt: &zip::write::SimpleFileOptions,
) -> Result<(), String> {
    let rel = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if path.is_dir() {
        z.add_directory(format!("{rel}/"), *opt).map_err(|e| e.to_string())?;
        for e in fs::read_dir(path).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            add_to_zip(z, &e.path(), base, opt)?;
        }
    } else {
        z.start_file(rel, *opt).map_err(|e| e.to_string())?;
        let data = fs::read(path).map_err(|e| e.to_string())?;
        z.write_all(&data).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn zip_paths(paths: Vec<String>, zip_path: String) -> Result<String, String> {
    let dest = unique_path(Path::new(&zip_path));
    let file = fs::File::create(&dest).map_err(|e| e.to_string())?;
    let mut zw = zip::ZipWriter::new(file);
    let opt = zip::write::SimpleFileOptions::default();
    for p in &paths {
        let path = Path::new(p);
        let base = path.parent().unwrap_or(Path::new(""));
        add_to_zip(&mut zw, path, base, &opt)?;
    }
    zw.finish().map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

fn replace_ci(hay: &str, from: &str, to: &str) -> String {
    let lower = hay.to_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while let Some(pos) = lower[i..].find(from) {
        let abs = i + pos;
        out.push_str(&hay[i..abs]);
        out.push_str(to);
        i = abs + from.len();
    }
    out.push_str(&hay[i..]);
    out
}

fn swap_lr(stem: &str) -> String {
    let lower = stem.to_lowercase();
    if lower.contains("rechts") {
        replace_ci(stem, "rechts", "links")
    } else if lower.contains("links") {
        replace_ci(stem, "links", "rechts")
    } else {
        format!("{stem}_gespiegeld")
    }
}

/// Spiegelt een afbeelding horizontaal en past de naam aan (rechts <-> links).
#[tauri::command]
fn mirror_image(path: String) -> Result<String, String> {
    let p = Path::new(&path);
    let img = image::open(&path).map_err(|e| e.to_string())?;
    let flipped = img.fliph();
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "png".into());
    // image-crate kan webp/ico/heic niet schrijven -> val terug op png
    let out_ext = match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tiff" | "tif" => ext,
        _ => "png".to_string(),
    };
    let parent = p.parent().ok_or("Geen bovenliggende map")?;
    let dest = unique_path(&parent.join(format!("{}.{}", swap_lr(&stem), out_ext)));
    flipped.save(&dest).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

/// Totale grootte van een map (recursief). Draait op de achtergrond.
#[tauri::command]
async fn dir_size(path: String) -> u64 {
    tauri::async_runtime::spawn_blocking(move || {
        let mut total: u64 = 0;
        for e in WalkBuilder::new(&path)
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .build()
            .flatten()
        {
            if let Ok(md) = e.metadata() {
                if md.is_file() {
                    total += md.len();
                }
            }
        }
        total
    })
    .await
    .unwrap_or(0)
}

/// Geeft het pad van het eerste afbeeldings-/video-/pdf-bestand in een map
/// (voor een mini-voorbeeld in het map-icoon). None als er geen is.
#[tauri::command]
fn folder_preview(path: String) -> Option<String> {
    let exts = [
        "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "ico", "mp4", "mov", "mkv", "avi",
        "webm", "m4v", "wmv", "pdf",
    ];
    let rd = fs::read_dir(&path).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file() {
            if let Some(ext) = p.extension().map(|s| s.to_string_lossy().to_lowercase()) {
                if exts.contains(&ext.as_str()) {
                    return Some(p.to_string_lossy().to_string());
                }
            }
        }
    }
    None
}

/// Bestandsinfo ophalen voor een lijst paden (voor de label-weergave).
#[tauri::command]
fn stat_paths(paths: Vec<String>) -> Vec<Entry> {
    paths
        .iter()
        .filter_map(|p| to_entry(Path::new(p)))
        .collect()
}

/// Schrijft een klein sleep-icoon naar de cache en geeft het pad terug
/// (nodig als voorbeeld-afbeelding bij het naar buiten slepen van bestanden).
#[tauri::command]
fn drag_icon() -> Result<String, String> {
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("macsplorer_thumbs");
    let _ = fs::create_dir_all(&dir);
    let out = dir.join("drag_icon.png");
    if !out.exists() {
        fs::write(&out, include_bytes!("../icons/128x128.png")).map_err(|e| e.to_string())?;
    }
    Ok(out.to_string_lossy().to_string())
}

/// Zoekt het LocalSend-programma op bekende installatieplekken (i.p.v. blind via
/// PATH, wat zowel onbetrouwbaar als een PATH-hijack-risico is).
#[cfg(windows)]
fn find_localsend_exe() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let names = ["localsend_app.exe", "LocalSend.exe", "localsend.exe"];
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(&la).join("Programs").join("LocalSend"));
    }
    for v in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(pf) = std::env::var(v) {
            dirs.push(PathBuf::from(pf).join("LocalSend"));
        }
    }
    for d in &dirs {
        for n in &names {
            let p = d.join(n);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Geeft de geselecteerde bestanden door aan de (al draaiende) LocalSend-app.
/// LocalSend opent dan zijn eigen verzendscherm met de apparaten in het netwerk;
/// Macsplorer doet zelf geen apparaat-detectie meer.
#[tauri::command]
fn localsend(paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Err("Niets geselecteerd".into());
    }
    #[cfg(windows)]
    {
        // 1) Bekend installatiepad (betrouwbaar + veilig).
        if let Some(exe) = find_localsend_exe() {
            let mut cmd = std::process::Command::new(exe);
            for p in &paths {
                cmd.arg(p);
            }
            return cmd.spawn().map(|_| ()).map_err(|e| e.to_string());
        }
        // 2) Terugval: via PATH (gebruikelijke exe-namen).
        for name in ["localsend_app.exe", "localsend.exe", "localsend"] {
            let mut cmd = std::process::Command::new(name);
            for p in &paths {
                cmd.arg(p);
            }
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
        return Err(
            "De LocalSend-app is niet gevonden. Installeer LocalSend (localsend.org) \
             en zorg dat de app draait."
                .into(),
        );
    }
    #[cfg(not(windows))]
    {
        let mut cmd = std::process::Command::new("localsend");
        for p in &paths {
            cmd.arg(p);
        }
        cmd.spawn()
            .map(|_| ())
            .map_err(|_| "De LocalSend-app is niet gevonden.".to_string())
    }
}

/// Pictogram van een toepassing (.exe/.msi) via de Shell, gecachet.
#[tauri::command]
async fn app_icon(path: String, max: u32) -> Result<String, String> {
    #[cfg(windows)]
    {
        tauri::async_runtime::spawn_blocking(move || {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let md = fs::metadata(&path).map_err(|e| e.to_string())?;
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let edge = max.clamp(32, 256);
            let mut h = DefaultHasher::new();
            path.hash(&mut h);
            mtime.hash(&mut h);
            edge.hash(&mut h);
            "app".hash(&mut h);
            let key = h.finish();
            let dir = dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("macsplorer_thumbs");
            let _ = fs::create_dir_all(&dir);
            let out = dir.join(format!("app{key:016x}.png"));
            if out.exists() {
                return Ok(out.to_string_lossy().to_string());
            }
            shell_thumb_to_png(&path, edge, &out, false)?;
            Ok(out.to_string_lossy().to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(windows))]
    {
        let _ = max;
        Err("Alleen op Windows".into())
    }
}

#[derive(Serialize, Clone)]
struct LsDevice {
    alias: String,
    ip: String,
    port: u16,
    protocol: String,
    fingerprint: String,
    #[serde(rename = "deviceType")]
    device_type: String,
}

/// Het info-blok dat Macsplorer over zichzelf uitstuurt (announce + registratie-antwoord).
/// `http_port` is de poort waarop ONZE HTTP-listener draait, zodat andere
/// apparaten daar hun registratie naartoe sturen (en niet naar 53317, dat
/// mogelijk al door de echte LocalSend-app bezet is).
fn ls_self_info(http_port: u16) -> serde_json::Value {
    serde_json::json!({
        "alias": "Macsplorer",
        "version": "2.0",
        "deviceModel": "PC",
        "deviceType": "desktop",
        "fingerprint": format!("macsplorer-{}", std::process::id()),
        "port": http_port,
        "protocol": "http",
        "download": false
    })
}

/// Voegt een ontdekt apparaat toe (uit multicast of uit een HTTP-registratie),
/// met ontdubbeling op fingerprint en filtering van onszelf.
fn ls_add_device(
    found: &std::sync::Mutex<Vec<LsDevice>>,
    seen: &std::sync::Mutex<std::collections::HashSet<String>>,
    v: &serde_json::Value,
    ip: String,
) {
    let alias = v.get("alias").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if alias.is_empty() || alias == "Macsplorer" {
        return;
    }
    let fp = v.get("fingerprint").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let key = if fp.is_empty() { format!("{alias}@{ip}") } else { fp.clone() };
    {
        let mut s = match seen.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if !s.insert(key) {
            return;
        }
    }
    let dev = LsDevice {
        alias,
        ip,
        port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(53317) as u16,
        protocol: v.get("protocol").and_then(|x| x.as_str()).unwrap_or("http").to_string(),
        fingerprint: fp,
        device_type: v.get("deviceType").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    };
    if let Ok(mut f) = found.lock() {
        f.push(dev);
    }
}

/// Maakt een TCP-listener. Probeert eerst de standaard LocalSend-poort 53317;
/// lukt dat niet (de echte LocalSend-app gebruikt hem al), dan een vrije poort
/// die de OS toewijst. Geeft de listener én de werkelijke poort terug, zodat we
/// die in onze announce kunnen zetten en apparaten daar naartoe registreren.
fn ls_tcp_listener() -> std::io::Result<(std::net::TcpListener, u16)> {
    use socket2::{Domain, Socket, Type};
    let make = |port: u16| -> std::io::Result<std::net::TcpListener> {
        let sock = Socket::new(Domain::IPV4, Type::STREAM, None)?;
        let _ = sock.set_reuse_address(true);
        let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        sock.bind(&addr.into())?;
        sock.listen(128)?;
        Ok(sock.into())
    };
    // Eerst 53317 (zodat standaard-clients ons zonder meer vinden); anders een
    // vrije poort (port 0 -> OS kiest), om naast de LocalSend-app te draaien.
    let listener = make(53317).or_else(|_| make(0))?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Handelt één inkomende HTTP-verbinding af. Andere LocalSend-apparaten doen een
/// POST naar /api/localsend/v2/register (als reactie op onze announce) of vragen
/// /api/localsend/v2/info op. In beide gevallen antwoorden we met ons eigen info-blok.
fn ls_handle_http(
    stream: &mut std::net::TcpStream,
    ip: &str,
    our_info: &str,
    found: &std::sync::Mutex<Vec<LsDevice>>,
    seen: &std::sync::Mutex<std::collections::HashSet<String>>,
) {
    use std::io::{Read, Write};
    use std::time::Duration;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                    let clen = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= pos + 4 + clen {
                        break;
                    }
                }
                if data.len() > 65536 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
        let body = &data[pos + 4..];
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
            ls_add_device(found, seen, &v, ip.to_string());
        }
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        our_info.len(),
        our_info
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn ls_discover_blocking() -> Vec<LsDevice> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let found: Arc<Mutex<Vec<LsDevice>>> = Arc::new(Mutex::new(Vec::new()));
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // --- TCP-listener: vangt HTTP-registraties van apparaten die op onze
    //     announce reageren (telefoons, andere pc's). Draait op 53317, of op
    //     een vrije poort als de echte LocalSend-app die al bezet houdt. We
    //     adverteren de werkelijke poort, zodat registraties bij ÓNS aankomen. ---
    let http_port = match ls_tcp_listener() {
        Ok((listener, port)) => {
            let _ = listener.set_nonblocking(true);
            let f2 = found.clone();
            let s2 = seen.clone();
            let info2 = ls_self_info(port).to_string();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_millis(3200);
                while Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, peer)) => {
                            let ip = peer.ip().to_string();
                            ls_handle_http(&mut stream, &ip, &info2, &f2, &s2);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(40));
                        }
                        Err(_) => break,
                    }
                }
            });
            port
        }
        Err(_) => 53317,
    };

    let mut announce_val = ls_self_info(http_port);
    announce_val["announce"] = serde_json::Value::Bool(true);
    let announce = announce_val.to_string();

    // --- UDP-multicast: announce uitsturen + announces van anderen opvangen. ---
    let udp_sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok();
    let bind: SocketAddr = "0.0.0.0:53317".parse().unwrap();
    let bound = udp_sock.as_ref().map(|sock| {
        let _ = sock.set_reuse_address(true);
        sock.bind(&bind.into()).is_ok()
    }).unwrap_or(false);
    if let (Some(sock), true) = (udp_sock, bound) {
        let _ = sock.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 167), &Ipv4Addr::UNSPECIFIED);
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        let udp: UdpSocket = sock.into();
        // Meerdere keren announcen zodat ook net-gestarte apparaten reageren.
        let _ = udp.send_to(announce.as_bytes(), "224.0.0.167:53317");
        let start = Instant::now();
        let mut buf = [0u8; 4096];
        let mut last_announce = Instant::now();
        while start.elapsed() < Duration::from_millis(3000) {
            if last_announce.elapsed() > Duration::from_millis(1000) {
                let _ = udp.send_to(announce.as_bytes(), "224.0.0.167:53317");
                last_announce = Instant::now();
            }
            if let Ok((n, src)) = udp.recv_from(&mut buf) {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                    ls_add_device(&found, &seen, &v, src.ip().to_string());
                }
            }
        }
    } else {
        // Poort bezet (waarschijnlijk de LocalSend-app zelf): geef de TCP-listener
        // nog even de tijd om registraties op te vangen.
        std::thread::sleep(Duration::from_millis(3200));
    }

    found.lock().map(|f| f.clone()).unwrap_or_default()
}

#[tauri::command]
async fn localsend_discover() -> Vec<LsDevice> {
    tauri::async_runtime::spawn_blocking(ls_discover_blocking)
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn localsend_send(device: serde_json::Value, paths: Vec<String>) -> Result<(), String> {
    let ip = device.get("ip").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let port = device.get("port").and_then(|x| x.as_u64()).unwrap_or(53317) as u16;
    let protocol = device.get("protocol").and_then(|x| x.as_str()).unwrap_or("http").to_string();
    if ip.is_empty() {
        return Err("Geen apparaat".into());
    }
    let base = format!("{protocol}://{ip}:{port}");
    // LocalSend gebruikt per ontwerp self-signed certificaten; daarom wordt de
    // standaard certificaatvalidatie hier overgeslagen. LET OP: dit biedt geen
    // bescherming tegen een man-in-the-middle op het LAN. Aanbevolen vervolg:
    // verifieer de door het protocol meegestuurde `fingerprint` (SHA-256 van het
    // certificaat) via een eigen rustls-verifier i.p.v. elk certificaat te
    // accepteren.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let mut files = serde_json::Map::new();
    let mut meta: Vec<(String, String)> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let path = Path::new(p);
        let md = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            continue;
        }
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let id = format!("f{i}");
        files.insert(
            id.clone(),
            serde_json::json!({"id":id,"fileName":name,"size":md.len(),"fileType":"application/octet-stream"}),
        );
        meta.push((id, p.clone()));
    }
    if files.is_empty() {
        return Err("Geen verzendbare bestanden (mappen worden niet ondersteund)".into());
    }
    let info = serde_json::json!({"alias":"Macsplorer","version":"2.0","deviceModel":"PC","deviceType":"desktop","fingerprint":format!("macsplorer-{}",std::process::id()),"port":53317,"protocol":protocol,"download":false});
    let body = serde_json::json!({"info":info,"files":files});
    let resp = client
        .post(format!("{base}/api/localsend/v2/prepare-upload"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() == 401 {
        return Err("Het andere apparaat vraagt om een PIN (nog niet ondersteund)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("Voorbereiden mislukt ({})", resp.status()));
    }
    let pr: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let session = pr.get("sessionId").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let tokens = pr.get("files").cloned().unwrap_or(serde_json::json!({}));
    for (id, p) in meta {
        let token = tokens.get(&id).and_then(|x| x.as_str()).unwrap_or("").to_string();
        if token.is_empty() {
            continue;
        }
        let data = fs::read(&p).map_err(|e| e.to_string())?;
        let url = format!("{base}/api/localsend/v2/upload?sessionId={session}&fileId={id}&token={token}");
        let up = client.post(url).body(data).send().await.map_err(|e| e.to_string())?;
        if !up.status().is_success() {
            return Err(format!("Uploaden mislukt ({})", up.status()));
        }
    }
    Ok(())
}

/// Opent het echte Windows-eigenschappenvenster (alle tabs).
#[cfg(windows)]
#[tauri::command]
fn shell_properties(path: String) -> Result<(), String> {
    std::thread::spawn(move || {
        use windows::core::PCWSTR;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};
        let wpath: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let verb: Vec<u16> = "properties".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let mut sei = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_INVOKEIDLIST,
                lpVerb: PCWSTR(verb.as_ptr()),
                lpFile: PCWSTR(wpath.as_ptr()),
                nShow: 1,
                ..Default::default()
            };
            let _ = ShellExecuteExW(&mut sei);
        }
    });
    Ok(())
}
#[cfg(not(windows))]
#[tauri::command]
fn shell_properties(_path: String) -> Result<(), String> {
    Err("Alleen op Windows".into())
}

/// Wijzigt het volume-label (naam) van een schijf.
#[cfg(windows)]
#[tauri::command]
fn set_drive_label(path: String, label: String) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::SetVolumeLabelW;
    let root: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let lab: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        SetVolumeLabelW(PCWSTR(root.as_ptr()), PCWSTR(lab.as_ptr())).map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[cfg(not(windows))]
#[tauri::command]
fn set_drive_label(_path: String, _label: String) -> Result<(), String> {
    Err("Alleen op Windows".into())
}

/// Verzamelt alle .exe-bestanden in de standaard programmamappen.
#[tauri::command]
fn list_apps() -> Vec<Entry> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Documents"));
    }
    #[cfg(windows)]
    {
        for p in [
            "C:\\Program Files",
            "C:\\Program Files (x86)",
            "C:\\ProgramData",
        ] {
            roots.push(PathBuf::from(p));
        }
    }
    let mut out: Vec<Entry> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let mut wb = WalkBuilder::new(&root);
        wb.hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .max_depth(Some(4))
            .threads(2);
        for de in wb.build().flatten() {
            let p = de.path();
            if p.extension()
                .map(|e| e.eq_ignore_ascii_case("exe"))
                .unwrap_or(false)
            {
                let s = p.to_string_lossy().to_string();
                if seen.insert(s) {
                    if let Some(en) = to_entry(p) {
                        out.push(en);
                    }
                    if out.len() >= 3000 {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Genereert een onvoorspelbare suffix voor tijdelijke bestanden (anti-TOCTOU).
fn unique_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Meng met het adres van een lokale variabele voor extra onvoorspelbaarheid.
    let salt = &nanos as *const _ as usize;
    format!("{:x}{:x}", nanos, salt)
}

/// Controleert of een download-URL veilig is voor de auto-update:
/// alleen https en alleen onze eigen GitHub-release-hosts.
fn is_allowed_update_url(url: &str) -> bool {
    let u = url.trim();
    if !u.starts_with("https://") {
        return false;
    }
    // Host uit de URL halen (tussen "https://" en de eerste '/').
    let rest = &u["https://".len()..];
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split('@').last().unwrap_or(host); // strip eventuele userinfo
    let host = host.split(':').next().unwrap_or(host); // strip poort
    let host = host.to_ascii_lowercase();
    matches!(
        host.as_str(),
        "github.com"
            | "api.github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
    ) || host.ends_with(".githubusercontent.com")
}

/// Opent een nieuwe Outlook-mail met de bestanden als bijlage.
/// Mappen (of meerdere items) worden eerst gezipt.
#[tauri::command]
fn send_mail(paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Err("Niets geselecteerd".into());
    }
    let needs_zip = paths.len() > 1 || paths.iter().any(|p| Path::new(p).is_dir());
    let attach = if needs_zip {
        let dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("macsplorer_mail");
        let _ = fs::create_dir_all(&dir);
        let zpath = dir.join(format!("bijlage_{}_{}.zip", std::process::id(), unique_token()));
        let file = fs::File::create(&zpath).map_err(|e| e.to_string())?;
        let mut zw = zip::ZipWriter::new(file);
        let opt = zip::write::SimpleFileOptions::default();
        for p in &paths {
            let path = Path::new(p);
            let base = path.parent().unwrap_or(Path::new(""));
            add_to_zip(&mut zw, path, base, &opt)?;
        }
        zw.finish().map_err(|e| e.to_string())?;
        zpath.to_string_lossy().to_string()
    } else {
        paths[0].clone()
    };
    #[cfg(windows)]
    {
        // Geen shell (cmd /C) en geen string-interpolatie: het pad wordt als
        // los argument doorgegeven, zodat een bestandsnaam met aanhalingstekens
        // of shell-tekens niet kan uitbreken (command injection).
        std::process::Command::new("outlook.exe")
            .arg("/a")
            .arg(&attach)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let _ = attach;
    }
    Ok(())
}

/// Leest platte tekst van het klembord (voor de speciale plak-hernoemfunctie).
#[tauri::command]
fn clipboard_text() -> Result<String, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.get_text().map_err(|e| e.to_string())
}

/// Sleept bestand(en) precies zoals Windows Verkenner doet, via het echte
/// shell data-object (SHDoDragDrop). Strikte drop-zones (Figma, Weave, ...)
/// accepteren dit wel, in tegenstelling tot een simpele bestand-drop.
#[cfg(windows)]
#[tauri::command]
fn shell_drag(app: tauri::AppHandle, paths: Vec<String>) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{IBindCtx, IDataObject};
    use windows::Win32::System::Ole::{
        IDropSource, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    };
    use windows::Win32::UI::Shell::{
        BHID_DataObject, SHCreateItemFromParsingName, SHDoDragDrop, IShellItem,
    };
    let first = match paths.into_iter().next() {
        Some(p) => p,
        None => return Err("geen pad".into()),
    };
    // De sleep MOET op de hoofdthread draaien (die het venster en de muis-invoer
    // bezit), anders pakt DoDragDrop de lopende muisbeweging niet op. OLE is op
    // die thread al geinitialiseerd door de webview.
    app.run_on_main_thread(move || unsafe {
        let w: Vec<u16> = first.encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(item) =
            SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(w.as_ptr()), None::<&IBindCtx>)
        {
            if let Ok(data) =
                item.BindToHandler::<_, IDataObject>(None::<&IBindCtx>, &BHID_DataObject)
            {
                let _ = SHDoDragDrop(
                    HWND::default(),
                    &data,
                    None::<&IDropSource>,
                    DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
                );
            }
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(windows))]
#[tauri::command]
fn shell_drag(_paths: Vec<String>) -> Result<(), String> {
    Err("alleen op Windows".into())
}

/// ===== Dubbels (identieke bestanden zoeken) =====

#[derive(Serialize, Clone)]
struct DupFile {
    path: String,
    name: String,
    size: u64,
}

/// Alle schijf-roots (C:\, D:\, ...) op Windows; "/" elders.
#[cfg(windows)]
fn drive_roots() -> Vec<String> {
    let mut v = Vec::new();
    for c in b'A'..=b'Z' {
        let root = format!("{}:\\", c as char);
        if Path::new(&root).exists() {
            v.push(root);
        }
    }
    v
}
#[cfg(not(windows))]
fn drive_roots() -> Vec<String> {
    vec!["/".to_string()]
}

/// Mappen die we bij het zoeken naar dubbels overslaan: Windows-systeemmappen en
/// andere kwetsbare/ruis-mappen waar je geen bestanden uit wilt verwijderen.
fn is_protected_dir(path: &std::path::Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        let n = name.to_ascii_lowercase();
        if n.starts_with('$') {
            return true; // $Recycle.Bin, $WinREAgent, ...
        }
        return matches!(
            n.as_str(),
            "windows"
                | "windows.old"
                | "winsxs"
                | "program files"
                | "program files (x86)"
                | "programdata"
                | "appdata"
                | "system volume information"
                | "recovery"
                | "perflogs"
                | "msocache"
                | "boot"
                | "config.msi"
                | "node_modules"
                | ".git"
        );
    }
    false
}

/// Hasht de eerste `n` bytes van een bestand (snelle voorselectie).
fn hash_prefix(path: &std::path::Path, n: usize) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::io::Read;
    let mut f = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let mut total = 0;
    while total < n {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(r) => total += r,
            Err(_) => return None,
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    buf[..total].hash(&mut h);
    Some(h.finish())
}

/// Hasht de volledige inhoud (definitieve vergelijking; bestanden in dezelfde
/// groottegroep lezen in dezelfde blokgroottes, dus identieke inhoud -> identieke hash).
fn hash_full(path: &std::path::Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::io::Read;
    let mut f = fs::File::open(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut buf = [0u8; 65536];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(r) => buf[..r].hash(&mut h),
            Err(_) => return None,
        }
    }
    Some(h.finish())
}

fn find_duplicates_blocking(ignored: Vec<String>) -> Vec<Vec<DupFile>> {
    use ignore::WalkBuilder;
    use std::collections::HashMap;
    let ignored: std::collections::HashSet<String> = ignored.into_iter().collect();

    // Stap 1: alle bestanden verzamelen, gegroepeerd op grootte (lege bestanden over slaan).
    let mut by_size: HashMap<u64, Vec<std::path::PathBuf>> = HashMap::new();
    for root in drive_roots() {
        let mut wb = WalkBuilder::new(&root);
        wb.standard_filters(false)
            .hidden(false)
            .follow_links(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
        wb.filter_entry(|e| {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                !is_protected_dir(e.path())
            } else {
                true
            }
        });
        for result in wb.build() {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let p = entry.path();
            if ignored.contains(&p.to_string_lossy().to_string()) {
                continue;
            }
            let size = match entry.metadata() {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if size == 0 {
                continue;
            }
            by_size.entry(size).or_default().push(p.to_path_buf());
        }
    }

    // Stap 2: per groottegroep eerst op prefix-hash, dan op volledige hash.
    let mut groups: Vec<Vec<DupFile>> = Vec::new();
    for (size, paths) in by_size {
        if paths.len() < 2 {
            continue;
        }
        let mut by_prefix: HashMap<u64, Vec<std::path::PathBuf>> = HashMap::new();
        for p in paths {
            if let Some(h) = hash_prefix(&p, 8192) {
                by_prefix.entry(h).or_default().push(p);
            }
        }
        for (_, pre) in by_prefix {
            if pre.len() < 2 {
                continue;
            }
            let mut by_full: HashMap<u64, Vec<std::path::PathBuf>> = HashMap::new();
            for p in pre {
                if let Some(h) = hash_full(&p) {
                    by_full.entry(h).or_default().push(p);
                }
            }
            for (_, full) in by_full {
                if full.len() < 2 {
                    continue;
                }
                let files: Vec<DupFile> = full
                    .into_iter()
                    .map(|p| DupFile {
                        name: p
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        path: p.to_string_lossy().to_string(),
                        size,
                    })
                    .collect();
                groups.push(files);
            }
        }
    }
    // Grootste verspilling eerst.
    groups.sort_by(|a, b| b[0].size.cmp(&a[0].size));
    groups
}

/// Zoekt op alle schijven naar inhoudelijk identieke bestanden (systeem- en
/// kwetsbare mappen worden automatisch overgeslagen). `ignored` = paden die de
/// gebruiker handmatig op negeren heeft gezet.
#[tauri::command]
async fn find_duplicates(ignored: Vec<String>) -> Result<Vec<Vec<DupFile>>, String> {
    tauri::async_runtime::spawn_blocking(move || find_duplicates_blocking(ignored))
        .await
        .map_err(|e| e.to_string())
}

/// Kopieert een afbeelding naar het klembord (om in canvassen zoals Figma te plakken).
#[tauri::command]
fn copy_image(path: String) -> Result<(), String> {
    let img = image::open(&path).map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(img.into_raw()),
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Werkt de app bij door ALLEEN het .exe-bestand op zijn plek te vervangen
/// (geen herinstallatie). Daardoor blijven de installatiemap, snelkoppeling én
/// de taakbalk-pin intact. Een klein helper-script wacht tot de app sluit,
/// overschrijft het exe en start de app opnieuw.
#[tauri::command]
async fn update_app(url: String) -> Result<(), String> {
    // Alleen https en alleen onze eigen GitHub-release-hosts toestaan. Hierdoor
    // kan een (eventueel via XSS) aangeroepen update niet naar een willekeurige
    // 'evil.exe' wijzen.
    if !is_allowed_update_url(&url) {
        return Err("Geweigerd: alleen https-downloads van GitHub-releases zijn toegestaan.".into());
    }
    let client = reqwest::Client::builder()
        .user_agent("Macsplorer")
        .https_only(true)
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    // Volg geen redirect naar een niet-toegestane host.
    if !is_allowed_update_url(resp.url().as_str()) {
        return Err("Geweigerd: download leidde om naar een niet-vertrouwde host.".into());
    }
    if !resp.status().is_success() {
        return Err(format!("Download faalde ({})", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() < 1024 {
        return Err("Download lijkt ongeldig (te klein).".into());
    }

    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("macsplorer_update");
    let _ = fs::create_dir_all(&dir);
    let token = unique_token();
    // Onvoorspelbare bestandsnaam (anti-TOCTOU).
    let new_exe = dir.join(format!("Macsplorer-new-{token}.exe"));
    fs::write(&new_exe, bytes.as_ref()).map_err(|e| e.to_string())?;

    #[cfg(windows)]
    {
        // Robuuste in-place update zónder extern script:
        // Windows staat toe een DRAAIEND .exe te HERNOEMEN (niet verwijderen).
        // Dus: huidige exe -> ".old", nieuw exe op het originele pad zetten,
        // de nieuwe versie starten en afsluiten. De ".old" ruimen we op bij de
        // volgende start. Pad blijft identiek → taakbalk-pin blijft behouden.
        let cur_exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let old_exe = {
            let mut s = cur_exe.clone().into_os_string();
            s.push(".old");
            std::path::PathBuf::from(s)
        };
        // Eventuele oude rommel opruimen (lukt pas als die niet meer in gebruik is).
        let _ = fs::remove_file(&old_exe);

        // Huidige (draaiende) exe aan de kant zetten.
        fs::rename(&cur_exe, &old_exe).map_err(|e| {
            format!("Kon de huidige app niet vrijmaken voor de update: {e}")
        })?;
        // Nieuwe exe op het originele pad plaatsen.
        if let Err(e) = fs::copy(&new_exe, &cur_exe) {
            // Mislukt → terugdraaien zodat de app bruikbaar blijft.
            let _ = fs::rename(&old_exe, &cur_exe);
            return Err(format!("Kon de nieuwe versie niet plaatsen: {e}"));
        }
        let _ = fs::remove_file(&new_exe);

        // Nieuwe versie starten en daarna afsluiten.
        match std::process::Command::new(&cur_exe).spawn() {
            Ok(_) => {
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    std::process::exit(0);
                });
            }
            Err(e) => {
                return Err(format!(
                    "Update geplaatst, maar herstarten mislukte ({e}). Sluit en open de app handmatig."
                ));
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = new_exe;
    }
    Ok(())
}

/// Verwijdert een achtergebleven '<app>.exe.old' van een vorige in-place update.
/// Wordt bij het opstarten aangeroepen; lukt nu wel omdat het oude bestand niet
/// meer in gebruik is.
#[cfg(windows)]
fn cleanup_old_update() {
    if let Ok(cur) = std::env::current_exe() {
        let mut s = cur.into_os_string();
        s.push(".old");
        let _ = fs::remove_file(std::path::PathBuf::from(s));
    }
}

fn main() {
    #[cfg(windows)]
    cleanup_old_update();
    tauri::Builder::default()
        .plugin(tauri_plugin_drag::init())
        .invoke_handler(tauri::generate_handler![
            read_dir,
            list_locations,
            search,
            parse_aliases,
            pick_folder,
            pick_image,
            pick_excel,
            open_path,
            rename,
            image_sizes,
            thumbnail,
            copy_paths,
            move_paths,
            create_folder,
            create_file,
            delete_paths,
            zip_paths,
            mirror_image,
            drag_icon,
            dir_size,
            stat_paths,
            folder_preview,
            app_icon,
            localsend,
            localsend_discover,
            localsend_send,
            shell_properties,
            set_drive_label,
            list_apps,
            send_mail,
            find_duplicates,
            copy_image,
            clipboard_text,
            shell_drag,
            update_app
        ])
        .run(tauri::generate_context!())
        .expect("Fout bij het starten van Macsplorer");
}
