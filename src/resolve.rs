// pg_graphwright/resolve — name normalization and phonetic keys, ported
// from the graphwright TS core (src/resolve/normalize.ts, phonetic/*).
// Pure, deterministic: the lexical stages of the resolution cascade. Used
// to fold entity surfaces (exact, on the normalized key) and to propose
// cross-script matches (phonetic keys).
//
// NFKC is not applied yet (the TS core runs it first); the explicit
// Arabic->Persian folds plus diacritic removal cover typed-name variants.

fn fold_char(c: char) -> char {
    match c {
        'ي' => 'ی',             // Arabic yeh -> Persian yeh
        'ك' => 'ک',             // Arabic kaf -> Persian keheh
        'آ' | 'أ' | 'إ' => 'ا', // alef variants -> bare alef
        'ة' => 'ه',             // teh marbuta -> heh
        _ => c,
    }
}

// Arabic combining diacritics + Quranic marks, tatweel, ZWNJ/ZWJ: padding
// and joins that never carry identity.
fn is_removable(c: char) -> bool {
    let u = c as u32;
    (0x064B..=0x0670).contains(&u)
        || (0x06D6..=0x06ED).contains(&u)
        || u == 0x0640
        || u == 0x200C
        || u == 0x200D
}

/// Aggressively normalize a name for matching (never for display). Folds
/// Arabic/Persian codepoint variants, strips diacritics/tatweel/ZWNJ,
/// lowercases, and trims surrounding punctuation, collapsing whitespace.
pub fn normalize_name(raw: &str) -> String {
    let folded: String = raw
        .chars()
        .map(fold_char)
        .filter(|c| !is_removable(*c))
        .collect();
    let lower = folded.to_lowercase();
    let trimmed = lower.trim_matches(|c: char| !c.is_alphanumeric());
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─── phonetic keys ──────────────────────────────────────────────────
//
// Cross-script consonant skeletons: "Faeze" and "فائزه" share zero
// character shingles, but map onto the same key space so unseen cross-
// script spellings meet. Keys are lossy on purpose; a collision is a
// PROPOSAL for review, never an auto-merge.

const MAX_KEYS: usize = 8;

fn collapse_repeats(s: &str) -> String {
    let mut out = String::new();
    let mut last = None;
    for ch in s.chars() {
        if Some(ch) != last {
            out.push(ch);
            last = Some(ch);
        }
    }
    out
}

fn dedupe_cap(xs: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for x in xs {
        if !x.is_empty() && seen.insert(x.clone()) {
            out.push(x);
            if out.len() >= MAX_KEYS {
                break;
            }
        }
    }
    out
}

fn expand_classes(class_seq: &[Vec<&str>]) -> Vec<String> {
    let mut variants = vec![String::new()];
    for classes in class_seq {
        let mut next = Vec::new();
        for v in &variants {
            for c in classes {
                next.push(format!("{v}{c}"));
            }
        }
        // Cap intermediate growth (dedupe keeps empties here for prefixing).
        let mut seen = std::collections::HashSet::new();
        variants = next
            .into_iter()
            .filter(|x| seen.insert(x.clone()))
            .take(MAX_KEYS)
            .collect();
    }
    dedupe_cap(variants.iter().map(|v| collapse_repeats(v)).collect())
}

// Latin scheme: romanizations. Digraphs that transliterate single letters
// of other scripts (kh, gh, sh, ch, zh) map onto the same symbols the
// sibling schemes use; vowels drop.
fn latin_matches(word: &str) -> bool {
    word.chars().any(|c| c.is_ascii_lowercase())
}

fn latin_word_keys(word: &str) -> Vec<String> {
    let mut s = word.replace('\'', "");
    for (d, sym) in [
        ("kh", "X"),
        ("gh", "Q"),
        ("sh", "C"),
        ("ch", "C"),
        ("zh", "J"),
        ("ph", "F"),
    ] {
        s = s.replace(d, sym);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() {
            out.push(ch.to_ascii_lowercase()); // digraph placeholder, pass through
            continue;
        }
        if "aeiou".contains(ch) {
            continue;
        }
        if !ch.is_ascii_lowercase() {
            continue;
        }
        // Glides are consonants word-initially, vowel-colored elsewhere.
        if (ch == 'y' || ch == 'w') && i > 0 {
            continue;
        }
        if ch == 'w' {
            out.push('v');
            continue;
        }
        out.push(if ch == 'c' { 'k' } else { ch });
    }
    let key = collapse_repeats(&out);
    if key.is_empty() {
        return vec![];
    }
    // Word-final h after a vowel forks (Sarah/Sara), mirroring silent heh.
    let bare: Vec<char> = s.chars().filter(|c| !c.is_ascii_uppercase()).collect();
    let ends_vowel_h =
        bare.len() >= 2 && bare[bare.len() - 1] == 'h' && "aeiou".contains(bare[bare.len() - 2]);
    if ends_vowel_h && key.ends_with('h') {
        return dedupe_cap(vec![key.clone(), key[..key.len() - 1].to_string()]);
    }
    vec![key]
}

// Persian scheme: Perso-Arabic letters onto the shared symbol space.
// Arabic-only letters fold to their Iranian pronunciation; glides and
// final heh fork.
fn persian_class(ch: char) -> Option<Vec<&'static str>> {
    let c: &[&str] = match ch {
        'ب' => &["b"],
        'پ' => &["p"],
        'ت' | 'ط' => &["t"],
        'ث' | 'س' | 'ص' => &["s"],
        'ج' => &["j"],
        'چ' => &["c"],
        'ح' | 'ه' | 'ة' => &["h"],
        'خ' => &["x"],
        'د' => &["d"],
        'ذ' | 'ز' | 'ض' | 'ظ' => &["z"],
        'ر' => &["r"],
        'ژ' => &["j"],
        'ش' => &["c"],
        'ف' => &["f"],
        'ک' | 'ك' => &["k"],
        'گ' => &["g"],
        'ل' => &["l"],
        'م' => &["m"],
        'ن' => &["n"],
        'ق' | 'غ' => &["q"],
        // Glottal carriers and pure vowels contribute nothing.
        'ع' | 'ء' | 'ئ' | 'أ' | 'إ' | 'ؤ' | 'ا' | 'آ' => &[""],
        // Ambiguous glides.
        'و' => &["v", ""],
        'ی' | 'ي' => &["y", ""],
        _ => return None,
    };
    Some(c.to_vec())
}

