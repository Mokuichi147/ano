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

#[cfg(test)]
mod tests {
    use super::*;

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
