// pg_graphwright/resolve — name normalization and phonetic keys, ported
// from the graphwright TS core (src/resolve/normalize.ts, phonetic/*).
// Pure, deterministic: the lexical stages of the resolution cascade. Used
// to fold entity surfaces (exact, on the normalized key) and to propose
// cross-script matches (phonetic keys).

use unicode_normalization::UnicodeNormalization;

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

/// Aggressively normalize a name for matching (never for display). Applies
/// NFKC first (so compatibility forms, presentation forms, and ligatures
/// reach one shape), then folds Arabic/Persian codepoint variants, strips
/// diacritics/tatweel/ZWNJ, lowercases, and trims surrounding punctuation.
pub fn normalize_name(raw: &str) -> String {
    let folded: String = raw
        .nfkc()
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

// Cyrillic scheme: Russian-leaning (г is g). Vowels and the soft/hard signs
// drop; the й glide and word-initial iotated vowels fork. So "хабаров" and
// "khabarov" both reduce to "xbrv".
fn cyrillic_matches(word: &str) -> bool {
    word.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

fn cyrillic_class(ch: char) -> Option<Vec<&'static str>> {
    let c: &[&str] = match ch {
        'б' => &["b"],
        'в' => &["v"],
        'г' => &["g"],
        'д' => &["d"],
        'ж' => &["j"],
        'з' => &["z"],
        'к' => &["k"],
        'л' => &["l"],
        'м' => &["m"],
        'н' => &["n"],
        'п' => &["p"],
        'р' => &["r"],
        'с' => &["s"],
        'т' => &["t"],
        'ф' => &["f"],
        'х' => &["x"],
        'ц' => &["ts"],
        'ч' | 'ш' | 'щ' => &["c"],
        // Ambiguous glide.
        'й' => &["y", ""],
        // Vowels and the soft/hard signs contribute nothing.
        'а' | 'о' | 'у' | 'ы' | 'э' | 'и' | 'е' | 'ё' | 'я' | 'ю' | 'і' | 'ї' | 'є' | 'ь'
        | 'ъ' => &[""],
        _ => return None,
    };
    Some(c.to_vec())
}

fn cyrillic_word_keys(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut class_seq: Vec<Vec<&str>> = Vec::new();
    for (i, &ch) in chars.iter().enumerate() {
        // Word-initial iotated vowels begin with a y-glide.
        if i == 0 && matches!(ch, 'я' | 'ю' | 'ё' | 'е' | 'є' | 'ї') {
            class_seq.push(vec!["y", ""]);
            continue;
        }
        let classes = match cyrillic_class(ch) {
            Some(c) => c,
            None => continue,
        };
        // Word-initial й is reliably a consonant.
        if i == 0 && ch == 'й' {
            class_seq.push(vec!["y"]);
            continue;
        }
        class_seq.push(classes);
    }
    expand_classes(&class_seq)
}

/// Phonetic keys for a name (any script, possibly multi-word). Per-word
/// skeletons joined by a space; Persian then Cyrillic are consulted before
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
            } else if cyrillic_matches(w) {
                cyrillic_word_keys(w)
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

// ─── entropy gate ───────────────────────────────────────────────────
//
// Short, low-entropy names ("Ali", "علی", "bob") produce false matches:
// one edit reaches a different real name. The gate keeps them out of
// phonetic auto-merge (they stay proposals); distinctive names auto-merge.

const ENTROPY_THRESHOLD: f64 = 2.0;

fn shannon_entropy(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let mut n = 0usize;
    for ch in s.chars() {
        *counts.entry(ch).or_insert(0usize) += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let mut h = 0.0;
    for &c in counts.values() {
        let p = c as f64 / n as f64;
        h -= p * p.log2();
    }
    h
}

/// Distinctive enough to auto-merge a phonetic match (>= 2 bits ~ four
/// reasonably distinct characters). "علی" and "bob" fail; longer names pass.
pub fn passes_entropy_gate(normalized: &str) -> bool {
    shannon_entropy(normalized) >= ENTROPY_THRESHOLD
}

// ─── canonical merge (union-find) ───────────────────────────────────
//
// Entities are keyed by a canonical norm: norms linked by a merge edge
// (manual decision, or gated phonetic) collapse to one entity. The rep is
// the lexicographically smallest norm, so the result is order-independent.

use std::collections::{HashMap, HashSet};

fn find(parent: &mut HashMap<String, String>, x: &str) -> String {
    let mut root = x.to_string();
    while parent[&root] != root {
        root = parent[&root].clone();
    }
    let mut cur = x.to_string();
    while parent[&cur] != root {
        let next = parent[&cur].clone();
        parent.insert(cur, root.clone());
        cur = next;
    }
    root
}

/// Map each norm to its canonical norm given merge edges. The smaller norm
/// of a union becomes the root, so canon(norm) is deterministic.
pub fn canonical_map(
    norms: &HashSet<String>,
    merges: &[(String, String)],
) -> HashMap<String, String> {
    let mut parent: HashMap<String, String> =
        norms.iter().map(|n| (n.clone(), n.clone())).collect();
    for (a, b) in merges {
        if !parent.contains_key(a) || !parent.contains_key(b) {
            continue;
        }
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            let (keep, drop) = if ra <= rb { (ra, rb) } else { (rb, ra) };
            parent.insert(drop, keep);
        }
    }
    let mut canon = HashMap::new();
    for n in norms {
        let r = find(&mut parent, n);
        canon.insert(n.clone(), r);
    }
    canon
}

/// Norm pairs (sorted) that should auto-merge on a shared phonetic key,
/// limited to names distinctive enough to pass the entropy gate.
pub fn gated_phonetic_pairs(norms: &HashSet<String>) -> Vec<(String, String)> {
    let mut by_key: HashMap<String, Vec<String>> = HashMap::new();
    for n in norms {
        if !passes_entropy_gate(n) {
            continue;
        }
        for k in phonetic_keys(n) {
            by_key.entry(k).or_default().push(n.clone());
        }
    }
    let mut pairs = HashSet::new();
    for (_, mut ns) in by_key {
        ns.sort();
        ns.dedup();
        for i in 0..ns.len() {
            for j in (i + 1)..ns.len() {
                pairs.insert((ns[i].clone(), ns[j].clone()));
            }
        }
    }
    pairs.into_iter().collect()
}

// ─── fuzzy (3-gram Jaccard) ─────────────────────────────────────────
//
// Character-shingle similarity, ported from the TS core's minhash.ts.
// Catches typo/transposition variants that the consonant skeletons miss
// (a changed consonant forks the phonetic key but barely moves Jaccard).

const SHINGLE_SIZE: usize = 3;
const FUZZY_THRESHOLD: f64 = 0.82;

fn shingles(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = HashSet::new();
    if chars.is_empty() {
        return out;
    }
    if chars.len() <= SHINGLE_SIZE {
        out.insert(s.to_string());
        return out;
    }
    for w in chars.windows(SHINGLE_SIZE) {
        out.insert(w.iter().collect());
    }
    out
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let inter = small.iter().filter(|x| large.contains(*x)).count();
    inter as f64 / (a.len() + b.len() - inter) as f64
}

/// Norm pairs (sorted) within the Jaccard threshold on 3-gram shingles,
/// limited to names distinctive enough to pass the entropy gate. All-pairs
/// (O(n^2)); past a few hundred norms an LSH band prefilter would replace
/// the inner scan (minhash.ts has the band keys to port when that bites).
pub fn gated_fuzzy_pairs(norms: &HashSet<String>) -> Vec<(String, String)> {
    let gated: Vec<(&String, HashSet<String>)> = norms
        .iter()
        .filter(|n| passes_entropy_gate(n))
        .map(|n| (n, shingles(n)))
        .collect();
    let mut pairs = Vec::new();
    for i in 0..gated.len() {
        for j in (i + 1)..gated.len() {
            if jaccard(&gated[i].1, &gated[j].1) >= FUZZY_THRESHOLD {
                let (a, b) = (gated[i].0, gated[j].0);
                let (lo, hi) = if a <= b {
                    (a.clone(), b.clone())
                } else {
                    (b.clone(), a.clone())
                };
                pairs.push((lo, hi));
            }
        }
    }
    pairs
}

// ─── embedding (cosine over an injected embedder) ───────────────────
//
// The semantic rescue lane: it merges names the lexical gate dropped
// (short, low-entropy) when their vectors are close. Vectors come from
// the graphwright.embedder seam; this is the pure scoring half. Not
// entropy-gated, on purpose — rescuing short names is the whole point.

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Norm pairs (sorted) whose embeddings are within the cosine threshold.
/// All-pairs O(n^2) over the supplied vectors.
pub fn embedding_pairs(vectors: &[(String, Vec<f64>)], threshold: f64) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for i in 0..vectors.len() {
        for j in (i + 1)..vectors.len() {
            if cosine(&vectors[i].1, &vectors[j].1) >= threshold {
                let (a, b) = (&vectors[i].0, &vectors[j].0);
                let (lo, hi) = if a <= b {
                    (a.clone(), b.clone())
                } else {
                    (b.clone(), a.clone())
                };
                pairs.push((lo, hi));
            }
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xs: &[&str]) -> HashSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cyrillic_bridges_to_latin() {
        // Хабаров and Khabarov both reduce to "xbrv".
        let ru = phonetic_keys("Хабаров");
        let en = phonetic_keys("Khabarov");
        assert!(ru.iter().any(|k| en.contains(k)), "ru={ru:?} en={en:?}");
        // Сергей shares "srg" with Sergei.
        let s_ru = phonetic_keys("Сергей");
        let s_en = phonetic_keys("Sergei");
        assert!(s_ru.iter().any(|k| s_en.contains(k)), "{s_ru:?} {s_en:?}");
        // Unrelated names do not bridge.
        let ivan = phonetic_keys("Иванов");
        assert!(!ivan.iter().any(|k| en.contains(k)));
    }

    #[test]
    fn nfkc_folds_compatibility_forms() {
        // Full-width Latin and the ﬁ ligature reach the same plain key.
        assert_eq!(normalize_name("Ｒｅｚａ"), "reza");
        assert_eq!(normalize_name("ﬁrouz"), "firouz");
        // Already-normal names are unchanged by NFKC.
        assert_eq!(normalize_name("Reza"), "reza");
    }

    #[test]
    fn embedding_merges_only_close_vectors() {
        let v = vec![
            ("ali".to_string(), vec![1.0, 0.0]),
            ("علی".to_string(), vec![0.98, 0.2]),
            ("reza".to_string(), vec![0.0, 1.0]),
        ];
        let pairs = embedding_pairs(&v, 0.83);
        assert_eq!(pairs, vec![("ali".to_string(), "علی".to_string())]);
    }

    #[test]
    fn jaccard_is_one_on_identity_zero_on_disjoint() {
        let a = shingles("esfandiyar");
        assert_eq!(jaccard(&a, &a), 1.0);
        assert_eq!(jaccard(&shingles("abcdef"), &shingles("uvwxyz")), 0.0);
    }

    #[test]
    fn fuzzy_catches_a_consonant_typo_that_phonetic_forks() {
        // Same name, last consonant differs: the phonetic skeletons fork
        // (…m vs …n), so only the fuzzy lane (Jaccard ~0.87) links them.
        let norms = set(&["shahrbanoodeylam", "shahrbanoodeylan"]);
        assert!(gated_phonetic_pairs(&norms).is_empty());
        let fuzzy = gated_fuzzy_pairs(&norms);
        assert_eq!(fuzzy.len(), 1);
        assert_eq!(
            fuzzy[0],
            (
                "shahrbanoodeylam".to_string(),
                "shahrbanoodeylan".to_string()
            )
        );
    }

    #[test]
    fn fuzzy_respects_the_threshold_and_the_entropy_gate() {
        // Distinctive but only ~0.56 Jaccard: below the bar, no merge.
        assert!(gated_fuzzy_pairs(&set(&["khorasani", "khorasari"])).is_empty());
        // Near-identical but low-entropy (short): the gate drops it.
        assert!(gated_fuzzy_pairs(&set(&["anna", "anaa"])).is_empty());
    }
}
