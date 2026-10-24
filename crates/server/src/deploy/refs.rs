//! Variable references, Railway style: `${{shared.NAME}}`, `${{service.NAME}}`
//! (another service in the same environment) and `${{NAME}}` (the same
//! service). References resolve when a deployment is created, so a deployment
//! keeps the values it started with.

use std::collections::BTreeMap;

/// Where a reference points.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Shared,
    /// The service the variable belongs to.
    Own,
    Service(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ref {
    pub target: Target,
    pub name: String,
}

impl Ref {
    pub fn parse(s: &str) -> Option<Ref> {
        parse(s)
    }

    /// `shared.NAME`, `service.NAME`, or `NAME` for the same service.
    pub fn key(&self) -> String {
        match &self.target {
            Target::Shared => format!("shared.{}", self.name),
            Target::Own => self.name.clone(),
            Target::Service(s) => format!("{s}.{}", self.name),
        }
    }
}

/// One `${{ … }}` occurrence: its byte range in the value and what it names.
/// Malformed ones (unknown characters, unterminated) are left as literal text.
fn occurrences(value: &str) -> Vec<(std::ops::Range<usize>, Ref)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) = value[from..].find("${{").map(|i| from + i) {
        let Some(len) = value[start + 3..].find("}}") else {
            break;
        };
        let end = start + 3 + len + 2;
        if let Some(r) = parse(value[start + 3..start + 3 + len].trim()) {
            out.push((start..end, r));
        }
        from = end;
    }
    out
}

/// One reference without the braces: `shared.NAME`, `service.NAME` or `NAME`.
pub fn parse(inner: &str) -> Option<Ref> {
    let is_var = |s: &str| {
        !s.is_empty()
            && !s.as_bytes()[0].is_ascii_digit()
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    // Service names are stored lowercase, but Railway configs write them as
    // displayed (`${{Postgres.DATABASE_URL}}`), so the service segment matches
    // case-insensitively. Variable names stay case-sensitive.
    let is_service =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    match inner.split_once('.') {
        // Any case, like service names: `Shared.X` must not become a service
        // named "shared" whose key (`shared.X`) then reads back as shared.
        Some((svc, name)) if svc.eq_ignore_ascii_case("shared") && is_var(name) => Some(Ref {
            target: Target::Shared,
            name: name.to_owned(),
        }),
        Some((svc, name)) if is_service(svc) && is_var(name) => Some(Ref {
            target: Target::Service(svc.to_ascii_lowercase()),
            name: name.to_owned(),
        }),
        None if is_var(inner) => Some(Ref {
            target: Target::Own,
            name: inner.to_owned(),
        }),
        _ => None,
    }
}

/// The references in a value, deduplicated, in order of first use.
pub fn references(value: &str) -> Vec<Ref> {
    let mut seen = Vec::new();
    for (_, r) in occurrences(value) {
        if !seen.contains(&r) {
            seen.push(r);
        }
    }
    seen
}

/// A variable's identity across an environment: `(service name or None for
/// shared, NAME)`.
pub type VarKey = (Option<String>, String);

/// Turn a reference made by `owner` (None = a shared variable) into the key
/// of the variable it names.
pub fn resolve_key(owner: Option<&str>, r: &Ref) -> VarKey {
    match &r.target {
        Target::Shared => (None, r.name.clone()),
        // `${{NAME}}` inside a shared variable means another shared variable.
        Target::Own => (owner.map(str::to_owned), r.name.clone()),
        Target::Service(s) => (Some(s.clone()), r.name.clone()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExpandError {
    /// The chain of variables that loops, e.g. `api.A → api.B → api.A`.
    Cycle(Vec<String>),
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cycle(chain) => write!(
                f,
                "variables reference each other in a loop: {}",
                chain.join(" → ")
            ),
        }
    }
}

pub fn label(k: &VarKey) -> String {
    match &k.0 {
        Some(s) => format!("{s}.{}", k.1),
        None => format!("shared.{}", k.1),
    }
}

/// Expands every reference in the variables of the environment. `raw` holds
/// every variable by key. Missing references expand to an empty string and are
/// listed in the second return value.
pub struct Expander<'a> {
    raw: &'a BTreeMap<VarKey, String>,
    done: BTreeMap<VarKey, String>,
    pub missing: Vec<String>,
}

const MAX_DEPTH: usize = 16;

impl<'a> Expander<'a> {
    pub fn new(raw: &'a BTreeMap<VarKey, String>) -> Self {
        Self {
            raw,
            done: BTreeMap::new(),
            missing: Vec::new(),
        }
    }

    pub fn value(&mut self, key: &VarKey) -> Result<Option<String>, ExpandError> {
        self.expand(key, &mut Vec::new())
    }

    fn expand(
        &mut self,
        key: &VarKey,
        stack: &mut Vec<VarKey>,
    ) -> Result<Option<String>, ExpandError> {
        if let Some(v) = self.done.get(key) {
            return Ok(Some(v.clone()));
        }
        let Some(raw) = self.raw.get(key) else {
            return Ok(None);
        };
        if stack.contains(key) || stack.len() >= MAX_DEPTH {
            let mut chain: Vec<String> =
                stack.iter().skip_while(|k| *k != key).map(label).collect();
            chain.push(label(key));
            return Err(ExpandError::Cycle(chain));
        }
        stack.push(key.clone());
        let mut out = String::with_capacity(raw.len());
        let mut last = 0;
        for (range, r) in occurrences(raw) {
            out.push_str(&raw[last..range.start]);
            let target = resolve_key(key.0.as_deref(), &r);
            match self.expand(&target, stack)? {
                Some(v) => out.push_str(&v),
                None => {
                    let l = label(&target);
                    if !self.missing.contains(&l) {
                        self.missing.push(l);
                    }
                }
            }
            last = range.end;
        }
        out.push_str(&raw[last..]);
        stack.pop();
        self.done.insert(key.clone(), out.clone());
        Ok(Some(out))
    }
}

/// Whether `key` is in `targets` or its value references one of them,
/// directly or through other variables in `raw`.
pub fn depends_on(
    raw: &BTreeMap<VarKey, String>,
    key: &VarKey,
    targets: &std::collections::BTreeSet<VarKey>,
) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    let mut todo = vec![key.clone()];
    while let Some(k) = todo.pop() {
        if targets.contains(&k) {
            return true;
        }
        if !seen.insert(k.clone()) {
            continue;
        }
        if let Some(value) = raw.get(&k) {
            todo.extend(
                occurrences(value)
                    .into_iter()
                    .map(|(_, r)| resolve_key(k.0.as_deref(), &r)),
            );
        }
    }
    false
}

