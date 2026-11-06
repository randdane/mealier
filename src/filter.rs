//! Mealie's `query_filter_string` (cookbooks and meal-plan rules): parsed into an AST, then run as a
//! parameterised WHERE over a group's recipes. Only the subset that maps to mealier's fields is
//! understood; anything else is an error naming the token, never a guess.
use serde::{Deserialize, Serialize};

use sqlx::Row;

use crate::{Db, RecipeSummary, Result, Scope};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError(pub String);

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "filter not supported: {}", self.0)
    }
}

impl std::error::Error for FilterError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetField {
    Tags,
    Categories,
    Tools,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum By {
    Id,
    Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetOp {
    In,
    NotIn,
    ContainsAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextField {
    Name,
    Description,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextOp {
    Like,
    NotLike,
    Eq,
    Ne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Field {
    LastMade,
    DateAdded,
    DateUpdated,
    Rating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Lit {
    Num(f64),
    /// A `YYYY-MM-DD` date.
    Date(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    /// `tags.id IN [...]`, `recipe_category.name CONTAINS ALL [...]`. Values are ids (as text) or names.
    Set { field: SetField, by: By, op: SetOp, values: Vec<String> },
    Text { field: TextField, op: TextOp, value: String },
    Cmp { field: Field, op: CmpOp, value: Lit },
    Null { field: Field, negated: bool },
}

const MAX_LEN: usize = 8192;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    LBrack,
    RBrack,
    Comma,
    Str(String),
    Num(f64),
    Word(String),
    Op(&'static str),
}

impl std::fmt::Display for Tok {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tok::LParen => f.write_str("("),
            Tok::RParen => f.write_str(")"),
            Tok::LBrack => f.write_str("["),
            Tok::RBrack => f.write_str("]"),
            Tok::Comma => f.write_str(","),
            Tok::Str(s) => write!(f, "\"{s}\""),
            Tok::Num(n) => write!(f, "{n}"),
            Tok::Word(w) => f.write_str(w),
            Tok::Op(o) => f.write_str(o),
        }
    }
}

fn fail<T>(msg: impl Into<String>) -> std::result::Result<T, FilterError> {
    Err(FilterError(msg.into()))
}

fn lex(src: &str) -> std::result::Result<Vec<Tok>, FilterError> {
    let cs: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => (out.push(Tok::LParen), i += 1).1,
            ')' => (out.push(Tok::RParen), i += 1).1,
            '[' => (out.push(Tok::LBrack), i += 1).1,
            ']' => (out.push(Tok::RBrack), i += 1).1,
            ',' => (out.push(Tok::Comma), i += 1).1,
            '"' | '\'' => {
                let mut v = String::new();
                i += 1;
                loop {
                    match cs.get(i) {
                        None => return fail("unterminated string"),
                        Some('\\') => match cs.get(i + 1) {
                            Some(&n) => (v.push(n), i += 2).1,
                            None => return fail("unterminated string"),
                        },
                        Some(&q) if q == c => break,
                        Some(&ch) => (v.push(ch), i += 1).1,
                    }
                }
                i += 1;
                out.push(Tok::Str(v));
            }
            '=' => (out.push(Tok::Op("=")), i += 1).1,
            '!' if cs.get(i + 1) == Some(&'=') => (out.push(Tok::Op("!=")), i += 2).1,
            '<' | '>' => {
                let next = cs.get(i + 1).copied();
                let (op, n) = match (c, next) {
                    ('<', Some('=')) => ("<=", 2),
                    ('<', Some('>')) => ("<>", 2),
                    ('>', Some('=')) => (">=", 2),
                    ('<', _) => ("<", 1),
                    _ => (">", 1),
                };
                out.push(Tok::Op(op));
                i += n;
            }
            c if c.is_ascii_digit() || (c == '-' && cs.get(i + 1).is_some_and(char::is_ascii_digit)) => {
                let start = i;
                i += 1;
                while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                    i += 1;
                }
                let t: String = cs[start..i].iter().collect();
                match t.parse() {
                    Ok(n) => out.push(Tok::Num(n)),
                    Err(_) => return fail(format!("bad number {t}")),
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_' || cs[i] == '.') {
                    i += 1;
                }
                out.push(Tok::Word(cs[start..i].iter().collect()));
            }
            c => return fail(format!("unexpected character {c:?}")),
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    depth: usize,
}

/// What a field name resolves to.
enum Target {
    Set(SetField, By),
    Text(TextField),
    Val(Field),
}

fn target(name: &str) -> std::result::Result<Target, FilterError> {
    let norm = name.to_lowercase().replace('_', "");
    let (head, tail) = match norm.split_once('.') {
        Some((h, t)) => (h, Some(t)),
        None => (norm.as_str(), None),
    };
    let set = match head {
        "tags" => Some(SetField::Tags),
        "recipecategory" | "recipecategories" | "categories" | "category" => Some(SetField::Categories),
        "tools" | "tool" => Some(SetField::Tools),
        _ => None,
    };
    if let Some(field) = set {
        return match tail {
            Some("id") => Ok(Target::Set(field, By::Id)),
            Some("name") => Ok(Target::Set(field, By::Name)),
            _ => fail(format!("{name} needs .id or .name")),
        };
    }
    if tail.is_some() {
        return fail(format!("unsupported field {name}"));
    }
    Ok(match head {
        "name" => Target::Text(TextField::Name),
        "description" => Target::Text(TextField::Description),
        "lastmade" => Target::Val(Field::LastMade),
        "dateadded" | "createdat" => Target::Val(Field::DateAdded),
        "dateupdated" | "updatedat" => Target::Val(Field::DateUpdated),
        "rating" => Target::Val(Field::Rating),
        _ => return fail(format!("unsupported field {name}")),
    })
}

/// `2024-01-31`, or a datetime starting with one: the date part.
fn date_of(s: &str) -> Option<String> {
    let d = s.get(..10)?;
    let b = d.as_bytes();
    let shaped = b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    let (m, day) = (d[5..7].parse::<u32>().ok()?, d[8..10].parse::<u32>().ok()?);
    let rest_ok = s.len() == 10 || s[10..].starts_with(['T', ' ']);
    (shaped && rest_ok && (1..=12).contains(&m) && (1..=31).contains(&day)).then(|| d.to_string())
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn describe(t: Option<Tok>) -> String {
        t.map_or_else(|| "end of the filter".into(), |t| t.to_string())
    }

    fn unexpected<T>(&self, t: Option<Tok>, wanted: &str) -> std::result::Result<T, FilterError> {
        fail(format!("unexpected {}, expected {wanted}", Self::describe(t)))
    }

    fn keyword(&self) -> Option<String> {
        match self.peek() {
            Some(Tok::Word(w)) => Some(w.to_uppercase()),
            _ => None,
        }
    }

    fn expr(&mut self) -> std::result::Result<Filter, FilterError> {
        let mut parts = vec![self.and()?];
        while self.keyword().as_deref() == Some("OR") {
            self.next();
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 { parts.remove(0) } else { Filter::Or(parts) })
    }

    fn and(&mut self) -> std::result::Result<Filter, FilterError> {
        let mut parts = vec![self.atom()?];
        while self.keyword().as_deref() == Some("AND") {
            self.next();
            parts.push(self.atom()?);
        }
        Ok(if parts.len() == 1 { parts.remove(0) } else { Filter::And(parts) })
    }

    fn atom(&mut self) -> std::result::Result<Filter, FilterError> {
        if self.peek() == Some(&Tok::LParen) {
            self.next();
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return fail("parentheses are nested too deeply");
            }
            let inner = self.expr()?;
            self.depth -= 1;
            return match self.next() {
                Some(Tok::RParen) => Ok(inner),
                t => self.unexpected(t, ")"),
            };
        }
        self.cond()
    }

    fn cond(&mut self) -> std::result::Result<Filter, FilterError> {
        let name = match self.next() {
            Some(Tok::Word(w)) if !matches!(w.to_uppercase().as_str(), "AND" | "OR" | "NOT" | "IN" | "IS" | "LIKE") => w,
            t => return self.unexpected(t, "a field name"),
        };
        let target = target(&name)?;
        let op = match self.next() {
            Some(Tok::Op(o)) => o.to_string(),
            Some(Tok::Word(w)) => match w.to_uppercase().as_str() {
                "NOT" => match self.next() {
                    Some(Tok::Word(w2)) if w2.eq_ignore_ascii_case("IN") => "NOT IN".into(),
                    Some(Tok::Word(w2)) if w2.eq_ignore_ascii_case("LIKE") => "NOT LIKE".into(),
                    t => return self.unexpected(t, "IN or LIKE"),
                },
                "CONTAINS" => match self.next() {
                    Some(Tok::Word(w2)) if w2.eq_ignore_ascii_case("ALL") => "CONTAINS ALL".into(),
                    t => return self.unexpected(t, "ALL"),
                },
                "IS" => {
                    let negated = self.keyword().as_deref() == Some("NOT");
                    if negated {
                        self.next();
                    }
                    return match (self.next(), target) {
                        (Some(Tok::Word(n)), Target::Val(field)) if n.eq_ignore_ascii_case("NULL") => Ok(Filter::Null { field, negated }),
                        (Some(Tok::Word(n)), _) if n.eq_ignore_ascii_case("NULL") => fail(format!("IS NULL is not supported on {name}")),
                        (t, _) => self.unexpected(t, "NULL"),
                    };
                }
                "IN" => "IN".into(),
                "LIKE" => "LIKE".into(),
                _ => return self.unexpected(Some(Tok::Word(w)), "an operator"),
            },
            t => return self.unexpected(t, "an operator"),
        };
        match target {
            Target::Set(field, by) => {
                let op = match op.as_str() {
                    "IN" => SetOp::In,
                    "NOT IN" => SetOp::NotIn,
                    "CONTAINS ALL" => SetOp::ContainsAll,
                    o => return fail(format!("operator {o} is not supported on {name}")),
                };
                Ok(Filter::Set { field, by, op, values: self.list()? })
            }
            Target::Text(field) => {
                let op = match op.as_str() {
                    "LIKE" => TextOp::Like,
                    "NOT LIKE" => TextOp::NotLike,
                    "=" => TextOp::Eq,
                    "!=" | "<>" => TextOp::Ne,
                    o => return fail(format!("operator {o} is not supported on {name}")),
                };
                match self.next() {
                    Some(Tok::Str(value)) => Ok(Filter::Text { field, op, value }),
                    t => self.unexpected(t, "a quoted string"),
                }
            }
            Target::Val(field) => {
                let op = match op.as_str() {
                    "=" => CmpOp::Eq,
                    "!=" | "<>" => CmpOp::Ne,
                    "<" => CmpOp::Lt,
                    "<=" => CmpOp::Le,
                    ">" => CmpOp::Gt,
                    ">=" => CmpOp::Ge,
                    o => return fail(format!("operator {o} is not supported on {name}")),
                };
                let value = match (field, self.next()) {
                    (Field::Rating, Some(Tok::Num(n))) => Lit::Num(n),
                    (Field::Rating, t) => return self.unexpected(t, "a number"),
                    (_, Some(Tok::Str(s))) => match date_of(&s) {
                        Some(d) => Lit::Date(d),
                        None => return fail(format!("\"{s}\" is not a YYYY-MM-DD date")),
                    },
                    (_, t) => return self.unexpected(t, "a quoted YYYY-MM-DD date"),
                };
                Ok(Filter::Cmp { field, op, value })
            }
        }
    }

    /// `["a", 2, 'c']`, values as text.
    fn list(&mut self) -> std::result::Result<Vec<String>, FilterError> {
        match self.next() {
            Some(Tok::LBrack) => {}
            t => return self.unexpected(t, "a [list]"),
        }
        let mut values = Vec::new();
        if self.peek() == Some(&Tok::RBrack) {
            self.next();
            return Ok(values);
        }
        loop {
            match self.next() {
                Some(Tok::Str(s)) => values.push(s),
                Some(Tok::Num(n)) => values.push(n.to_string()),
                t => return self.unexpected(t, "a value"),
            }
            match self.next() {
                Some(Tok::Comma) => {}
                Some(Tok::RBrack) => return Ok(values),
                t => return self.unexpected(t, ", or ]"),
            }
        }
    }
}

/// `Ok(None)` for a blank string (Mealie: no filter, every recipe).
pub fn parse(src: &str) -> std::result::Result<Option<Filter>, FilterError> {
    if src.len() > MAX_LEN {
        return fail(format!("the filter is too long ({} bytes, at most {MAX_LEN})", src.len()));
    }
    if src.trim().is_empty() {
        return Ok(None);
    }
    let mut p = Parser { toks: lex(src)?, at: 0, depth: 0 };
    let f = p.expr()?;
    match p.next() {
        None => Ok(Some(f)),
        t => p.unexpected(t, "AND, OR or the end of the filter"),
    }
}

impl From<FilterError> for crate::Error {
    fn from(e: FilterError) -> Self {
        crate::Error::Validation(e.to_string())
    }
}

/// A value bound to a `?` in the generated SQL. User text only ever travels as one of these.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Bind {
    Text(String),
    Int(i64),
    Real(f64),
}

impl SetField {
    pub(crate) fn table(self) -> &'static str {
        match self {
            SetField::Tags => "tags",
            SetField::Categories => "categories",
            SetField::Tools => "tools",
        }
    }

    /// Where the recipe's names live in its current snapshot.
    fn path(self) -> &'static str {
        match self {
            SetField::Tags => "$.tags",
            SetField::Categories => "$.categories",
            SetField::Tools => "$.tools",
        }
    }
}

