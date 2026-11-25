//! UI and log translations, following the KalangoOS pattern (crates/kalango-i18n, as Kalango Audio does): source
//! strings are English and double as message ids (`tr("Recording")`); each locale has a catalog (English →
//! translation) embedded in the binary; the language is the one chosen in the system (`LANGUAGE`, `LC_ALL`,
//! `LC_MESSAGES`, `LANG`); a missing language or string falls back to English. The pt_BR catalog is split per part
//! (`locales/pt_BR/*.json`) and merged here. Every crate of the workspace uses this one (`cast_core::tr`).
//!
//! Placeholders are named: `trf("{n} frames", &[("n", n.to_string())])`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The pt_BR catalog files, merged at start (a key in two files is a mistake the test catches).
const PT_BR: &[(&str, &str)] = &[
    ("captura", include_str!("../locales/pt_BR/captura.json")),
    ("codificacao", include_str!("../locales/pt_BR/codificacao.json")),
    ("motor", include_str!("../locales/pt_BR/motor.json")),
    ("cli", include_str!("../locales/pt_BR/cli.json")),
];

const LOCALES: &[&str] = &["pt_BR"];

static ACTIVE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Normalizes a POSIX locale (`pt_BR.UTF-8@euro`) to a catalog name; `pt` alone maps to `pt_BR`.
pub fn normalize(locale: &str) -> Option<&'static str> {
    let base = locale.split(['.', '@']).next().unwrap_or("");
    if base.is_empty() || base == "C" || base == "POSIX" {
        return None;
    }
    LOCALES.iter().copied().find(|n| *n == base).or_else(|| {
        let lang = base.split('_').next().unwrap_or("");
        LOCALES.iter().copied().find(|n| n.split('_').next() == Some(lang))
    })
}

/// The catalog chosen by the system environment (or None for English).
pub fn system_locale() -> Option<&'static str> {
    if let Ok(list) = std::env::var("LANGUAGE") {
        if let Some(l) = list.split(':').find_map(normalize) {
            return Some(l);
        }
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return normalize(&v);
            }
        }
    }
    None
}

pub fn catalog(locale: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if locale == Some("pt_BR") {
        for (_, json) in PT_BR {
            if let Ok(m) = serde_json::from_str::<HashMap<String, String>>(json) {
                out.extend(m);
            }
        }
    }
    out
}

fn active() -> &'static HashMap<String, String> {
    // unit tests read the English source strings, whatever the machine's language
    ACTIVE.get_or_init(|| if cfg!(test) { HashMap::new() } else { catalog(system_locale()) })
}

/// Marks a string for translation without translating it yet (for `const` tables); translate with [`tr`] when shown.
pub const fn n_(msgid: &'static str) -> &'static str {
    msgid
}

/// Translates a UI string (English in the source).
pub fn tr(msgid: &str) -> &str {
    match active().get(msgid) {
        Some(t) => t.as_str(),
        None => msgid,
    }
}

/// Translates and fills named placeholders (`{name}`).
pub fn trf(msgid: &str, args: &[(&str, String)]) -> String {
    let mut s = tr(msgid).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    #[test]
    fn locale_detection() {
        assert_eq!(normalize("pt_BR.UTF-8"), Some("pt_BR"));
        assert_eq!(normalize("pt_PT.UTF-8"), Some("pt_BR"), "same language falls to the closest catalog");
        assert_eq!(normalize("en_US.UTF-8"), None);
        assert_eq!(normalize("C.UTF-8"), None);
    }

    /// The msgids marked in the sources of every crate of the workspace (tr, trf, n_).
    fn used_ids() -> BTreeSet<String> {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let mut used = BTreeSet::new();
        let mut dirs: Vec<_> = std::fs::read_dir(&crates).unwrap().flatten().map(|e| e.path().join("src")).filter(|p| p.is_dir()).collect();
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                    continue;
                }
                if p.extension().is_some_and(|x| x == "rs") && p.file_name().is_some_and(|n| n != "i18n.rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    for marker in ["tr(", "trf(", "n_("] {
                        let mut rest = text.as_str();
                        while let Some(i) = rest.find(marker) {
                            // (only a call: `str(` or `attr(` are not markers)
                            let before = &rest[..i];
                            rest = &rest[i + marker.len()..];
                            if before.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                                continue;
                            }
                            // the string may start on the next line
                            let after = rest.trim_start();
                            let Some(body) = after.strip_prefix('"') else { continue };
                            let bytes = body.as_bytes();
                            let mut end = 0;
                            while end < bytes.len() && !(bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\')) {
                                end += 1;
                            }
                            used.insert(body[..end].replace("\\\"", "\"").replace("\\n", "\n"));
                        }
                    }
                }
            }
        }
        used
    }

    /// Every string marked for translation has a pt_BR entry, no entry is left unused, no key is in two files, and
    /// placeholders survive translation.
    #[test]
    fn catalog_is_complete_and_has_no_leftovers() {
        let used = used_ids();
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        for (file, json) in PT_BR {
            let m: HashMap<String, String> = serde_json::from_str(json).unwrap_or_else(|e| panic!("locales/pt_BR/{file}.json: {e}"));
            for k in m.keys() {
                if let Some(other) = seen.insert(k.clone(), file) {
                    panic!("'{k}' está em {other}.json e em {file}.json");
                }
            }
        }
        let pt = catalog(Some("pt_BR"));
        let missing: Vec<_> = used.iter().filter(|m| !pt.contains_key(*m)).collect();
        let unused: Vec<_> = pt.keys().filter(|k| !used.contains(*k)).collect();
        assert!(missing.is_empty(), "sem tradução pt_BR: {missing:#?}");
        assert!(unused.is_empty(), "traduções sem uso: {unused:#?}");
        for (k, v) in &pt {
            for part in k.split('{').skip(1) {
                let name = part.split('}').next().unwrap();
                assert!(v.contains(&format!("{{{name}}}")), "'{k}' → '{v}' perdeu {{{name}}}");
            }
        }
    }
}
