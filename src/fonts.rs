//! The fonts a Look may use: the ones the engine ships and the creator's own.
//!
//! Bundled fonts are compiled in (`resources/fonts/`, licences in
//! `resources/fonts/licenses/`, the table in `resources/fonts/README.md`) and
//! written to the fonts folder for libass by `provision::ensure_fonts`.
//!
//! A creator's fonts are `.ttf` / `.otf` files in the same folder, which is
//! `<data dir>/fonts` (the data dir the engine runs with, `--data-dir`, so two
//! data dirs never share fonts). libass is given that one folder, so a render
//! sees both sets. A font is listed by its *family*: name id 1 of its `name`
//! table, the very name libass matches a style's font name against, and the
//! name a Look uses in `captions.font` / `headline.font` (matched without
//! regard to case).
//!
//! Fonts are read once into a registry that the Look parser and the caption
//! metrics consult. Adding or removing a font republishes it; a name the
//! registry does not know triggers one re-scan of the active folder (at most
//! every few seconds), so a file dropped into the folder by hand is picked up
//! too. Reading a font never panics, whatever the file holds
//! ([`crate::captions::metrics::Face`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::captions::metrics::Face;

/// Largest font file the creator can add.
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// How a bundled font is grouped in a picker. A creator's font is `custom`.
pub const CATEGORIES: [&str; 8] = [
    "display", "sans", "rounded", "comic", "serif", "hand", "mono", "custom",
];

/// A font the engine ships.
pub struct Bundled {
    /// The name a Look uses (name id 1 of the file).
    pub family: &'static str,
    pub file: &'static str,
    /// `usWeightClass` of the file.
    pub weight: u16,
    pub category: &'static str,
    pub licence: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! bundled {
    ($family:expr, $file:expr, $weight:expr, $cat:expr, $lic:expr) => {
        Bundled {
            family: $family,
            file: $file,
            weight: $weight,
            category: $cat,
            licence: $lic,
            bytes: include_bytes!(concat!("../resources/fonts/", $file)),
        }
    };
}

/// Every bundled font, in the order a picker lists them.
pub const BUNDLED: &[Bundled] = &[
    bundled!("Anton", "Anton-Regular.ttf", 400, "display", "OFL-1.1"),
    bundled!(
        "Bebas Neue",
        "BebasNeue-Regular.ttf",
        400,
        "display",
        "OFL-1.1"
    ),
    bundled!("Oswald", "Oswald-Bold.ttf", 700, "display", "OFL-1.1"),
    bundled!(
        "Archivo Black",
        "ArchivoBlack-Regular.ttf",
        400,
        "display",
        "OFL-1.1"
    ),
    bundled!(
        "Lilita One",
        "LilitaOne-Regular.ttf",
        400,
        "rounded",
        "OFL-1.1"
    ),
    bundled!("Bangers", "Bangers-Regular.ttf", 400, "comic", "OFL-1.1"),
    bundled!(
        "Luckiest Guy",
        "LuckiestGuy-Regular.ttf",
        400,
        "comic",
        "Apache-2.0"
    ),
    bundled!("Inter Medium", "Inter-Medium.ttf", 500, "sans", "OFL-1.1"),
    bundled!(
        "Montserrat ExtraBold",
        "Montserrat-ExtraBold.ttf",
        800,
        "sans",
        "OFL-1.1"
    ),
    bundled!("Poppins", "Poppins-Bold.ttf", 700, "sans", "OFL-1.1"),
    bundled!(
        "Space Grotesk",
        "SpaceGrotesk-Bold.ttf",
        700,
        "sans",
        "OFL-1.1"
    ),
    bundled!(
        "DM Serif Display",
        "DMSerifDisplay-Regular.ttf",
        400,
        "serif",
        "OFL-1.1"
    ),
    bundled!(
        "Permanent Marker",
        "PermanentMarker-Regular.ttf",
        400,
        "hand",
        "Apache-2.0"
    ),
    bundled!(
        "JetBrains Mono",
        "JetBrainsMono-Variable.ttf",
        400,
        "mono",
        "OFL-1.1"
    ),
    bundled!("Space Mono", "SpaceMono-Bold.ttf", 700, "mono", "OFL-1.1"),
];

/// The names a Look may use for a bundled font.
pub fn bundled_families() -> impl Iterator<Item = &'static str> {
    BUNDLED.iter().map(|f| f.family)
}

