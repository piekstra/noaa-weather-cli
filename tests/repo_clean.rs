//! This repository is public. Everything tracked in git stays free of personal
//! data and of credentials. Checks shapes, never a list of real values: such a
//! list would publish exactly what it exists to keep out.

use std::process::Command;

fn tracked_files() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files"])
        .output()
        .expect("git runs, or this guard checks nothing");
    assert!(out.status.success(), "git ls-files failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// `1234 Some Name Dr`-shaped text. Weather needs no street addresses at all.
fn street_addresses(text: &str) -> Vec<String> {
    const SUFFIXES: [&str; 14] = [
        "st", "ave", "rd", "dr", "ln", "ct", "blvd", "way", "ter", "cir", "pl", "trl", "hwy",
        "street",
    ];
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut found = Vec::new();
    for w in words.windows(4) {
        let number = w[0].len() >= 2 && w[0].chars().all(|c| c.is_ascii_digit());
        let name = w[1].chars().next().is_some_and(char::is_alphabetic);
        let suffix = |s: &str| {
            let t = s.trim_end_matches(|c: char| !c.is_alphanumeric());
            SUFFIXES.contains(&t.to_ascii_lowercase().as_str())
        };
        if number
            && name
            && (suffix(w[2]) || (suffix(w[3]) && w[2].chars().all(char::is_alphabetic)))
        {
            found.push(w.join(" "));
        }
    }
    found
}

fn phone_numbers(text: &str) -> usize {
    let digits_only: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
        .filter(|t| {
            let d: String = t.chars().filter(char::is_ascii_digit).collect();
            let parts: Vec<&str> = t.split('-').collect();
            d.len() == 10 && parts.len() == 3 && parts[0].len() == 3 && parts[1].len() == 3
        })
        .collect();
    digits_only.len()
}

#[test]
fn shape_scanner_catches_what_it_claims_to() {
    assert_eq!(street_addresses("lives at 1234 Example Dr today").len(), 1);
    assert_eq!(phone_numbers("call 561-555-0100 now"), 1);
    assert!(street_addresses("the 72 hour product").is_empty());
}

#[test]
fn tracked_files_carry_no_addresses_phones_or_emails() {
    let files = tracked_files();
    assert!(
        !files.is_empty(),
        "git tracks no files, so nothing is checked"
    );
    for f in files {
        if f.ends_with(".lock") || f.starts_with("LICENSE") || f == "tests/repo_clean.rs" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let addrs = street_addresses(&text);
        assert!(addrs.is_empty(), "{f}: address-shaped text: {addrs:?}");
        assert_eq!(phone_numbers(&text), 0, "{f}: phone-shaped text");
        let emails = text
            .split_whitespace()
            .filter(|w| {
                w.contains('@')
                    && w.contains(".com")
                    && !w.contains("users.noreply")
                    && !w.contains("example")
            })
            .count();
        assert_eq!(emails, 0, "{f}: email-shaped text");
    }
}
