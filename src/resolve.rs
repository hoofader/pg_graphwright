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