/// Reject a set of variables whose references loop (checked on save, from
/// the references alone; values aren't needed).
pub fn check_cycles(edges: &BTreeMap<VarKey, Vec<VarKey>>) -> Result<(), ExpandError> {
    fn visit(
        k: &VarKey,
        edges: &BTreeMap<VarKey, Vec<VarKey>>,
        stack: &mut Vec<VarKey>,
        clean: &mut std::collections::BTreeSet<VarKey>,
    ) -> Result<(), ExpandError> {
        if clean.contains(k) {
            return Ok(());
        }
        if let Some(pos) = stack.iter().position(|s| s == k) {
            let mut chain: Vec<String> = stack[pos..].iter().map(label).collect();
            chain.push(label(k));
            return Err(ExpandError::Cycle(chain));
        }
        stack.push(k.clone());
        for next in edges.get(k).into_iter().flatten() {
            visit(next, edges, stack, clean)?;
        }
        stack.pop();
        clean.insert(k.clone());
        Ok(())
    }
    let mut clean = std::collections::BTreeSet::new();
    for k in edges.keys() {
        visit(k, edges, &mut Vec::new(), &mut clean)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: Option<&str>, n: &str) -> VarKey {
        (s.map(str::to_owned), n.to_owned())
    }

    #[test]
    fn parses_the_three_forms_and_ignores_junk() {
        let refs = references(
            "postgres://${{ db.USER }}:${{shared.PASS}}@${{HOST}}:${{ bad name }}${{x.y.z}}${{unterminated",
        );
        let keys: Vec<String> = refs.iter().map(Ref::key).collect();
        assert_eq!(keys, ["db.USER", "shared.PASS", "HOST"]);
    }

    #[test]
    fn service_names_match_case_insensitively() {
        let keys: Vec<String> = references(
            "${{Postgres.DATABASE_URL}}${{REDIS-cache.URL}}${{postgres.database_url}}${{Shared.X}}",
        )
        .iter()
        .map(Ref::key)
        .collect();
        // The service is normalized to its stored (lowercase) form; variable
        // names keep their case.
        assert_eq!(
            keys,
            [
                "postgres.DATABASE_URL",
                "redis-cache.URL",
                "postgres.database_url",
                "shared.X"
            ]
        );
        assert_eq!(parse("Shared.X").unwrap().target, Target::Shared);

        let raw: BTreeMap<VarKey, String> = [
            (
                key(Some("api"), "DATABASE_URL"),
                "${{Postgres.DATABASE_URL}}?schema=public".into(),
            ),
            (
                key(Some("postgres"), "DATABASE_URL"),
                "postgresql://db".into(),
            ),
        ]
        .into();
        let mut e = Expander::new(&raw);
        assert_eq!(
            e.value(&key(Some("api"), "DATABASE_URL")).unwrap().unwrap(),
            "postgresql://db?schema=public"
        );
        assert!(e.missing.is_empty());
    }

    #[test]
    fn expands_across_services_and_reports_missing() {
        let raw: BTreeMap<VarKey, String> = [
            (
                key(Some("api"), "DATABASE_URL"),
                "postgres://u:${{db.PASSWORD}}@${{db.HOST}}/${{shared.DB}}".into(),
            ),
            (key(Some("db"), "PASSWORD"), "s3cret".into()),
            (key(Some("db"), "HOST"), "${{MODULA_PRIVATE_DOMAIN}}".into()),
            (
                key(Some("db"), "MODULA_PRIVATE_DOMAIN"),
                "db.modula.internal".into(),
            ),
        ]
        .into();
        let mut e = Expander::new(&raw);
        assert_eq!(
            e.value(&key(Some("api"), "DATABASE_URL")).unwrap().unwrap(),
            "postgres://u:s3cret@db.modula.internal/"
        );
        assert_eq!(e.missing, ["shared.DB"]);
    }

    #[test]
    fn dependencies_are_followed_through_other_variables() {
        let raw: BTreeMap<VarKey, String> = [
            (key(Some("api"), "URL"), "postgres://${{db.AUTH}}@db".into()),
            (key(Some("api"), "PLAIN"), "${{shared.REGION}}".into()),
            (key(Some("api"), "LOOP"), "${{LOOP}}".into()),
            (key(Some("db"), "AUTH"), "u:${{shared.SECRET}}".into()),
            (key(None, "SECRET"), "s3cret".into()),
            (key(None, "REGION"), "eu".into()),
        ]
        .into();
        let sealed = [key(None, "SECRET")].into();
        assert!(depends_on(&raw, &key(Some("api"), "URL"), &sealed));
        assert!(depends_on(&raw, &key(None, "SECRET"), &sealed));
        assert!(!depends_on(&raw, &key(Some("api"), "PLAIN"), &sealed));
        assert!(!depends_on(&raw, &key(Some("api"), "LOOP"), &sealed));
    }

    #[test]
    fn cycles_are_errors() {
        let raw: BTreeMap<VarKey, String> = [
            (key(Some("a"), "X"), "${{b.Y}}".into()),
            (key(Some("b"), "Y"), "${{a.X}}".into()),
        ]
        .into();
        let err = Expander::new(&raw).value(&key(Some("a"), "X")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "variables reference each other in a loop: a.X → b.Y → a.X"
        );

        let edges: BTreeMap<VarKey, Vec<VarKey>> = [
            (key(None, "A"), vec![key(Some("s"), "B")]),
            (key(Some("s"), "B"), vec![key(None, "A")]),
        ]
        .into();
        assert!(check_cycles(&edges).is_err());
        let ok: BTreeMap<VarKey, Vec<VarKey>> =
            [(key(None, "A"), vec![key(Some("s"), "B")])].into();
        check_cycles(&ok).unwrap();
    }
}
