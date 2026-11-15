//! Slug generation shared by every entity that has one (organization, user,
//! arrangement, voice, collection).
//!
//! A slug is immutable path and object-key material (WebDAV folders, S3 keys),
//! so it stays plain ASCII: lowercase letters, numbers and single hyphens. The
//! human-facing name keeps its Unicode; the slug is made *readable* by
//! transliterating rather than dropping what is not ASCII (#42):
//!
//! - German umlauts expand the way a German speaker writes them without
//!   umlauts: `ä` → `ae`, `ö` → `oe`, `ü` → `ue` (and `ß` → `ss`), so
//!   "Märsche" becomes `maersche`, not `marsche`. Lied's first users are
//!   German-speaking bands; for them `marsche` reads as a different word.
//! - Everything else goes through [`deunicode`]: accents drop
//!   ("Boléro" → `bolero`), other scripts are romanised ("Щелкунчик" →
//!   `shchelkunchik`, "序曲" → `xu-qu`).
//! - What still yields nothing (punctuation or symbols only) becomes
//!   `<entity>-<8 hex characters>`, taken from a SHA-256 of the input.
//!   Distinct inputs therefore get distinct slugs instead of all colliding on
//!   one fixed word, and the same input always gets the same slug, as any
//!   other title does.
//!
//! **Stability:** the output for a given input must not change between
//! releases, because two rows of one title created before and after a change
//! would get different slugs. `deunicode` is pinned to an exact version in the
//! workspace `Cargo.toml`, and the tests below pin the output; a dependency
//! bump that changes them is a deliberate, breaking change.

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Slugify `input`, falling back to `<fallback>-<hash>` when nothing
/// alphanumeric survives transliteration. `fallback` names the entity
/// (`"arrangement"`, `"user"`, …).
pub fn slugify(input: &str, fallback: &str) -> String {
    // Compose first: macOS sends names decomposed ("a" + U+0308), and the
    // umlaut expansion only matches the precomposed character.
    let composed: String = input.nfc().collect();
    let ascii = deunicode::deunicode(&expand_german(&composed));
    let mut slug = String::with_capacity(ascii.len());
    let mut pending_hyphen = false;
    for ch in ascii.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_hyphen && !slug.is_empty() {
                slug.push('-');
            }
            pending_hyphen = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    if slug.is_empty() {
        let hash = Sha256::digest(input.as_bytes());
        slug = format!(
            "{fallback}-{:02x}{:02x}{:02x}{:02x}",
            hash[0], hash[1], hash[2], hash[3]
        );
    }
    slug
}

/// Expand German umlauts and ß before the generic transliteration, which
/// would only strip the diaeresis.
fn expand_german(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            'ä' => out.push_str("ae"),
            'ö' => out.push_str("oe"),
            'ü' => out.push_str("ue"),
            'Ä' => out.push_str("Ae"),
            'Ö' => out.push_str("Oe"),
            'Ü' => out.push_str("Ue"),
            'ß' | 'ẞ' => out.push_str("ss"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::slugify;

    fn s(input: &str) -> String {
        slugify(input, "arrangement")
    }

    #[test]
    fn ascii_is_lowercased_and_hyphenated() {
        assert_eq!(s("Symphony No. 5"), "symphony-no-5");
        assert_eq!(s("  weird___name!!"), "weird-name");
        assert_eq!(s("admin"), "admin");
    }

    #[test]
    fn accented_latin_is_transliterated_not_dropped() {
        assert_eq!(s("Boléro"), "bolero");
        assert_eq!(s("Peer Gynt: Åse's Death"), "peer-gynt-ase-s-death");
        assert_eq!(s("Dvořák: Slavonic Dance"), "dvorak-slavonic-dance");
        assert_eq!(s("Façade"), "facade");
    }

    #[test]
    fn german_umlauts_expand() {
        assert_eq!(s("Märsche"), "maersche");
        assert_eq!(s("Frühschoppen"), "fruehschoppen");
        assert_eq!(s("Böhmischer Traum"), "boehmischer-traum");
        assert_eq!(s("ÜBER ALLES"), "ueber-alles");
        assert_eq!(s("Straße"), "strasse");
    }

    #[test]
    fn decomposed_umlauts_slug_like_precomposed_ones() {
        // macOS clients send NFD ("a" + combining diaeresis); the same title
        // must get the same slug whichever form it arrives in.
        assert_eq!(s("Ma\u{0308}rsche"), "maersche");
        assert_eq!(s("Ma\u{0308}rsche"), s("Märsche"));
    }

    #[test]
    fn other_scripts_are_romanised() {
        assert_eq!(s("Щелкунчик"), "shchelkunchik");
        assert_eq!(s("序曲"), "xu-qu");
        assert_eq!(s("Συμφωνία"), "sumphonia");
    }

    #[test]
    fn emoji_are_transliterated_to_words() {
        assert_eq!(s("🎺"), "trumpet");
    }

    #[test]
    fn nothing_alphanumeric_falls_back_to_entity_and_hash() {
        let a = s("!!!");
        let b = s("???");
        assert!(
            a.starts_with("arrangement-") && a.len() == "arrangement-".len() + 8,
            "{a}"
        );
        assert_ne!(a, b, "distinct inputs must not collide on one fixed word");
        assert_eq!(a, s("!!!"), "the same input always gives the same slug");
        assert_eq!(slugify("", "voice").len(), "voice-".len() + 8);
        assert!(slugify("---", "user").starts_with("user-"));
    }

    #[test]
    fn output_is_pinned() {
        // A change here changes slugs for new rows: treat it as breaking.
        assert_eq!(s("!!!"), "arrangement-e84c538e");
    }
}