impl Field {
    /// The column-like expression over `r` (recipes), `s` (its current snapshot); binds the household where it needs one.
    fn expr(self, sc: &Scope, binds: &mut Vec<Bind>) -> &'static str {
        match self {
            Field::LastMade => {
                binds.push(Bind::Int(sc.household_id));
                "(SELECT max(b.made_on) FROM batches b WHERE b.recipe_id = r.id AND b.household_id = ? AND b.status = 'done')"
            }
            Field::Rating => {
                binds.push(Bind::Int(sc.household_id));
                "(SELECT avg(b.rating) FROM batches b WHERE b.recipe_id = r.id AND b.household_id = ? AND b.status = 'done')"
            }
            Field::DateAdded => "(SELECT substr(min(x.created_at), 1, 10) FROM recipe_snapshots x WHERE x.recipe_id = r.id)",
            Field::DateUpdated => "substr(s.created_at, 1, 10)",
        }
    }
}

impl CmpOp {
    fn sql(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

impl Filter {
    /// A SQL condition over `r` and `s`, with its binds in placeholder order. Nothing from the filter is spliced
    /// into the SQL text: only fixed fragments, and `?` for every value.
    pub(crate) fn sql(&self, sc: &Scope, binds: &mut Vec<Bind>) -> String {
        match self {
            Filter::And(parts) | Filter::Or(parts) => {
                let joiner = if matches!(self, Filter::And(_)) { " AND " } else { " OR " };
                let sqls: Vec<String> = parts.iter().map(|p| p.sql(sc, binds)).collect();
                format!("({})", sqls.join(joiner))
            }
            Filter::Set { field, by, op, values } => {
                // One EXISTS over the snapshot's names per value for CONTAINS ALL, one over all of them otherwise.
                let one = |binds: &mut Vec<Bind>, v: &str| match by {
                    By::Name => {
                        binds.push(Bind::Text(v.into()));
                        "j.value = ?".to_string()
                    }
                    By::Id => {
                        binds.push(Bind::Int(sc.group_id));
                        binds.push(Bind::Text(v.into()));
                        format!("j.value IN (SELECT name FROM {} WHERE group_id = ? AND id = ?)", field.table())
                    }
                };
                let exists = |cond: String| format!("EXISTS (SELECT 1 FROM json_each(s.content, '{}') j WHERE {cond})", field.path());
                match op {
                    SetOp::ContainsAll => {
                        let parts: Vec<String> = values.iter().map(|v| exists(one(binds, v))).collect();
                        if parts.is_empty() { "1".into() } else { format!("({})", parts.join(" AND ")) }
                    }
                    SetOp::In | SetOp::NotIn => {
                        let conds: Vec<String> = values.iter().map(|v| one(binds, v)).collect();
                        let any = if conds.is_empty() { "0".to_string() } else { exists(conds.join(" OR ")) };
                        if *op == SetOp::NotIn { format!("NOT ({any})") } else { any }
                    }
                }
            }
            Filter::Text { field, op, value } => {
                binds.push(Bind::Text(value.clone()));
                let col = match field {
                    TextField::Name => "r.name",
                    TextField::Description => "r.description",
                };
                let op = match op {
                    TextOp::Like => "LIKE",
                    TextOp::NotLike => "NOT LIKE",
                    TextOp::Eq => "=",
                    TextOp::Ne => "<>",
                };
                format!("{col} {op} ?")
            }
            Filter::Cmp { field, op, value } => {
                let expr = field.expr(sc, binds);
                binds.push(match value {
                    Lit::Num(n) => Bind::Real(*n),
                    Lit::Date(d) => Bind::Text(d.clone()),
                });
                format!("{expr} {} ?", op.sql())
            }
            Filter::Null { field, negated } => {
                let expr = field.expr(sc, binds);
                format!("{expr} IS {}NULL", if *negated { "NOT " } else { "" })
            }
        }
    }

    /// Every id the filter names (`tags.id IN [..]`) is a number, which is all a mealier id can be.
    /// For filters a user writes; the importer maps Mealie's uuids before storing.
    pub fn check_native(&self) -> std::result::Result<(), FilterError> {
        match self {
            Filter::And(p) | Filter::Or(p) => p.iter().try_for_each(Filter::check_native),
            Filter::Set { by: By::Id, values, .. } => match values.iter().find(|v| v.parse::<i64>().is_err()) {
                Some(v) => fail(format!("\"{v}\" is not a mealier id")),
                None => Ok(()),
            },
            _ => Ok(()),
        }
    }
}

/// What a stored filter selects.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Sel {
    All,
    /// A filter that could not be read: matches nothing.
    Nothing,
    Filter(Filter),
}

impl Sel {
    /// From the verbatim string and the structured form stored beside it.
    pub(crate) fn stored(query_filter: &str, structured: Option<&str>) -> Result<Sel> {
        if query_filter.trim().is_empty() {
            return Ok(Sel::All);
        }
        Ok(match structured {
            Some(j) => Sel::Filter(serde_json::from_str(j)?),
            None => Sel::Nothing,
        })
    }
}

impl Db {
    /// The caller's group's live recipes matching `filter`, by name. `lastMade` and `rating` read the caller's household's batches.
    pub async fn filter_recipes(&self, sc: &Scope, filter: &Filter) -> Result<Vec<RecipeSummary>> {
        self.select_recipes(sc, &[Sel::Filter(filter.clone())]).await
    }

    /// Live recipes of the caller's group matching any of `sels`, by name; none given matches nothing.
    pub(crate) async fn select_recipes(&self, sc: &Scope, sels: &[Sel]) -> Result<Vec<RecipeSummary>> {
        let mut binds = vec![Bind::Int(sc.group_id)];
        let conds: Vec<String> = sels
            .iter()
            .map(|s| match s {
                Sel::All => "1".to_string(),
                Sel::Nothing => "0".to_string(),
                Sel::Filter(f) => f.sql(sc, &mut binds),
            })
            .collect();
        let cond = if conds.is_empty() { "0".to_string() } else { conds.join(" OR ") };
        let sql = format!(
            "SELECT r.id, r.name, r.archived FROM recipes r JOIN recipe_snapshots s ON s.id = r.current_snapshot_id
             WHERE r.group_id = ? AND r.archived = 0 AND ({cond}) ORDER BY r.name, r.id"
        );
        let mut q = sqlx::query(&sql);
        for b in binds {
            q = match b {
                Bind::Text(t) => q.bind(t),
                Bind::Int(i) => q.bind(i),
                Bind::Real(r) => q.bind(r),
            };
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| RecipeSummary { id: r.get(0), name: r.get(1), archived: r.get(2) }).collect())
    }
}
