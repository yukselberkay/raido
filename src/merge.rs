//! A three-way merge of whole lines, for a file that an AI (or anything else)
//! changes on disk while you have unsaved edits in it: their changes are carried
//! into your text where they don't meet yours, as `git merge` would, and yours are
//! kept where they do.
use similar::{capture_diff_slices_deadline, Algorithm, DiffTag};
use std::{
    ops::Range,
    time::{Duration, Instant},
};

/// A long diff gives up on being minimal after this; the merge stays correct, only
/// more cautious.
const DEADLINE: Duration = Duration::from_millis(300);

/// What of `theirs` a merge brings into `ours`.
#[derive(Debug, Default, PartialEq)]
pub struct Merge {
    /// `(start, end, text)` char ranges of `ours` to replace.
    pub edits: Vec<(usize, usize, String)>,
    /// Where a change of theirs met one of yours and yours was kept: the first line
    /// of each (from 0), in the merged text.
    pub kept: Vec<usize>,
}

/// Bring into `ours` what changed from `base` to `theirs`. A change of theirs that
/// overlaps or meets one of ours, lines touching end to start included, is left
/// out and yours kept there (unless both made the same change): either side alone
/// would lose something there, and only you can choose. The rest comes in, as
/// `git merge` would bring it.
pub fn merge(base: &str, ours: &str, theirs: &str) -> Merge {
    let lines = |text| -> Vec<&str> { str::split_inclusive(text, '\n').collect() };
    let (base, mine, other) = (lines(base), lines(ours), lines(theirs));
    let ours_hunks = hunks(&base, &mine);
    let their_hunks = hunks(&base, &other);
    let meets = |a: &Range<usize>, b: &Range<usize>| a.start <= b.end && b.start <= a.end;
    // Where each of our lines starts, in chars, and where the text ends.
    let mut starts = Vec::with_capacity(mine.len() + 1);
    let mut at = 0;
    for line in &mine {
        starts.push(at);
        at += line.chars().count();
    }
    starts.push(at);
    let mut merged = Merge::default();
    // Lines of ours the edits replace, and how many lines take their place.
    let mut moves: Vec<(Range<usize>, usize)> = Vec::new();
    let mut kept = Vec::new();
    for (changed, with) in their_hunks {
        let met: Vec<_> = ours_hunks
            .iter()
            .filter(|(o, _)| meets(&changed, o))
            .collect();
        if let [(o, new)] = met[..] {
            if *o == changed && mine[new.clone()] == other[with.clone()] {
                continue; // The same change on both sides: there already.
            }
        }
        if let Some((_, new)) = met.first() {
            kept.push(new.start);
            continue;
        }
        // Lines we added or removed above this change move it.
        let shift: isize = ours_hunks
            .iter()
            .filter(|(o, _)| o.end <= changed.start)
            .map(|(o, new)| new.len() as isize - o.len() as isize)
            .sum();
        let first = changed.start.saturating_add_signed(shift);
        moves.push((first..first + changed.len(), with.len()));
        merged.edits.push((
            starts[first],
            starts[first + changed.len()],
            other[with].concat(),
        ));
    }
    // Our lines where theirs were left out, moved by the edits above them.
    merged.kept = kept
        .into_iter()
        .map(|line| {
            let shift: isize = moves
                .iter()
                .filter(|(gone, _)| gone.end <= line)
                .map(|(gone, added)| *added as isize - gone.len() as isize)
                .sum();
            line.saturating_add_signed(shift)
        })
        .collect();
    merged.kept.dedup();
    merged
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

    /// The merged text, when nothing of yours had to be kept over theirs.
    fn merged(base: &str, ours: &str, theirs: &str) -> Option<String> {
        let (text, kept) = merged_keeping(base, ours, theirs);
        kept.is_empty().then_some(text)
    }

    /// The merged text and the lines where yours were kept.
    fn merged_keeping(base: &str, ours: &str, theirs: &str) -> (String, Vec<usize>) {
        let mut text: Vec<char> = ours.chars().collect();
        let Merge { mut edits, kept } = merge(base, ours, theirs);
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (start, end, with) in edits {
            text.splice(start..end, with.chars());
        }
        (text.into_iter().collect(), kept)
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
    fn changes_that_meet_yours_keep_yours_and_the_rest_still_comes_in() {
        // The same line: yours stays, said to be on line 2.
        let ours = BASE.replace("return 1", "return 10");
        let theirs = BASE.replace("return 1", "return 11");
        assert_eq!(
            merged_keeping(BASE, &ours, &theirs),
            (ours.clone(), vec![1])
        );
        // The next line: touching counts, as in git.
        let theirs = BASE.replace("def a():", "def alpha():");
        assert_eq!(
            merged_keeping(BASE, &ours, &theirs),
            (ours.clone(), vec![1])
        );
        // Both added at the very end.
        let (ours_end, theirs_end) = (format!("{BASE}x\n"), format!("{BASE}y\n"));
        assert_eq!(
            merged_keeping(BASE, &ours_end, &theirs_end),
            (ours_end.clone(), vec![6])
        );
        // They also changed b() and added lines above yours: those come in, and
        // your kept line is counted where it is now.
        let theirs = format!(
            "# header\n{}",
            BASE.replace("return 1", "return 11")
                .replace("return 2", "return 20")
        );
        assert_eq!(
            merged_keeping(BASE, &ours, &theirs),
            (
                format!("# header\n{}", ours.replace("return 2", "return 20")),
                vec![2]
            )
        );
        // The same change on both sides isn't a clash: it's there already.
        let theirs = ours.replace("return 2", "return 20");
        assert_eq!(
            merged(BASE, &ours, &theirs).unwrap(),
            ours.replace("return 2", "return 20")
        );
        // Nothing of theirs to bring: no edits.
        assert_eq!(merge(BASE, &ours, BASE), Merge::default());
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
