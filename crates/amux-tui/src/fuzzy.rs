//! Fuzzy matching for the sidebar's `/` search: is the query a (case-insensitive) subsequence of
//! the candidate, and how good is the best such alignment? Small on purpose — candidates are
//! `repo/name` strings, a few dozen at most, so an O(query × candidate) DP is free.

/// Every matched char scores this much…
const MATCH: i32 = 1;
/// …plus this when it starts a word (the candidate's first char, or one after a separator), so
/// `fb` prefers `foo-bar` over `xfxb`…
const BOUNDARY: i32 = 8;
/// …plus this when it directly follows the previous matched char. Worth more than a boundary, so
/// a contiguous run (`bar` in `bar`) beats the same letters scattered across word starts (`b_a_r`).
const CONSECUTIVE: i32 = 10;

fn is_separator(c: char) -> bool {
    matches!(c, '/' | '-' | '_' | '.' | ' ')
}

/// The score of the best alignment of `query` inside `candidate` (higher is better), or `None`
/// when `query` is not a subsequence of it. An empty query matches everything with score 0.
pub(crate) fn score(query: &str, candidate: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let c: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let bonus = |j: usize| {
        MATCH
            + if j == 0 || is_separator(c[j - 1]) {
                BOUNDARY
            } else {
                0
            }
    };
    // `prev[j]`: the best score for `q[..i]` with `q[i - 1]` placed at `c[j]` (`None` if impossible).
    let mut prev: Vec<Option<i32>> = vec![None; c.len()];
    for (i, &qc) in q.iter().enumerate() {
        let mut cur = vec![None; c.len()];
        // Best `prev[k]` over `k < j - 1` — a placement that leaves a gap before `j`.
        let mut gapped: Option<i32> = None;
        for j in 0..c.len() {
            if j >= 2 {
                gapped = gapped.max(prev[j - 2]);
            }
            if c[j] != qc {
                continue;
            }
            let before = if i == 0 {
                Some(0)
            } else {
                let adjacent = j
                    .checked_sub(1)
                    .and_then(|k| prev[k])
                    .map(|s| s + CONSECUTIVE);
                gapped.max(adjacent)
            };
            cur[j] = before.map(|s| s + bonus(j));
        }
        prev = cur;
    }
    prev.into_iter().flatten().max()
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("", ""), Some(0));
    }

    #[test]
    fn requires_an_in_order_subsequence() {
        assert!(score("abc", "a-b-c").is_some());
        assert_eq!(score("abc", "acb"), None);
        assert_eq!(score("x", "abc"), None);
        assert_eq!(score("abcd", "abc"), None, "longer than the candidate");
    }

    #[test]
    fn is_case_insensitive() {
        assert!(score("AMUX", "amux/main").is_some());
        assert!(score("amux", "AMUX/Main").is_some());
    }

    #[test]
    fn word_starts_beat_mid_word_letters() {
        let good = score("fb", "foo-bar").unwrap();
        let bad = score("fb", "xfxb").unwrap();
        assert!(good > bad, "{good} vs {bad}");
    }

    #[test]
    fn a_contiguous_run_beats_scattered_word_starts() {
        let run = score("bar", "bar").unwrap();
        let scattered = score("bar", "b_a_r").unwrap();
        assert!(run > scattered, "{run} vs {scattered}");
    }

    #[test]
    fn picks_the_best_alignment_not_the_first() {
        // Greedy would take the `m` in `amux` and then scatter; the best alignment is `main`.
        let best = score("main", "amux/main").unwrap();
        let exact = score("main", "main").unwrap();
        assert_eq!(
            best, exact,
            "the `main` after the slash is a word start too"
        );
    }
}
