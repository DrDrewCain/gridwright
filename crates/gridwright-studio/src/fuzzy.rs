//! Subsequence matching, for finding a bus by typing part of its name.
//!
//! Written rather than depended on. The whole of it is one pass over a string,
//! and a matcher is a place where the *scoring* is the product — a crate would
//! bring its own opinion about what ranks above what, and that opinion is
//! exactly the thing worth choosing here.
//!
//! Subsequence rather than substring: `b12` should find `bus112`, and typing
//! the letters you remember in the order you remember them is how people search
//! when they half-know a name. Substring matching refuses that, which is why
//! Figma's documented keyword matching is visibly the weaker experience.

/// How well `needle` matches `haystack`, or `None` if it does not.
///
/// Higher is better. The absolute value means nothing; only the order does.
pub fn score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }

    // Lowercased once each rather than per character. Case-insensitive because
    // a person searching for a bus does not know or care how the file
    // capitalised it.
    //
    // Diacritics are *not* folded: `munchen` will not find `München`. Doing it
    // properly needs a Unicode normalisation table, and doing it improperly --
    // an ASCII-only fold -- would work for German and silently fail for every
    // other alphabet a network is named in. Typing the accent works.
    let n: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let raw: Vec<char> = haystack.chars().collect();
    let h: Vec<char> = raw.iter().flat_map(|c| c.to_lowercase()).collect();
    if n.len() > h.len() {
        return None;
    }

    // Dynamic programming rather than a greedy scan.
    //
    // Greedy takes the first character that matches and cannot take it back,
    // which gets the *wrong* answer rather than a worse one: searching `hv`
    // against `north_hv`, greedy consumes the `h` in "nort(h)" and then has no
    // `v` after it, so a name that plainly contains the query scores nothing.
    // Names here are short, so the quadratic table costs nothing worth saving.
    //
    // `best[i][j]` is the best score for matching the first `i` needle
    // characters within the first `j` haystack characters. `run[i][j]` is the
    // same, restricted to alignments where needle `i-1` matched haystack `j-1`,
    // which is what lets a contiguous streak compound.
    const MISS: i32 = i32::MIN / 4;
    let (rows, cols) = (n.len() + 1, h.len() + 1);
    let mut best = vec![MISS; rows * cols];
    let mut run = vec![MISS; rows * cols];
    // Matching nothing costs nothing, wherever we are.
    for j in 0..cols {
        best[j] = 0;
    }

    for i in 1..rows {
        for j in 1..cols {
            let at = i * cols + j;
            // Skip this haystack character.
            best[at] = best[at - 1];

            if n[i - 1] != h[j - 1] {
                continue;
            }

            let mut points = bonus(&raw, &h, j - 1);
            // A streak compounds, so a contiguous match outranks the same
            // letters scattered through a longer name.
            let streak = run[at - cols - 1];
            let scattered = best[at - cols - 1];
            let take = if streak > scattered.saturating_sub(1) && streak > MISS {
                points += 10;
                streak
            } else {
                scattered
            };
            if take <= MISS {
                continue;
            }
            run[at] = take + points;
            best[at] = best[at].max(run[at]);
        }
    }

    let total = best[(rows - 1) * cols + cols - 1];
    if total <= MISS {
        return None;
    }
    // Shorter names win ties: between `bus1` and `bus1_transformer_hv`, someone
    // who typed `bus1` meant the first one.
    Some(total - (h.len() as i32) / 4)
}

/// What a match at this position is worth before streaks are counted.
fn bonus(raw: &[char], h: &[char], j: usize) -> i32 {
    // The very start beats the middle: `gen` should find `gen_bus2` before
    // `hydrogen_link`, because a name usually begins with the thing it is.
    if j == 0 {
        return 14;
    }
    // A word boundary is nearly as good. The boundaries are the ones real
    // component names use -- separators, and the lower-to-upper transition in
    // camel case.
    let separator = matches!(h[j - 1], '_' | '-' | ' ' | '.' | '/' | ':');
    let camel = raw.get(j).is_some_and(|c| c.is_uppercase())
        && raw.get(j - 1).is_some_and(|c| c.is_lowercase());
    if separator || camel { 9 } else { 1 }
}

/// Whether `needle` matches at all.
#[cfg(test)]
fn matches(needle: &str, haystack: &str) -> bool {
    score(needle, haystack).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn best<'a>(needle: &str, names: &[&'a str]) -> &'a str {
        names
            .iter()
            .filter_map(|n| score(needle, n).map(|s| (s, *n)))
            .max_by_key(|(s, _)| *s)
            .expect("nothing matched")
            .1
    }

    #[test]
    fn a_subsequence_matches_and_a_missing_letter_does_not() {
        assert!(matches("b12", "bus112"));
        assert!(matches("bus", "bus1"));
        assert!(!matches("bz", "bus1"));
        // Order matters: the letters must appear in the order they were typed.
        assert!(!matches("21sub", "bus112"));
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(matches("BUS", "bus1"));
        assert!(matches("bus", "BUS1"));
    }

    #[test]
    fn a_prefix_beats_a_match_in_the_middle() {
        // Someone typing `gen` means the thing that starts with it.
        assert_eq!(best("gen", &["hydrogen_link", "gen_bus2"]), "gen_bus2");
    }

    #[test]
    fn a_word_boundary_beats_an_interior_match() {
        assert_eq!(best("hv", &["shvorto", "north_hv"]), "north_hv");
    }

    #[test]
    fn camel_case_counts_as_a_boundary() {
        assert_eq!(best("nb", &["nobody", "northBus"]), "northBus");
    }

    #[test]
    fn a_contiguous_run_beats_scattered_letters() {
        assert_eq!(best("bus", &["b_u_s_1", "bus1"]), "bus1");
    }

    #[test]
    fn the_shorter_of_two_equal_matches_wins() {
        // Both are prefix matches with the same run; the tie is broken by
        // length, because someone who typed `bus1` meant `bus1`.
        assert_eq!(best("bus1", &["bus1_transformer_hv", "bus1"]), "bus1");
    }

    #[test]
    fn scoring_a_name_against_itself_never_fails() {
        for name in ["a", "bus1", "gen_bus2", "NORTH-HV", "l.1/2:3"] {
            assert!(score(name, name).is_some(), "{name} did not match itself");
        }
    }

    #[test]
    fn a_query_longer_than_the_name_does_not_match() {
        assert!(!matches("bus1234", "bus1"));
    }

    #[test]
    fn non_ascii_names_do_not_panic() {
        // Real networks carry these: Ö in Nordic substation names, accents in
        // French ones. Indexing a `String` by byte would slice mid-character.
        assert!(matches("ö", "Örebro"));
        assert!(matches("süd", "München-Süd"));
        assert!(matches("nchen", "München-Süd"));
    }

    #[test]
    fn diacritics_are_not_folded_and_that_is_deliberate() {
        // `munchen` does not find `München`. Folding properly needs a Unicode
        // normalisation table; folding improperly -- an ASCII-only table --
        // would work for German and silently fail for every other alphabet a
        // network is named in. Pinned so the limitation is a decision rather
        // than a surprise.
        assert!(!matches("munchen", "München"));
    }

    #[test]
    fn a_greedy_scan_would_get_this_one_wrong() {
        // Greedy matching takes the `h` in "nort(h)" and then finds no `v`
        // after it, scoring a name that plainly contains the query at nothing.
        assert!(matches("hv", "north_hv"));
        assert!(matches("bs2", "bus_bus2"));
    }
}