/// One usable family, as listed to the app.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FontEntry {
    /// The name a Look uses.
    pub family: String,
    /// File name inside the fonts folder (and in the `/font/<file>` route).
    pub file: String,
    pub bundled: bool,
    /// One of [`CATEGORIES`]: the picker's group.
    pub category: String,
    /// `usWeightClass` of the file.
    pub weight: u16,
    pub bytes: u64,
    /// SPDX id of a bundled font's licence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    /// Changes when the file does: add it to the font URL as a cache buster.
    pub rev: String,
    /// Route to fetch the file from, without the token: `/font/<file>`.
    pub url: String,
}

/// A creator's font that was read successfully.
struct Loaded {
    family: &'static str,
    file: String,
    weight: u16,
    bytes: u64,
    rev: String,
    face: Arc<Face>,
}

impl Loaded {
    fn entry(&self) -> FontEntry {
        FontEntry {
            family: self.family.to_string(),
            file: self.file.clone(),
            bundled: false,
            category: "custom".into(),
            weight: self.weight,
            bytes: self.bytes,
            licence: None,
            rev: self.rev.clone(),
            url: format!("/font/{}", self.file),
        }
    }
}

fn bundled_entry(f: &Bundled) -> FontEntry {
    FontEntry {
        family: f.family.into(),
        file: f.file.into(),
        bundled: true,
        category: f.category.into(),
        weight: f.weight,
        bytes: f.bytes.len() as u64,
        licence: Some(f.licence.into()),
        rev: "bundled".into(),
        url: format!("/font/{}", f.file),
    }
}

/// Family names live as long as the process (a `&'static str` is what the
/// Look and the ASS writers carry); one copy per distinct name.
fn intern(s: &str) -> &'static str {
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut g = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(HashSet::new);
    if let Some(&n) = set.get(s) {
        return n;
    }
    let n: &'static str = Box::leak(s.to_string().into_boxed_str());
    set.insert(n);
    n
}

fn is_font_ext(name: &str) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "ttf" => Some("ttf"),
        "otf" => Some("otf"),
        _ => None,
    }
}

fn is_bundled_file(name: &str) -> bool {
    BUNDLED.iter().any(|f| f.file.eq_ignore_ascii_case(name))
}

/// A family name an ASS style row can carry: no comma, braces, backslash or
/// control characters, 1 to 64 characters.
fn check_family(family: &str) -> Result<(), String> {
    let n = family.chars().count();
    if n == 0 || n > 64 {
        return Err("the font's family name must be 1 to 64 characters long".into());
    }
    if family
        .chars()
        .any(|c| c.is_control() || ",{}\\;".contains(c) || c == '\u{fffd}')
    {
        return Err(format!(
            "the font's family name “{family}” has characters a Look cannot use (comma, braces, backslash, semicolon or unreadable text)"
        ));
    }
    Ok(())
}

/// A file name that is safe to store: the last path part only, letters,
/// digits, `.`, `-` and `_`, lower-case extension, never a Windows device
/// name, never longer than 100 characters.
pub fn sanitize_file_name(raw: &str, ext: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = Path::new(last)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut s: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else if c == ' ' {
                '-'
            } else {
                '_'
            }
        })
        .collect();
    s = s.trim_matches(|c| c == '_' || c == '-').to_string();
    s.truncate(90);
    if s.is_empty() {
        s = "font".into();
    }
    let upper = s.to_ascii_uppercase();
    let device = ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit());
    if device {
        s.insert(0, '_');
    }
    format!("{s}.{ext}")
}

// ---------------------------------------------------------------------------
// The registry: the creator's fonts the Look parser and the metrics see
// ---------------------------------------------------------------------------

type Registry = HashMap<PathBuf, Vec<Arc<Loaded>>>;
static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);

/// Parsed files by (folder, name): reused while size and mtime stand.
type CacheMap = HashMap<(PathBuf, String), (u64, Option<SystemTime>, Option<Arc<Loaded>>)>;
static CACHE: Mutex<Option<CacheMap>> = Mutex::new(None);

/// Serialises add / remove (one name check, one write).
static WRITE: Mutex<()> = Mutex::new(());

