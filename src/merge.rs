//! A three-way merge of whole lines, for a file that an AI (or anything else)
//! changes on disk while you have unsaved edits in it: their changes are carried
//! into your text when they don't meet yours, as `git merge` would.
use similar::{capture_diff_slices_deadline, Algorithm, DiffTag};
use std::{
    ops::Range,
    time::{Duration, Instant},
};

/// A long diff gives up on being minimal after this; the merge stays correct, only
/// more cautious.
const DEADLINE: Duration = Duration::from_millis(300);

/// The edits that bring into `ours` what changed from `base` to `theirs`, as
/// `(start, end, text)` char ranges of `ours`. `None` when a change of theirs
/// overlaps or meets one of ours, lines touching end to start included: then
/// either side alone would lose something, and only you can choose.
pub fn merge(base: &str, ours: &str, theirs: &str) -> Option<Vec<(usize, usize, String)>> {
    let lines = |text| -> Vec<&str> { str::split_inclusive(text, '\n').collect() };
    let (base, mine, other) = (lines(base), lines(ours), lines(theirs));
    let ours_hunks = hunks(&base, &mine);
    let their_hunks = hunks(&base, &other);
    let meets = |a: &Range<usize>, b: &Range<usize>| a.start <= b.end && b.start <= a.end;
    if their_hunks
        .iter()
        .any(|(t, _)| ours_hunks.iter().any(|(o, _)| meets(t, o)))
    {
        return None;
    }
    // Where each of our lines starts, in chars, and where the text ends.
    let mut starts = Vec::with_capacity(mine.len() + 1);
    let mut at = 0;
    for line in &mine {
        starts.push(at);
        at += line.chars().count();
    }
    starts.push(at);
    let edits = their_hunks
        .into_iter()
        .map(|(changed, with)| {
            // Lines we added or removed above this change move it.
            let shift: isize = ours_hunks
                .iter()
                .filter(|(o, _)| o.end <= changed.start)
                .map(|(o, new)| new.len() as isize - o.len() as isize)
                .sum();
            let first = changed.start.saturating_add_signed(shift);
            (
                starts[first],
                starts[first + changed.len()],
                other[with].concat(),
            )
        })
        .collect();
    Some(edits)
}

/// The changed stretches from `old` to `new`: lines `old[a]` became `new[b]`.
fn hunks(old: &[&str], new: &[&str]) -> Vec<(Range<usize>, Range<usize>)> {
    let deadline = Some(Instant::now() + DEADLINE);
    let mut out: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for op in capture_diff_slices_deadline(Algorithm::Myers, old, new, deadline) {
        let (tag, a, b) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        match out.last_mut() {
            Some((pa, pb)) if pa.end == a.start && pb.end == b.start => {
                pa.end = a.end;
                pb.end = b.end;
            }
            _ => out.push((a, b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(base: &str, ours: &str, theirs: &str) -> Option<String> {
        let mut text: Vec<char> = ours.chars().collect();
        let mut edits = merge(base, ours, theirs)?;
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (start, end, with) in edits {
            text.splice(start..end, with.chars());
        }
        Some(text.into_iter().collect())
    }

    const BASE: &str = "def a():\n    return 1\n\n\ndef b():\n    return 2\n";

    #[test]
    fn their_changes_away_from_yours_are_carried_into_your_text() {
        // You renamed a(); the AI changed b() and added c() at the end.
        let ours = BASE.replace("def a", "def first");
        let theirs = BASE.replace("return 2", "return 20") + "\n\ndef c():\n    return 3\n";
        assert_eq!(
            merged(BASE, &ours, &theirs).unwrap(),
            "def first():\n    return 1\n\n\ndef b():\n    return 20\n\n\ndef c():\n    return 3\n"
        );
        // Lines you added above their change move it down; non-ASCII counts as chars.
        let ours = format!("# café ☕\n# notes\n{BASE}");
        let theirs = BASE.replace("return 2", "return 22");
        assert_eq!(
            merged(BASE, &ours, &theirs).unwrap(),
            format!(
                "# café ☕\n# notes\n{}",
                BASE.replace("return 2", "return 22")
            )
        );
        // They removed lines; you edited far from them.
        let ours = BASE.replace("return 2", "return 2  # mine");
        let theirs = BASE.replace("def a():\n    return 1\n\n\n", "");
        assert_eq!(
            merged(BASE, &ours, &theirs).unwrap(),
            "def b():\n    return 2  # mine\n"
        );
    }

    #[test]
    fn changes_that_meet_yours_are_left_for_you() {
        // The same line.
        let ours = BASE.replace("return 1", "return 10");
        let theirs = BASE.replace("return 1", "return 11");
        assert_eq!(merged(BASE, &ours, &theirs), None);
        // The next line: touching counts, as in git.
        let theirs = BASE.replace("def a():", "def alpha():");
        assert_eq!(merged(BASE, &ours, &theirs), None);
        // Both added at the very end.
        assert_eq!(
            merged(BASE, &format!("{BASE}x\n"), &format!("{BASE}y\n")),
            None
        );
        // Nothing of theirs to bring: no edits.
        assert_eq!(merge(BASE, &ours, BASE), Some(Vec::new()));
    }

    #[test]
    fn a_last_line_without_a_newline_and_windows_endings_merge_too() {
        let base = "one\r\ntwo\r\nthree";
        let ours = "ONE\r\ntwo\r\nthree";
        let theirs = "one\r\ntwo\r\nthree\r\nfour";
        assert_eq!(
            merged(base, ours, theirs).unwrap(),
            "ONE\r\ntwo\r\nthree\r\nfour"
        );
    }
}
