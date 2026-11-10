//! Language packs: `locales/<code>.json`, a flat "key": "text" map per language.
//! Texts may hold `{name}` placeholders, filled by `t!("key", name = value)`.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Ru,
    En,
    Zh,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::Ru, Lang::En, Lang::Zh];

    pub fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    /// The language's own name, for the language picker.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Ru => "Русский",
            Lang::En => "English",
            Lang::Zh => "中文",
        }
    }

    /// "ru", "ru-RU", "ru_RU.UTF-8", "zh-Hans-CN"...
    pub fn from_code(code: &str) -> Option<Lang> {
        let code = code.to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| code == l.code() || code.starts_with(&format!("{}-", l.code())) || code.starts_with(&format!("{}_", l.code())))
    }

    /// The OS UI language; English when it has no pack.
    pub fn system() -> Lang {
        sys_locale::get_locale().and_then(|l| Lang::from_code(&l)).unwrap_or(Lang::En)
    }
}

static PACKS: LazyLock<[HashMap<String, String>; 3]> = LazyLock::new(|| {
    [include_str!("../locales/ru.json"), include_str!("../locales/en.json"), include_str!("../locales/zh.json")].map(|s| serde_json::from_str(s).expect("invalid language pack"))
});

static CURRENT: AtomicU8 = AtomicU8::new(Lang::En as u8);

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    Lang::ALL[CURRENT.load(Ordering::Relaxed) as usize]
}

/// The text for `key` in the current language, English if missing there, else the key itself.
pub fn get(key: &'static str) -> &'static str {
    let packs = &*PACKS;
    packs[current() as usize].get(key).or_else(|| packs[Lang::En as usize].get(key)).map_or(key, String::as_str)
}

/// Replaces each `{name}` in `text`.
pub fn fill(text: &str, args: &[(&str, &dyn Display)]) -> String {
    args.iter().fold(text.to_string(), |s, (name, v)| s.replace(&format!("{{{name}}}"), &v.to_string()))
}

/// `t!("key")` -> `&'static str`; `t!("key", n = 5)` -> `String` with `{n}` filled.
#[macro_export]
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::get($key)
    };
    // The block drops the `&dyn Display` temporaries at once, so the result can live across `.await`.
    ($key:literal, $($name:ident = $val:expr),+ $(,)?) => {{
        $crate::i18n::fill($crate::i18n::get($key), &[$((stringify!($name), &$val as &dyn ::std::fmt::Display)),+])
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(s: &str) -> BTreeSet<&str> {
        s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|(name, _)| name)).collect()
    }

    #[test]
    fn packs_match() {
        let ru = &PACKS[Lang::Ru as usize];
        for lang in [Lang::En, Lang::Zh] {
            let pack = &PACKS[lang as usize];
            let missing: Vec<_> = ru.keys().filter(|k| !pack.contains_key(*k)).collect();
            let extra: Vec<_> = pack.keys().filter(|k| !ru.contains_key(*k)).collect();
            assert!(missing.is_empty() && extra.is_empty(), "{lang:?}: missing {missing:?}, extra {extra:?}");
            for (k, v) in ru {
                assert_eq!(placeholders(v), placeholders(&pack[k]), "{lang:?}: placeholders of {k}");
            }
        }
    }

    /// Every `t!("key"` in the sources has a text.
    #[test]
    fn keys_exist() {
        let mut dirs = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
        let mut files = vec![];
        while let Some(dir) = dirs.pop() {
            for path in std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()) {
                if path.is_dir() { dirs.push(path) } else { files.push(path) }
            }
        }
        for path in files {
            let src = std::fs::read_to_string(&path).unwrap();
            for (at, _) in src.match_indices("t!(\"") {
                // Skip `format!(` and the like.
                if src[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let key = src[at + 4..].split('"').next().unwrap();
                if key != "key" {
                    assert!(PACKS[Lang::Ru as usize].contains_key(key), "{}: unknown key {key}", path.display());
                }
            }
        }
    }

    #[test]
    fn lang_codes() {
        assert_eq!(Lang::from_code("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::from_code("zh-Hans-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_code("en"), Some(Lang::En));
        assert_eq!(Lang::from_code("de-DE"), None);
        assert_eq!(Lang::from_code("system"), None);
    }

    #[test]
    fn fills_placeholders() {
        assert_eq!(fill("{n} из {total}", &[("n", &1), ("total", &"3")]), "1 из 3");
    }
}