/// Read every font file of `dir` that is not a bundled one.
fn scan(dir: &Path) -> Vec<Arc<Loaded>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_font_ext(n).is_some() && !is_bundled_file(n))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    let mut out: Vec<Arc<Loaded>> = Vec::new();
    let mut seen: HashSet<String> = bundled_families().map(|f| f.to_lowercase()).collect();
    for name in names {
        let path = dir.join(&name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let (len, mtime) = (meta.len(), meta.modified().ok());
        let key = (dir.to_path_buf(), name.clone());
        let cached = CACHE
            .lock()
            .ok()
            .and_then(|c| c.as_ref()?.get(&key).cloned())
            .filter(|(l, m, _)| *l == len && *m == mtime);
        let loaded = match cached {
            Some((_, _, l)) => l,
            None => {
                let l = load(&path, &name, len, mtime);
                if let Ok(mut c) = CACHE.lock() {
                    c.get_or_insert_with(HashMap::new)
                        .insert(key, (len, mtime, l.clone()));
                }
                l
            }
        };
        if let Some(l) = loaded {
            if seen.insert(l.family.to_lowercase()) {
                out.push(l);
            }
        }
    }
    out
}

fn load(path: &Path, name: &str, len: u64, mtime: Option<SystemTime>) -> Option<Arc<Loaded>> {
    if len == 0 || len > MAX_BYTES {
        return None;
    }
    let face = Face::from_bytes(std::fs::read(path).ok()?).ok()?;
    check_family(face.family()).ok()?;
    let secs = mtime
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some(Arc::new(Loaded {
        family: intern(face.family()),
        file: name.to_string(),
        weight: face.weight(),
        bytes: len,
        rev: format!("{len:x}-{secs:x}"),
        face: Arc::new(face),
    }))
}

fn publish(dir: &Path, fonts: Vec<Arc<Loaded>>) {
    if let Ok(mut r) = REGISTRY.write() {
        r.get_or_insert_with(HashMap::new)
            .insert(dir.to_path_buf(), fonts);
    }
}

fn find_user(family: &str) -> Option<Arc<Loaded>> {
    let want = family.trim().to_lowercase();
    REGISTRY
        .read()
        .ok()?
        .as_ref()?
        .values()
        .flatten()
        .find(|l| l.family.to_lowercase() == want)
        .cloned()
}

/// A creator's font by family, with one throttled re-scan of the active
/// folder when it is not known yet (a file dropped in by hand, a fresh
/// process). Tests never read the real folder.
fn lookup_user(family: &str) -> Option<Arc<Loaded>> {
    if let Some(l) = find_user(family) {
        return Some(l);
    }
    if cfg!(test) {
        return None;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().ok()?;
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            return None;
        }
        *last = Some(Instant::now());
    }
    Library::active().publish();
    find_user(family)
}

/// The name a Look uses for `name`, if it is a font we have: bundled, or one
/// the creator added. Case does not matter.
pub fn resolve(name: &str) -> Option<&'static str> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    BUNDLED
        .iter()
        .find(|f| f.family.eq_ignore_ascii_case(n))
        .map(|f| f.family)
        .or_else(|| lookup_user(n).map(|l| l.family))
}

/// The face of a creator's font (the bundled ones are `metrics::face`'s).
pub fn user_face(family: &str) -> Option<Arc<Face>> {
    lookup_user(family).map(|l| l.face.clone())
}

// ---------------------------------------------------------------------------
// The library: one fonts folder
// ---------------------------------------------------------------------------

/// Where a font file lives, for the HTTP route.
pub enum FontFile {
    Bundled(&'static [u8]),
    User(PathBuf),
}

/// A fonts folder: the creator's files, beside the bundled ones libass reads.
#[derive(Debug, Clone)]
pub struct Library {
    dir: PathBuf,
}

impl Library {
    pub fn new(dir: impl Into<PathBuf>) -> Library {
        Library { dir: dir.into() }
    }