fn persian_matches(word: &str) -> bool {
    word.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
}

fn persian_word_keys(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut class_seq: Vec<Vec<&str>> = Vec::new();
    for (i, &ch) in chars.iter().enumerate() {
        let classes = match persian_class(ch) {
            Some(c) => c,
            None => continue,
        };
        // Word-initial glides are reliably consonants.
        if i == 0 && (ch == 'و' || ch == 'ی' || ch == 'ي') {
            class_seq.push(vec![classes[0]]);
            continue;
        }
        // Word-final heh is routinely dropped in romanization.
        if i == chars.len() - 1 && (ch == 'ه' || ch == 'ة') {
            class_seq.push(vec!["h", ""]);
            continue;
        }
        class_seq.push(classes);
    }
    expand_classes(&class_seq)
}

/// Phonetic keys for a name (any script, possibly multi-word). Per-word
/// skeletons joined by a space; the Persian scheme is consulted before
/// Latin. An empty result means no scheme claimed any word.
pub fn phonetic_keys(name: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphabetic() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    let keyed: Vec<Vec<String>> = words
        .iter()
        .map(|w| {
            if persian_matches(w) {
                persian_word_keys(w)
            } else if latin_matches(w) {
                latin_word_keys(w)
            } else {
                vec![]
            }
        })
        .filter(|ks| !ks.is_empty())
        .collect();
    if keyed.is_empty() {
        return vec![];
    }
    let mut keys = vec![String::new()];
    for word_keys in &keyed {
        let mut next = Vec::new();
        for prefix in &keys {
            for wk in word_keys {
                next.push(if prefix.is_empty() {
                    wk.clone()
                } else {
                    format!("{prefix} {wk}")
                });
            }
        }
        next.truncate(MAX_KEYS * 4);
        keys = next;
    }
    let mut seen = std::collections::HashSet::new();
    keys.into_iter()
        .filter(|k| seen.insert(k.clone()))
        .collect()
}
