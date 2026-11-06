//! Rule-based ingredient parser. Pure: no database, no I/O.
use serde::Serialize;
use utoipa::ToSchema;

use crate::units::KNOWN_UNITS;
use crate::{Food, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Food found, and an amount (or "to taste").
    Full,
    /// Something was recognised, but not enough to trust.
    Partial,
    /// Nothing recognised; only `original_text` is meaningful.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Parsed {
    pub qty_min: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub food_id: Option<i64>,
    pub note: String,
    pub original_text: String,
    pub confidence: Confidence,
}

const UNICODE_FRACTIONS: &[(char, f64)] =
    &[('½', 0.5), ('¼', 0.25), ('¾', 0.75), ('⅓', 1.0 / 3.0), ('⅔', 2.0 / 3.0), ('⅛', 0.125)];

/// Splits leading ascii digits off `s`.
fn digits(s: &str) -> (&str, &str) {
    s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()))
}

fn unicode_fraction(s: &str) -> Option<(f64, &str)> {
    let c = s.chars().next()?;
    let v = UNICODE_FRACTIONS.iter().find(|(f, _)| *f == c)?.1;
    Some((v, &s[c.len_utf8()..]))
}

/// "1/2" -> 0.5.
fn ascii_fraction(s: &str) -> Option<(f64, &str)> {
    let (n, rest) = digits(s);
    let (d, rest) = digits(rest.strip_prefix('/')?);
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0.0).then_some((n / d, rest))
}

/// One number at the start of `s`: 2, 1.5, ½, 1/2, 1½, 1 1/2. Returns the value and the rest.
fn number(s: &str) -> Option<(f64, &str)> {
    if let Some(r) = unicode_fraction(s) {
        return Some(r);
    }
    if let Some(r) = ascii_fraction(s) {
        return Some(r);
    }
    let (int, rest) = digits(s);
    if int.is_empty() {
        return None;
    }
    // Decimal part.
    let (text, rest) = match rest.strip_prefix('.').map(digits) {
        Some((frac, after)) if !frac.is_empty() => (&s[..int.len() + 1 + frac.len()], after),
        _ => (int, rest),
    };
    let whole: f64 = text.parse().ok()?;
    // Mixed number: "1 1/2", "1½", "2 ½".
    let after_space = rest.trim_start_matches(' ');
    if let Some((frac, r)) = unicode_fraction(after_space).or_else(|| ascii_fraction(after_space)) {
        return Some((whole + frac, r));
    }
    Some((whole, rest))
}

/// A quantity or range at the start of `s`.
fn quantity(s: &str) -> Option<(f64, f64, &str)> {
    let (min, rest) = number(s)?;
    let after = rest.trim_start();
    let tail = if let Some(t) = after.strip_prefix(['-', '–', '—']) {
        Some(('-', t))
    } else if after.len() > 3 && after[..2].eq_ignore_ascii_case("to") && after[2..].starts_with(' ') {
        Some(('t', &after[2..]))
    } else {
        None
    };
    if let Some((sep, t)) = tail
        && let Some((max, r)) = number(t.trim_start())
    {
        // "1-1/2" is one and a half, not a range.
        if sep == '-' && min.fract() == 0.0 && max < 1.0 && t.contains('/') {
            return Some((min + max, min + max, r));
        }
        return Some((min, max.max(min), r));
    }
    Some((min, min, rest))
}

/// Unit spellings to try, longest first: each unit's name and aliases, plus the
/// shared abbreviation group they belong to. Single letters (T, t, c, l) are too
/// ambiguous; "g" is the one exception.
fn unit_candidates(units: &[Unit]) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for u in units {
        let own: Vec<String> = std::iter::once(&u.name).chain(&u.aliases).map(|n| n.trim().to_lowercase()).collect();
        for n in &own {
            out.push((n.clone(), u.id));
        }
        if let Some((group, ..)) = KNOWN_UNITS.iter().find(|(g, ..)| own.iter().any(|n| g.contains(&n.as_str()))) {
            out.extend(group.iter().map(|g| (g.to_string(), u.id)));
        }
    }
    out.retain(|(n, _)| n.chars().count() > 1 || n == "g");
    out.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// A unit at the start of `s` (and an optional "of" after it). Returns its id and the rest.
