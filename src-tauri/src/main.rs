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
                let (total, free) = disk_space(&root);
                let lbl = volume_label(&root);
                let name = if lbl.is_empty() {
                    format!("Schijf ({}:)", c as char)
                } else {
                    format!("{} ({}:)", lbl, c as char)
                };
                v.push(Location {
                    name,
                    path: root,
                    kind: "drive".into(),
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

/// Bestand of map hernoemen. Geeft het nieuwe pad terug.
#[tauri::command]
fn rename(path: String, new_name: String) -> Result<String, String> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') {
        return Err("Ongeldige naam".into());
    }
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
    if nm.is_empty() || nm.contains('/') || nm.contains('\\') {
        return Err("Ongeldige naam".into());
    }
    let mut dest = Path::new(&parent).join(nm);
    let mut i = 2;
    while dest.exists() {
        dest = Path::new(&parent).join(format!("{nm} ({i})"));
        i += 1;
    }
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
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

/// Experimenteel: probeert LocalSend te starten met de geselecteerde bestanden.
#[tauri::command]
fn localsend(paths: Vec<String>) -> Result<(), String> {
    let mut cmd = std::process::Command::new("localsend");
    for p in &paths {
        cmd.arg(p);
    }
    cmd.spawn().map_err(|e| e.to_string())?;
    Ok(())
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

fn ls_discover_blocking() -> Vec<LsDevice> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::time::{Duration, Instant};
    let mut out: Vec<LsDevice> = Vec::new();
    let sock = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let _ = sock.set_reuse_address(true);
    let bind: SocketAddr = "0.0.0.0:53317".parse().unwrap();
    if sock.bind(&bind.into()).is_err() {
        return out;
    }
    let _ = sock.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 167), &Ipv4Addr::UNSPECIFIED);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(600)));
    let udp: UdpSocket = sock.into();
    let announce = format!(
        "{{\"alias\":\"Macsplorer\",\"version\":\"2.0\",\"deviceModel\":\"PC\",\"deviceType\":\"desktop\",\"fingerprint\":\"macsplorer-{}\",\"port\":53317,\"protocol\":\"http\",\"download\":false,\"announce\":true}}",
        std::process::id()
    );
    let _ = udp.send_to(announce.as_bytes(), "224.0.0.167:53317");
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let mut seen = std::collections::HashSet::new();
    while start.elapsed() < Duration::from_millis(3000) {
        if let Ok((n, src)) = udp.recv_from(&mut buf) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                let alias = v.get("alias").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if alias.is_empty() || alias == "Macsplorer" {
                    continue;
                }
                let fp = v.get("fingerprint").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !fp.is_empty() && !seen.insert(fp.clone()) {
                    continue;
                }
                out.push(LsDevice {
                    alias,
                    ip: src.ip().to_string(),
                    port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(53317) as u16,
                    protocol: v.get("protocol").and_then(|x| x.as_str()).unwrap_or("http").to_string(),
                    fingerprint: fp,
                    device_type: v.get("deviceType").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                });
            }
        }
    }
    out
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
        let zpath = dir.join(format!("bijlage_{}.zip", std::process::id()));
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
        use std::os::windows::process::CommandExt;
        let line = format!("start \"\" outlook.exe /a \"{}\"", attach);
        std::process::Command::new("cmd")
            .arg("/C")
            .raw_arg(&line)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let _ = attach;
    }
    Ok(())
}

/// Sleept bestand(en) precies zoals Windows Verkenner doet, via het echte
/// shell data-object (SHDoDragDrop). Strikte drop-zones (Figma, Weave, ...)
/// accepteren dit wel, in tegenstelling tot een simpele bestand-drop.
#[cfg(windows)]
#[tauri::command]
fn shell_drag(paths: Vec<String>) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{IBindCtx, IDataObject};
    use windows::Win32::System::Ole::{
        IDropSource, OleInitialize, OleUninitialize, DROPEFFECT_COPY, DROPEFFECT_LINK,
        DROPEFFECT_MOVE,
    };
    use windows::Win32::UI::Shell::{BHID_DataObject, SHCreateItemFromParsingName, IShellItem};
    let first = match paths.into_iter().next() {
        Some(p) => p,
        None => return Err("geen pad".into()),
    };
    std::thread::spawn(move || unsafe {
        let _ = OleInitialize(None);
        let w: Vec<u16> = first.encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
            PCWSTR(w.as_ptr()),
            None::<&IBindCtx>,
        ) {
            if let Ok(data) = item.BindToHandler::<IDataObject>(None, &BHID_DataObject) {
                let _ = windows::Win32::UI::Shell::SHDoDragDrop(
                    HWND::default(),
                    &data,
                    None::<&IDropSource>,
                    DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
                );
            }
        }
        OleUninitialize();
    });
    Ok(())
}

#[cfg(not(windows))]
#[tauri::command]
fn shell_drag(_paths: Vec<String>) -> Result<(), String> {
    Err("alleen op Windows".into())
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

/// Downloadt de nieuwe installer en start die (werkt in-place bij), sluit daarna de app.
#[tauri::command]
async fn update_app(url: String) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("Macsplorer")
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("Download faalde ({})", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("macsplorer_update");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("Macsplorer-setup.exe");
    fs::write(&path, bytes.as_ref()).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        std::process::Command::new(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(900));
            std::process::exit(0);
        });
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}

fn main() {
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
            copy_image,
            shell_drag,
            update_app
        ])
        .run(tauri::generate_context!())
        .expect("Fout bij het starten van Macsplorer");
}
