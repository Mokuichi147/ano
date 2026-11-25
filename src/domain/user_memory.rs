//! Memory: short facts about the user that conversations saved, so later
//! runs can tailor answers and suggestions without being told again.
//!
//! Each fact is one line. Facts are data about the user, never instructions
//! to the agent. The most recent facts are sent with every request, so the
//! amount sent is bounded.

use anyhow::{bail, Result};

/// Tool that saves a fact about the user. It always requires approval,
/// because every later run reads the fact.
pub const MEMORY_SAVE_NAME: &str = "memory_save";
/// Tool that removes facts the user wants forgotten or that became wrong.
pub const MEMORY_FORGET_NAME: &str = "memory_forget";

pub const MAX_FACT_CHARS: usize = 300;
/// Facts one user may keep. Saving beyond it fails, so the user (or the
/// model) consolidates instead of the file growing without bound.
pub const MAX_FACTS: usize = 300;
/// Upper bound on the facts sent with one run. The most recent facts that
/// fit are sent; older ones are left out.
pub const MAX_MEMORY_INDEX_BYTES: usize = 4 * 1024;

/// A fact: one line within the size limit. Surrounding whitespace is
/// trimmed and inner runs of whitespace become one space.
pub fn normalize_fact(text: &str) -> Result<String> {
    let fact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if fact.is_empty() {
        bail!("a fact must not be empty");
    }
    if fact.chars().count() > MAX_FACT_CHARS {
        bail!("a fact must be at most {MAX_FACT_CHARS} characters; save one short fact at a time");
    }
    if fact.chars().any(char::is_control) {
        bail!("a fact must not contain control characters");
    }
    Ok(fact)
}

/// The facts to send with a run: the most recent ones (the end of `facts`)
/// that fit within `MAX_MEMORY_INDEX_BYTES`, in their saved order, and how
/// many older facts were left out.
pub fn recent_facts(facts: &[String]) -> (&[String], usize) {
    let mut bytes = 0;
    let mut start = facts.len();
    while start > 0 {
        let next = facts[start - 1].len() + 3;
        if bytes + next > MAX_MEMORY_INDEX_BYTES {
            break;
        }
        bytes += next;
        start -= 1;
    }
    (&facts[start..], start)
}

/// Facts one merge may combine. A merge of more would turn many facts into
/// one sentence that cannot hold them.
pub const MAX_MERGE_MEMBERS: usize = 6;

/// One step of a consolidation: the facts at `members` (positions in the
/// saved order) become the single fact `into`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    pub members: Vec<usize>,
    pub into: String,
}

/// What a consolidation does to the facts it was given.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Consolidation {
    /// For each fact, in the saved order: its text afterwards, or `None`
    /// when it went into a merged fact.
    pub facts: Vec<Option<String>>,
    /// The merges that were applied, with their fact normalized.
    pub applied: Vec<Merge>,
    /// The merges that were left out, each with the reason.
    pub skipped: Vec<(Merge, String)>,
}

impl Consolidation {
    /// The facts afterwards, in order.
    pub fn remaining(&self) -> Vec<String> {
        self.facts.iter().flatten().cloned().collect()
    }
}

/// How many facts one consolidation may take away from `count`: a quarter,
/// and at least two. Merging is proposed by a model, so one pass cannot
/// rewrite most of the memory; a later pass can go on.
pub fn merge_allowance(count: usize) -> usize {
    count.div_ceil(4).max(2)
}