    /// The folder of the data dir the engine runs with.
    pub fn active() -> Library {
        Library::new(crate::provision::fonts_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read the folder and make its fonts known to Looks and layout.
    pub fn publish(&self) -> Vec<FontEntry> {
        let found = scan(&self.dir);
        let entries = found.iter().map(|l| l.entry()).collect();
        publish(&self.dir, found);
        entries
    }

    /// Every usable family: the bundled ones first, then the creator's.
    pub fn list(&self) -> Vec<FontEntry> {
        let mut out: Vec<FontEntry> = BUNDLED.iter().map(bundled_entry).collect();
        out.extend(self.publish());
        out
    }

    /// Add a font from a local file: validated, copied in under a safe name,
    /// and answered with its entry. Refusals are plain sentences.
    pub fn add(&self, src: &Path) -> Result<FontEntry, String> {
        let shown = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("that file")
            .to_string();
        let refuse = |why: String| Err(format!("cannot add “{shown}”: {why}"));
        let meta = match std::fs::metadata(src) {
            Ok(m) if m.is_file() => m,
            Ok(_) => return refuse("it is not a file".into()),
            Err(e) => return refuse(format!("it cannot be read ({e})")),
        };
        let Some(ext) = is_font_ext(&shown) else {
            return refuse(if shown.to_ascii_lowercase().ends_with(".ttc") {
                "font collections (.ttc) are not supported: add a single .ttf or .otf file".into()
            } else {
                "only .ttf and .otf font files can be added".into()
            });
        };
        if meta.len() == 0 {
            return refuse("the file is empty".into());
        }
        if meta.len() > MAX_BYTES {
            return refuse(format!(
                "it is {:.1} MB and the limit is {} MB",
                meta.len() as f64 / 1_048_576.0,
                MAX_BYTES / 1_048_576
            ));
        }
        // Read at most one byte past the limit: the size may have changed.
        let mut bytes = Vec::new();
        {
            use std::io::Read;
            let f = std::fs::File::open(src)
                .map_err(|e| format!("cannot add “{shown}”: it cannot be read ({e})"))?;
            f.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot add “{shown}”: it cannot be read ({e})"))?;
        }
        if bytes.len() as u64 > MAX_BYTES {
            return refuse(format!("it is larger than {} MB", MAX_BYTES / 1_048_576));
        }
        let face = match Face::from_bytes(bytes.clone()) {
            Ok(f) => f,
            Err(e) => return refuse(e),
        };
        if let Err(e) = check_family(face.family()) {
            return refuse(e);
        }
        let family = face.family().to_string();
        let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
        if bundled_families().any(|f| f.eq_ignore_ascii_case(&family)) {
            return refuse(format!(
                "the font family “{family}” is already installed with DigiClip"
            ));
        }
        let have = scan(&self.dir);
        if let Some(l) = have
            .iter()
            .find(|l| l.family.to_lowercase() == family.to_lowercase())
        {
            return refuse(format!(
                "the font family “{family}” is already added ({})",
                l.file
            ));
        }
        // A stored name that is safe, not a bundled file's, and not taken.
        let mut file = sanitize_file_name(&shown, ext);
        if is_bundled_file(&file) {
            return refuse(format!(
                "the file name “{file}” belongs to a font installed with DigiClip: rename the file"
            ));
        }
        let taken = |f: &str| self.dir.join(f).exists();
        if taken(&file) {
            let stem = file.trim_end_matches(&format!(".{ext}")).to_string();
            file = (2..1000)
                .map(|n| format!("{stem}-{n}.{ext}"))
                .find(|f| !taken(f))
                .ok_or_else(|| format!("cannot add “{shown}”: too many fonts with that name"))?;
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot add “{shown}”: the fonts folder cannot be made ({e})"))?;
        let part = self.dir.join(format!("{file}.part"));
        let dest = self.dir.join(&file);
        if let Err(e) = std::fs::write(&part, &bytes).and_then(|_| std::fs::rename(&part, &dest)) {
            let _ = std::fs::remove_file(&part);
            return refuse(format!("it could not be stored ({e})"));
        }
        let entries = self.publish();
        entries.into_iter().find(|e| e.file == file).ok_or_else(|| {
            format!("cannot add “{shown}”: it was stored but could not be read back")
        })
    }

    /// Remove a font the creator added, by family or file name. Bundled fonts
    /// stay.
    pub fn remove(&self, name: &str) -> Result<FontEntry, String> {
        let n = name.trim();
        if let Some(b) = BUNDLED
            .iter()
            .find(|f| f.family.eq_ignore_ascii_case(n) || f.file.eq_ignore_ascii_case(n))
        {
            return Err(format!(
                "“{}” is installed with DigiClip and cannot be removed",
                b.family
            ));
        }
        let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
        let have = scan(&self.dir);
        let want = n.to_lowercase();
        let Some(found) = have
            .iter()
            .find(|l| l.family.to_lowercase() == want || l.file.to_lowercase() == want)
        else {
            return Err(format!("no added font named “{n}”"));
        };
        let entry = found.entry();
        std::fs::remove_file(self.dir.join(&found.file)).map_err(|e| {
            format!(
                "cannot remove “{}”: {e} (a render may be using it; try again in a moment)",
                found.family
            )
        })?;
        if let Ok(mut c) = CACHE.lock() {
            if let Some(c) = c.as_mut() {
                c.remove(&(self.dir.clone(), found.file.clone()));
            }
        }
        self.publish();
        Ok(entry)
    }

    /// The file behind `/font/<file>`: only a name the listing holds is
    /// served, so the request text never becomes a path.
    pub fn locate(&self, file: &str) -> Option<(FontFile, &'static str, String)> {
        if file.is_empty()
            || file.len() > 200
            || file.starts_with('.')
            || file.contains("..")
            || file.contains(['/', '\\', ':', '\0'])
            || file.chars().any(char::is_control)
        {
            return None;
        }
        let ctype = match is_font_ext(file)? {
            "otf" => "font/otf",
            _ => "font/ttf",
        };
        if let Some(b) = BUNDLED.iter().find(|f| f.file.eq_ignore_ascii_case(file)) {
            return Some((FontFile::Bundled(b.bytes), ctype, "bundled".into()));
        }
        let l = self
            .publish_listing()
            .into_iter()
            .find(|l| l.file.eq_ignore_ascii_case(file))?;
        Some((FontFile::User(self.dir.join(&l.file)), ctype, l.rev.clone()))
    }

    fn publish_listing(&self) -> Vec<Arc<Loaded>> {
        scan(&self.dir)
    }
}

/// A fixed-width table of fonts for a terminal.
pub fn table(fonts: &[FontEntry]) -> String {
    let w = fonts
        .iter()
        .map(|f| f.family.chars().count())
        .max()
        .unwrap_or(6)
        .max(6);
    let wf = fonts
        .iter()
        .map(|f| f.file.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = format!(
        "{:<w$}  {:<wf$}  {:>6}  {:<8}  {}\n",
        "FAMILY", "FILE", "WEIGHT", "CATEGORY", "SOURCE"
    );
    for f in fonts {
        out.push_str(&format!(
            "{:<w$}  {:<wf$}  {:>6}  {:<8}  {}\n",
            f.family,
            f.file,
            f.weight,
            f.category,
            if f.bundled { "bundled" } else { "added" }
        ));
    }
    out
}

/// `digiclip fonts [list|add <file>|remove <font>] [--data-dir <dir>] [--json]`.
pub fn run_cli(args: crate::cli::FontsArgs) -> anyhow::Result<()> {
    use crate::cli::FontsAction;
    let dir = args
        .data_dir
        .unwrap_or_else(crate::provision::root)
        .join("fonts");
    let lib = Library::new(&dir);
    let (fonts, note) = match args.action.unwrap_or(FontsAction::List) {
        FontsAction::List => (lib.list(), None),
        FontsAction::Add { file } => {
            let e = lib.add(&file).map_err(anyhow::Error::msg)?;
            let note = format!("added “{}” ({})", e.family, e.file);
            (lib.list(), Some(note))
        }
        FontsAction::Remove { font } => {
            let e = lib.remove(&font).map_err(anyhow::Error::msg)?;
            let note = format!("removed “{}” ({})", e.family, e.file);
            (lib.list(), Some(note))
        }
    };
    if args.json {
        let v = serde_json::json!({
            "fonts": fonts,
            "dir": dir.display().to_string(),
            "max_bytes": MAX_BYTES,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        if let Some(n) = note {
            println!("{n}");
        }
        print!("{}", table(&fonts));
        println!("added fonts live in {}", dir.display());
    }
    Ok(())
}

/// `captions.font` / `headline.font` of a Look value that name no font we
/// have, once each, as the one-line warning the engine reports.
pub fn missing_font_notes<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let n = n.trim();
        if n.is_empty() || resolve(n).is_some() {
            continue;
        }
        let line = format!("font “{n}” is not installed; used the default");
        if !out.contains(&line) {
            out.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::captions::metrics::testfont;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("digiclip-fonts-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn the_bundled_table_matches_the_files() {
        assert!(BUNDLED.len() >= 14);
        let mut seen = HashSet::new();
        for f in BUNDLED {
            assert!(seen.insert(f.family.to_lowercase()), "{}", f.family);
            assert!(CATEGORIES.contains(&f.category), "{}", f.family);
            assert!(
                ["OFL-1.1", "Apache-2.0"].contains(&f.licence),
                "{}",
                f.family
            );
            let face = Face::parse(f.bytes).unwrap_or_else(|| panic!("{} unreadable", f.file));
            assert_eq!(face.family(), f.family, "{}", f.file);
            assert_eq!(face.weight(), f.weight, "{}", f.file);
            assert!(check_family(f.family).is_ok());
            assert_eq!(resolve(&f.family.to_uppercase()), Some(f.family));
        }
        assert_eq!(resolve("  anton "), Some("Anton"));
        assert_eq!(resolve("Comic Sans"), None);
        assert_eq!(resolve(""), None);
    }

    #[test]
    fn a_licence_text_sits_beside_every_bundled_font() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/fonts");
        let licences: Vec<String> = std::fs::read_dir(dir.join("licenses"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        for f in BUNDLED {
            let stem = f.file.split('-').next().unwrap();
            assert!(
                licences.iter().any(|l| l.starts_with(stem)),
                "no licence for {}",
                f.file
            );
        }
        let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
        for f in BUNDLED {
            assert!(
                readme.contains(f.file) && readme.contains(f.family),
                "{}",
                f.file
            );
        }
    }

    #[test]
    fn add_list_and_remove_against_a_data_dir() {
        let dir = tmp("crud");
        let lib = Library::new(dir.join("fonts"));
        let src = write(
            &dir,
            "My Cool Font!.TTF",
            &testfont::build("Crud Marker", false),
        );
        // Nothing added yet: the bundled set only, bundled first.
        let first = lib.list();
        assert_eq!(first.len(), BUNDLED.len());
        assert!(first.iter().all(|e| e.bundled));
        let e = lib.add(&src).unwrap();
        assert_eq!(e.family, "Crud Marker");
        assert_eq!(e.file, "My-Cool-Font.ttf");
        assert!(!e.bundled && e.category == "custom" && e.url == "/font/My-Cool-Font.ttf");
        assert!(lib.dir().join(&e.file).is_file());
        let all = lib.list();
        assert_eq!(all.len(), BUNDLED.len() + 1);
        assert!(all[..BUNDLED.len()].iter().all(|e| e.bundled));
        assert_eq!(all.last().unwrap().family, "Crud Marker");
        // A Look can use it, in any case, and the metrics read it.
        assert_eq!(resolve("crud MARKER"), Some("Crud Marker"));
        assert!(crate::captions::metrics::face("Crud Marker").is_some());
        // Another data dir does not see it.
        let other = Library::new(dir.join("other").join("fonts"));
        assert_eq!(other.list().len(), BUNDLED.len());
        // Remove by family; the file goes, the family is unknown again.
        let gone = lib.remove("crud marker").unwrap();
        assert_eq!(gone.file, e.file);
        assert!(!lib.dir().join(&e.file).exists());
        assert_eq!(lib.list().len(), BUNDLED.len());
        assert_eq!(resolve("Crud Marker"), None);
        // Remove by file name works too.
        let again = lib.add(&src).unwrap();
        assert_eq!(lib.remove(&again.file).unwrap().family, "Crud Marker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_refusal_is_a_clear_sentence() {
        let dir = tmp("refuse");
        let lib = Library::new(dir.join("fonts"));
        let good = testfont::build("Refuse Marker", false);
        let src = write(&dir, "good.ttf", &good);
        lib.add(&src).unwrap();
        let err = |p: &Path| lib.add(p).unwrap_err();
        // A bundled family, in any case.
        let e = err(&write(&dir, "dup.ttf", &testfont::build("anton", false)));
        assert!(
            e.contains("“anton”") && e.contains("already installed"),
            "{e}"
        );
        // An added family.
        let e = err(&write(&dir, "dup2.ttf", &good));
        assert!(e.contains("already added") && e.contains("good.ttf"), "{e}");
        // Collections and other containers.
        let e = err(&write(&dir, "pack.ttc", b"ttcf\0\x01\0\0\0\0\0\x01"));
        assert!(e.contains("collections (.ttc)"), "{e}");
        let e = err(&write(&dir, "disguised.ttf", b"ttcf\0\x01\0\0\0\0\0\x01"));
        assert!(e.contains("collections (.ttc)"), "{e}");
        let e = err(&write(&dir, "web.woff", b"wOFF"));
        assert!(e.contains(".ttf and .otf"), "{e}");
        let e = err(&write(&dir, "web2.ttf", b"wOF2\0\0\0\0\0\0\0\0"));
        assert!(e.contains("web fonts"), "{e}");
        // Not a font at all; empty; cut short; a directory; a missing path.
        assert!(err(&write(&dir, "text.ttf", b"hello, this is text")).contains("not a TrueType"));
        assert!(err(&write(&dir, "empty.ttf", b"")).contains("empty"));
        let cut = write(&dir, "cut.ttf", &good[..good.len() / 2]);
        assert!(err(&cut).contains("truncated"), "{}", err(&cut));
        let garbage: Vec<u8> = (0..5000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert!(!err(&write(&dir, "noise.ttf", &garbage)).is_empty());
        assert!(err(&dir).contains("not a file"));
        assert!(err(&dir.join("missing.ttf")).contains("cannot be read"));
        // Too big: the limit is on the file, checked before it is read.
        let big = dir.join("big.ttf");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        let e = err(&big);
        assert!(e.contains("limit is 20 MB"), "{e}");
        // A family a Look cannot carry in a style row.
        for bad in ["Comma, Font", "Brace {Font}", "Back\\slash"] {
            let e = err(&write(&dir, "bad.ttf", &testfont::build(bad, false)));
            assert!(e.contains("characters a Look cannot use"), "{bad}: {e}");
        }
        // An OpenType/CFF font is welcome.
        let otf = write(&dir, "cff.otf", &testfont::build("Refuse CFF", true));
        assert!(!lib.add(&otf).unwrap().bundled);
        // Bundled fonts cannot be removed, unknown names are named.
        let e = lib.remove("Anton").unwrap_err();
        assert!(e.contains("cannot be removed"), "{e}");
        assert!(lib
            .remove("Inter-Medium.ttf")
            .unwrap_err()
            .contains("cannot be removed"));
        assert!(lib
            .remove("Nope")
            .unwrap_err()
            .contains("no added font named “Nope”"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_names_are_sanitised() {
        for (raw, want) in [
            ("../../etc/passwd.ttf", "passwd.ttf"),
            ("C:\\Windows\\Fonts\\Evil Font.TTF", "Evil-Font.ttf"),
            ("..ttf", "font.ttf"),
            ("con.ttf", "_con.ttf"),
            ("COM3.otf", "_COM3.otf"),
            ("a b&c%d.ttf", "a-b_c_d.ttf"),
            ("日本語.ttf", "font.ttf"),
            ("-_x_-.ttf", "x.ttf"),
        ] {
            let ext = if raw.to_lowercase().ends_with("otf") {
                "otf"
            } else {
                "ttf"
            };
            let got = sanitize_file_name(raw, ext);
            assert_eq!(got, want, "{raw}");
            assert!(!got.contains(['/', '\\', ':']) && !got.starts_with('.'));
        }
        assert!(sanitize_file_name(&"x".repeat(500), "ttf").len() <= 100);
    }

    #[test]
    fn a_file_name_of_a_bundled_font_is_refused_and_a_taken_one_is_numbered() {
        let dir = tmp("names");
        let lib = Library::new(dir.join("fonts"));
        let e = lib
            .add(&write(
                &dir,
                "anton-regular.TTF",
                &testfont::build("Names One", false),
            ))
            .unwrap_err();
        assert!(
            e.contains("belongs to a font installed with DigiClip"),
            "{e}"
        );
        let a = lib
            .add(&write(&dir, "same.ttf", &testfont::build("Names A", false)))
            .unwrap();
        let b = lib
            .add(&write(&dir, "same.ttf", &testfont::build("Names B", false)))
            .unwrap();
        assert_eq!(
            (a.file.as_str(), b.file.as_str()),
            ("same.ttf", "same-2.ttf")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_route_serves_listed_files_and_nothing_else() {
        let dir = tmp("route");
        let lib = Library::new(dir.join("fonts"));
        let e = lib
            .add(&write(
                &dir,
                "mine.otf",
                &testfont::build("Route Mine", true),
            ))
            .unwrap();
        // A secret beside the fonts and one in the parent: never reachable.
        write(lib.dir(), "secret.txt", b"nope");
        write(&dir, "settings.json", b"{}");
        let ok = |n: &str| lib.locate(n).is_some();
        assert!(ok("Anton-Regular.ttf") && ok("anton-regular.ttf"));
        assert!(ok(&e.file));
        let (file, ctype, _) = lib.locate(&e.file).unwrap();
        assert_eq!(ctype, "font/otf");
        assert!(matches!(file, FontFile::User(p) if p == lib.dir().join(&e.file)));
        assert_eq!(lib.locate("Anton-Regular.ttf").unwrap().1, "font/ttf");
        for bad in [
            "secret.txt",
            "../settings.json",
            "..\\settings.json",
            "../fonts/mine.otf",
            "..%2fsettings.json",
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "C:win.ini",
            "mine.otf/..",
            "mine.otf\0.txt",
            "mine.otf:stream",
            ".hidden.ttf",
            "nothere.ttf",
            "Anton-Regular.ttf.part",
            "",
        ] {
            assert!(lib.locate(bad).is_none(), "{bad:?}");
        }
        // A listing name with a font extension that is a directory is not served.
        std::fs::create_dir_all(lib.dir().join("dir.ttf")).unwrap();
        assert!(lib.locate("dir.ttf").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_files_in_the_folder_are_skipped_not_fatal() {
        let dir = tmp("broken");
        let lib = Library::new(dir.join("fonts"));
        std::fs::create_dir_all(lib.dir()).unwrap();
        let good = testfont::build("Broken Neighbour", false);
        write(lib.dir(), "a-good.ttf", &good);
        write(lib.dir(), "b-cut.ttf", &good[..100]);
        write(lib.dir(), "c-noise.otf", &[0xAB; 3000]);
        write(lib.dir(), "d-note.txt", b"hi");
        write(lib.dir(), "e.ttf.part", &good);
        let all = lib.list();
        assert_eq!(all.len(), BUNDLED.len() + 1);
        assert_eq!(all.last().unwrap().family, "Broken Neighbour");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_font_is_one_line_per_name() {
        let n = missing_font_notes(["Anton", "Nope Sans", "nope sans", " ", "Other"]);
        assert_eq!(
            n,
            vec![
                "font “Nope Sans” is not installed; used the default".to_string(),
                "font “nope sans” is not installed; used the default".to_string(),
                "font “Other” is not installed; used the default".to_string(),
            ]
        );
    }

    #[test]
    fn the_cli_lists_adds_and_removes_in_the_given_data_dir() {
        use crate::cli::{FontsAction, FontsArgs};
        let dir = tmp("cli");
        let src = write(&dir, "cli-font.ttf", &testfont::build("Cli Marker", false));
        let run = |action| {
            run_cli(FontsArgs {
                action,
                data_dir: Some(dir.join("data")),
                json: true,
            })
        };
        run(None).unwrap();
        run(Some(FontsAction::Add { file: src.clone() })).unwrap();
        assert!(dir
            .join("data")
            .join("fonts")
            .join("cli-font.ttf")
            .is_file());
        assert!(run(Some(FontsAction::Add { file: src })).is_err());
        assert!(run(Some(FontsAction::Remove {
            font: "Anton".into()
        }))
        .is_err());
        run(Some(FontsAction::Remove {
            font: "cli marker".into(),
        }))
        .unwrap();
        assert!(!dir.join("data").join("fonts").join("cli-font.ttf").exists());
        let t = table(&Library::new(dir.join("data").join("fonts")).list());
        assert!(t.starts_with("FAMILY") && t.contains("Anton") && t.contains("bundled"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
