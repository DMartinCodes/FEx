//! Type-to-find ranking. Explorer only matches name prefixes; this ranks, in
//! order: prefix, start of a word inside the name, substring, then fuzzy
//! subsequence (`q3rp` → "Q3 report"). Match positions are returned so the UI
//! can highlight them.

/// Score bands keep the tiers strictly ordered.
const PREFIX: i32 = 40_000;
const WORD: i32 = 30_000;
const SUBSTRING: i32 = 20_000;
const FUZZY: i32 = 10_000;

pub struct Match {
    pub score: i32,
    /// Character indices of matched characters in the name.
    pub positions: Vec<usize>,
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Positions where a "word" starts: after a separator, at a lower→upper
/// case change (camelCase), or at a letter/digit change.
fn boundaries(name: &[char]) -> Vec<bool> {
    (0..name.len())
        .map(|i| {
            if i == 0 {
                return true;
            }
            let (p, c) = (name[i - 1], name[i]);
            matches!(p, ' ' | '_' | '-' | '.' | '(' | '[' | '{' | ',' | '+' | '&')
                || (p.is_lowercase() && c.is_uppercase())
                || (p.is_alphabetic() && c.is_ascii_digit())
                || (p.is_ascii_digit() && c.is_alphabetic())
        })
        .collect()
}

pub fn score(query: &str, name: &str) -> Option<Match> {
    let q: Vec<char> = query.chars().map(fold).collect();
    if q.is_empty() {
        return None;
    }
    let orig: Vec<char> = name.chars().collect();
    let n: Vec<char> = orig.iter().copied().map(fold).collect();
    if q.len() > n.len() {
        return None;
    }
    // Shorter names win ties within a tier ("report" before "report final").
    let len_penalty = (n.len() as i32).min(200);
    let span = |start: usize| (start..start + q.len()).collect::<Vec<_>>();

    if n.starts_with(&q) {
        return Some(Match { score: PREFIX - len_penalty, positions: span(0) });
    }
    let bounds = boundaries(&orig);
    let mut first_sub = None;
    for i in 1..=n.len() - q.len() {
        if n[i..i + q.len()] == q[..] {
            if bounds[i] {
                return Some(Match { score: WORD - (i as i32) * 4 - len_penalty, positions: span(i) });
            }
            first_sub.get_or_insert(i);
        }
    }
    if let Some(i) = first_sub {
        return Some(Match { score: SUBSTRING - (i as i32) * 4 - len_penalty, positions: span(i) });
    }
    if q.len() < 2 {
        return None;
    }
    // In fuzzy mode the extension isn't a meaningful "word": `q3rp` should
    // find "Q3 re*p*ort.pdf", not the p of ".pdf".
    let mut bounds = bounds;
    if let Some(dot) = orig.iter().rposition(|&c| c == '.').filter(|&d| d > 0) {
        bounds[dot + 1..].iter_mut().for_each(|b| *b = false);
    }
    fuzzy(&q, &n, &bounds).map(|(s, positions)| Match { score: FUZZY + s - len_penalty, positions })
}

/// Best subsequence alignment in O(|q|·|n|): matching a word start earns a
/// bonus, consecutive matches earn a bonus, and every jump costs a little.
fn fuzzy(q: &[char], n: &[char], bounds: &[bool]) -> Option<(i32, Vec<usize>)> {
    const NONE: i32 = i32::MIN / 2;
    const BASE: i32 = 10;
    const BOUNDARY: i32 = 30;
    const CONSECUTIVE: i32 = 25;
    const JUMP: i32 = 8;
    // Cheap rejection before the DP.
    let mut it = n.iter();
    if !q.iter().all(|c| it.any(|x| x == c)) {
        return None;
    }
    let (qn, nn) = (q.len(), n.len());
    // best[i][j]: q[..=i] matched with q[i] at n[j]; from[i][j]: where q[i-1] sat.
    let mut best = vec![NONE; qn * nn];
    let mut from = vec![usize::MAX; qn * nn];
    for j in 0..nn {
        if n[j] == q[0] {
            best[j] = BASE + if bounds[j] { BOUNDARY } else { 0 } - (j as i32).min(10);
        }
    }
    for i in 1..qn {
        // Running max of the previous row over k <= j-2 (a jump).
        let (mut run_max, mut run_arg) = (NONE, usize::MAX);
        for j in 1..nn {
            if j >= 2 && best[(i - 1) * nn + j - 2] > run_max {
                run_max = best[(i - 1) * nn + j - 2];
                run_arg = j - 2;
            }
            if n[j] != q[i] {
                continue;
            }
            let bonus = BASE + if bounds[j] { BOUNDARY } else { 0 };
            let consecutive = best[(i - 1) * nn + j - 1];
            let (prev, arg) = if consecutive > NONE && consecutive + CONSECUTIVE >= run_max - JUMP {
                (consecutive + CONSECUTIVE, j - 1)
            } else {
                (run_max - JUMP, run_arg)
            };
            if prev > NONE / 2 {
                best[i * nn + j] = prev + bonus;
                from[i * nn + j] = arg;
            }
        }
    }
    let last = (qn - 1) * nn;
    // Highest score; on ties the earliest end position.
    let (mut j, score) = (0..nn).map(|j| (j, best[last + j])).fold(None, |acc: Option<(usize, i32)>, (j, s)| match acc {
        Some((_, bs)) if bs >= s => acc,
        _ => Some((j, s)),
    })?;
    if score <= NONE / 2 {
        return None;
    }
    let mut positions = vec![0; qn];
    for i in (0..qn).rev() {
        positions[i] = j;
        if i > 0 {
            j = from[i * nn + j];
        }
    }
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn best<'a>(q: &str, names: &[&'a str]) -> Option<&'a str> {
        names.iter().filter_map(|n| score(q, n).map(|m| (m.score, *n))).max_by_key(|(s, _)| *s).map(|(_, n)| n)
    }

    #[test]
    fn tiers() {
        let names = ["Annual report.pdf", "report-final.docx", "reports", "my_reportcard", "the porter"];
        assert_eq!(best("rep", &names), Some("reports")); // prefix, shortest
        assert_eq!(best("final", &names), Some("report-final.docx")); // word start
        assert_eq!(best("eport", &names), Some("reports")); // substring
        assert_eq!(best("anrep", &names), Some("Annual report.pdf")); // fuzzy on word starts
        assert!(score("xyz", "report").is_none());
    }

    #[test]
    fn word_boundaries_and_case() {
        let m = score("vid", "MyHolidayVideo.mp4").unwrap();
        assert_eq!(m.positions, vec![9, 10, 11]); // camelCase start beats "...day V"
        assert!(score("2024", "taxes2024.xlsx").unwrap().score > score("2024", "x12024").unwrap().score);
        assert!(score("REP", "report").is_some());
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        let m = score("q3rp", "Q3 report.pdf").unwrap();
        assert_eq!(m.positions, vec![0, 1, 3, 5]);
        let a = score("fb", "foo_bar").unwrap().score;
        let b = score("fb", "fxxxxb").unwrap().score;
        assert!(a > b);
    }

    #[test]
    fn unicode_names() {
        let m = score("été", "Été 2024").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2]);
        assert!(score("ete", "Été").is_none()); // accents are significant, as in Explorer
    }
}