fn unit<'a>(s: &'a str, units: &[Unit]) -> Option<(i64, &'a str)> {
    for (name, id) in unit_candidates(units) {
        let Some(head) = s.get(..name.len()) else { continue };
        let rest = &s[name.len()..];
        if head.eq_ignore_ascii_case(&name) && !rest.starts_with(is_word_char) {
            let rest = rest.strip_prefix('.').unwrap_or(rest).trim_start();
            let rest = match rest.get(..3) {
                Some(of) if of.eq_ignore_ascii_case("of ") => rest[3..].trim_start(),
                _ => rest,
            };
            return Some((id, rest));
        }
    }
    None
}

/// Plural and singular guesses for the last word of a food name.
fn inflections(name: &str) -> Vec<String> {
    let mut v = vec![name.to_string(), format!("{name}s"), format!("{name}es")];
    if let Some(stem) = name.strip_suffix('y') {
        v.push(format!("{stem}ies"));
    }
    if let Some(stem) = name.strip_suffix("ies") {
        v.push(format!("{stem}y"));
    }
    for suffix in ["es", "s"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            v.push(stem.to_string());
        }
    }
    v
}

/// The longest food name or alias (in any inflection) found on word boundaries in `s`.
/// Returns the food id and the byte range it covers.
fn food(s: &str, foods: &[Food]) -> Option<(i64, std::ops::Range<usize>)> {
    let hay = s.to_ascii_lowercase();
    let mut best: Option<(i64, std::ops::Range<usize>)> = None;
    for f in foods {
        for name in std::iter::once(&f.name).chain(&f.aliases) {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            for cand in inflections(&name) {
                for (at, _) in hay.match_indices(&cand) {
                    let end = at + cand.len();
                    let bounded = !hay[..at].ends_with(is_word_char) && !hay[end..].starts_with(is_word_char);
                    if bounded && best.as_ref().is_none_or(|(_, r)| cand.len() > r.len()) {
                        best = Some((f.id, at..end));
                    }
                }
            }
        }
    }
    best
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rule-based parse of one ingredient line. Never fails: a line the rules
/// cannot read comes back with `Confidence::None` and its `original_text`.
pub fn parse_line(text: &str, units: &[Unit], foods: &[Food]) -> Parsed {
    // step 9: LLM fallback for unparsed lines
    let mut notes: Vec<String> = Vec::new();
    let mut body = String::new();

    // "(optional)" groups become notes, in order.
    let mut depth = 0;
    let mut group = String::new();
    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                if depth > 1 {
                    group.push(c);
                }
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth > 0 {
                    group.push(c);
                } else {
                    notes.push(squash(&std::mem::take(&mut group)));
                }
            }
            _ if depth > 0 => group.push(c),
            _ => body.push(c),
        }
    }
    let mut body = squash(&body);

    // ", chopped": everything after the first comma.
    if let Some((head, tail)) = body.split_once(',') {
        notes.push(squash(tail));
        body = head.trim().to_string();
    }
    // "salt to taste"
    if body.len() >= 8 && body.get(body.len() - 8..).is_some_and(|t| t.eq_ignore_ascii_case("to taste")) {
        notes.push("to taste".into());
        body.truncate(body.len() - 8);
        body = body.trim().to_string();
    }

    let mut rest = body.as_str();
    let (mut qty_min, mut qty_max, mut unit_id) = (None, None, None);
    if let Some((min, max, r)) = quantity(rest) {
        (qty_min, qty_max) = (Some(min), Some(max));
        rest = r.trim_start();
        if let Some((id, r)) = unit(rest, units) {
            unit_id = Some(id);
            rest = r;
        }
    }

    let mut food_id = None;
    let mut leftover = String::new();
    if let Some((id, range)) = food(rest, foods) {
        food_id = Some(id);
        leftover = squash(&format!("{} {}", &rest[..range.start], &rest[range.end..]));
    } else if qty_min.is_some() {
        // Unknown food: keep its words so nothing is lost.
        leftover = rest.to_string();
    }
    if !leftover.is_empty() {
        notes.insert(0, leftover);
    }
    notes.retain(|n| !n.is_empty());

    let to_taste = notes.iter().any(|n| n == "to taste");
    let confidence = match (food_id, qty_min.is_some() || to_taste) {
        (Some(_), true) => Confidence::Full,
        (None, false) if unit_id.is_none() => Confidence::None,
        _ => Confidence::Partial,
    };
    let note = if confidence == Confidence::None { String::new() } else { notes.join(", ") };
    Parsed { qty_min, qty_max, unit_id, food_id, note, original_text: text.to_string(), confidence }
}