/// Apply `merges` to `facts`, leaving out every merge that is not sound.
///
/// A merge needs two to `MAX_MERGE_MEMBERS` different facts that no earlier
/// merge used, and a text that is a valid fact. The merged fact takes the
/// place of its most recent member, because the most recent facts are the
/// ones sent with a run. Facts no merge names stay exactly as they are, and
/// merges stop once `merge_allowance` facts are gone.
pub fn consolidate(facts: &[String], merges: Vec<Merge>) -> Consolidation {
    let mut result = Consolidation {
        facts: facts.iter().cloned().map(Some).collect(),
        ..Consolidation::default()
    };
    let mut used = vec![false; facts.len()];
    let mut allowance = merge_allowance(facts.len());
    for mut merge in merges {
        merge.members.sort_unstable();
        merge.members.dedup();
        let reason = if merge.members.len() < 2 {
            Some("it names fewer than two different facts".to_string())
        } else if merge.members.len() > MAX_MERGE_MEMBERS {
            Some(format!("it merges more than {MAX_MERGE_MEMBERS} facts"))
        } else if merge.members.iter().any(|&member| member >= facts.len()) {
            Some("it names a fact that does not exist".to_string())
        } else if merge.members.iter().any(|&member| used[member]) {
            Some("an earlier merge already used one of its facts".to_string())
        } else if merge.members.len() - 1 > allowance {
            Some("one consolidation removes only a part of the memory; run it again".to_string())
        } else {
            match normalize_fact(&merge.into) {
                Ok(fact) => {
                    merge.into = fact;
                    None
                }
                Err(error) => Some(format!("its fact is not valid: {error}")),
            }
        };
        if let Some(reason) = reason {
            result.skipped.push((merge, reason));
            continue;
        }
        let (last, earlier) = merge.members.split_last().expect("at least two members");
        for &member in earlier {
            result.facts[member] = None;
            used[member] = true;
        }
        result.facts[*last] = Some(merge.into.clone());
        used[*last] = true;
        allowance -= earlier.len();
        result.applied.push(merge);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(count: usize) -> Vec<String> {
        (0..count).map(|n| format!("fact {n}")).collect()
    }

    fn merge(members: &[usize], into: &str) -> Merge {
        Merge {
            members: members.to_vec(),
            into: into.into(),
        }
    }

    #[test]
    fn a_merged_fact_takes_the_place_of_its_most_recent_member() {
        let result = consolidate(&facts(6), vec![merge(&[4, 1, 4], " both  1 and 4 ")]);
        assert_eq!(
            result.remaining(),
            ["fact 0", "fact 2", "fact 3", "both 1 and 4", "fact 5"]
        );
        assert_eq!(result.facts[1], None);
        assert_eq!(result.applied, [merge(&[1, 4], "both 1 and 4")]);
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn unsound_merges_are_left_out_and_the_others_still_apply() {
        let long = "x".repeat(MAX_FACT_CHARS + 1);
        let result = consolidate(
            &facts(12),
            vec![
                merge(&[0, 1], "zero and one"),
                merge(&[2, 2], "only one fact"),
                merge(&[1, 3], "reuses a merged fact"),
                merge(&[4, 99], "names a missing fact"),
                merge(&[5, 6], &long),
                merge(&[5, 6], " \n "),
                merge(&[2, 3, 4, 5, 6, 7, 8], "too many"),
                merge(&[10, 11], "ten and eleven"),
            ],
        );
        assert_eq!(
            result.applied,
            [
                merge(&[0, 1], "zero and one"),
                merge(&[10, 11], "ten and eleven")
            ]
        );
        assert_eq!(result.skipped.len(), 6);
        assert_eq!(result.remaining().len(), 10);
        assert_eq!(result.facts[2].as_deref(), Some("fact 2"));
    }

    #[test]
    fn one_consolidation_removes_only_a_part_of_the_memory() {
        assert_eq!(
            (merge_allowance(0), merge_allowance(3), merge_allowance(8)),
            (2, 2, 2)
        );
        assert_eq!((merge_allowance(9), merge_allowance(300)), (3, 75));
        // Twelve facts allow three to go: two merges of two, and then a
        // merge of three is too much, but another merge of two still fits.
        let result = consolidate(
            &facts(12),
            vec![
                merge(&[0, 1], "a"),
                merge(&[2, 3], "b"),
                merge(&[4, 5, 6], "c"),
                merge(&[7, 8], "d"),
                merge(&[9, 10], "e"),
            ],
        );
        assert_eq!(result.applied.len(), 3);
        assert_eq!(result.remaining().len(), 9);
        assert_eq!(result.skipped.len(), 2);
    }

    #[test]
    fn facts_are_one_bounded_line() {
        assert_eq!(
            normalize_fact("  Likes\n  green tea \t").unwrap(),
            "Likes green tea"
        );
        assert!(normalize_fact(" \n ").is_err());
        assert!(normalize_fact(&"x".repeat(MAX_FACT_CHARS + 1)).is_err());
        assert!(normalize_fact("a\u{7}b").is_err());
        normalize_fact(&"あ".repeat(MAX_FACT_CHARS)).unwrap();
    }

    #[test]
    fn the_most_recent_facts_that_fit_are_sent() {
        let facts: Vec<String> = (0..10).map(|n| format!("fact {n}")).collect();
        let (sent, omitted) = recent_facts(&facts);
        assert_eq!((sent.len(), omitted), (10, 0));

        let long: Vec<String> = (0..40)
            .map(|n| format!("{n:03}{}", "x".repeat(200)))
            .collect();
        let (sent, omitted) = recent_facts(&long);
        assert!(omitted > 0);
        assert_eq!(sent.len() + omitted, long.len());
        assert_eq!(sent.last(), long.last());
        assert!(sent.iter().map(|fact| fact.len() + 3).sum::<usize>() <= MAX_MEMORY_INDEX_BYTES);
    }
}
