//! Patch definitions, one TOML file per stock firmware in `firmware/` (embedded at
//! build time). See `docs/firmware-definitions.md` for the format.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::{err, Result};

include!(concat!(env!("OUT_DIR"), "/definitions.rs"));

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// SHA-256 of the unmodified `GEMINI_PACK.BIN`.
    pub sha256: String,
    pub size: u64,
    /// Where the vendor publishes this exact file.
    #[serde(default)]
    pub download_url: Option<String>,
    /// Vendor directory listing that is checked for newer versions.
    #[serde(default)]
    pub update_index: Option<String>,
    /// Shared actions, applied once if any selected patch `requires` them.
    #[serde(default)]
    pub components: BTreeMap<String, ComponentDef>,
    #[serde(rename = "patch")]
    pub patches: Vec<PatchDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentDef {
    #[serde(rename = "action", default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PatchDef {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(rename = "param", default)]
    pub params: Vec<ParamDef>,
    #[serde(default)]
    pub requires: Vec<String>,
    /// Lines added to a named collection, which component templates can insert with
    /// `{{collect:<name>}}`.
    #[serde(rename = "contribute", default)]
    pub contributions: Vec<Contribution>,
    #[serde(rename = "action", default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    pub collection: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParamDef {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default = "one")]
    pub min_bytes: usize,
    pub max_bytes: usize,
    /// Characters that may not appear in the value (in addition to control characters).
    #[serde(default)]
    pub forbidden_chars: String,
    /// Only printable ASCII is allowed.
    #[serde(default)]
    pub ascii_only: bool,
}

fn one() -> usize {
    1
}

fn default_mode() -> u16 {
    0o755
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Replace bytes at `offset` in `file`, which must currently be `expect`.
    BytePatch {
        partition: String,
        file: String,
        offset: u64,
        expect: String,
        replace: String,
        #[serde(default)]
        note: String,
    },
    /// Replace exactly `count` occurrences of `find` in a text file.
    ReplaceText {
        partition: String,
        file: String,
        find: String,
        replace: String,
        #[serde(default = "one")]
        count: usize,
    },
    /// Add a new file (its directory must exist and the name must be unused).
    AddFile {
        partition: String,
        path: String,
        #[serde(default = "default_mode")]
        mode: u16,
        content: String,
    },
}

impl Action {
    pub fn partition(&self) -> &str {
        match self {
            Action::BytePatch { partition, .. }
            | Action::ReplaceText { partition, .. }
            | Action::AddFile { partition, .. } => partition,
        }
    }

    /// The action with `{{...}}` placeholders substituted in all text fields.
    pub fn render(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Action> {
        let r = |s: &str| render(s, lookup);
        Ok(match self {
            Action::BytePatch { .. } => self.clone(),
            Action::ReplaceText {
                partition,
                file,
                find,
                replace,
                count,
            } => Action::ReplaceText {
                partition: partition.clone(),
                file: file.clone(),
                find: r(find)?,
                replace: r(replace)?,
                count: *count,
            },
            Action::AddFile {
                partition,
                path,
                mode,
                content,
            } => Action::AddFile {
                partition: partition.clone(),
                path: path.clone(),
                mode: *mode,
                content: r(content)?,
            },
        })
    }
}

impl ParamDef {
    pub fn validate(&self, value: &str) -> Result<()> {
        let label = &self.label;
        if value.len() < self.min_bytes {
            return Err(err!("{label}: too short (at least {} bytes)", self.min_bytes));
        }
        if value.len() > self.max_bytes {
            return Err(err!(
                "{label}: too long ({} bytes, at most {})",
                value.len(),
                self.max_bytes
            ));
        }
        if value.trim() != value {
            return Err(err!("{label}: must not start or end with spaces"));
        }
        if let Some(c) = value
            .chars()
            .find(|c| c.is_control() || self.forbidden_chars.contains(*c) || (self.ascii_only && !c.is_ascii()))
        {
            return Err(err!("{label}: character {c:?} is not allowed"));
        }
        Ok(())
    }
}

/// Substitutes `{{name}}` placeholders. Unknown names are an error.
pub fn render(template: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let end = rest[i..]
            .find("}}")
            .ok_or_else(|| err!("unclosed '{{{{' in template"))?
            + i;
        let name = rest[i + 2..end].trim();
        out.push_str(&lookup(name).ok_or_else(|| err!("unknown template placeholder '{name}'"))?);
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

impl FirmwareDef {
    pub fn patch(&self, id: &str) -> Option<&PatchDef> {
        self.patches.iter().find(|p| p.id == id)
    }

    fn check(&self) -> Result<()> {
        let ctx = |e: crate::Error| err!("firmware definition '{}': {}", self.id, e.0);
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ctx(err!("sha256 must be 64 lowercase hex digits")));
        }
        let collections: BTreeSet<&str> = self
            .patches
            .iter()
            .flat_map(|p| p.contributions.iter().map(|c| c.collection.as_str()))
            .collect();
        let collect = |name: &str| {
            name.strip_prefix("collect:")
                .filter(|c| collections.contains(c))
                .map(|_| String::new())
        };
        for (name, c) in &self.components {
            for a in &c.actions {
                a.render(&collect).map_err(|e| ctx(err!("component {name}: {e}")))?;
            }
        }
        let mut ids = BTreeSet::new();
        for p in &self.patches {
            let pctx = |e: crate::Error| ctx(err!("patch {}: {}", p.id, e.0));
            if !ids.insert(&p.id) {
                return Err(pctx(err!("duplicate patch id")));
            }
            for r in &p.requires {
                if !self.components.contains_key(r) {
                    return Err(pctx(err!("requires unknown component '{r}'")));
                }
            }
            let params = |name: &str| {
                p.params
                    .iter()
                    .find(|q| q.id == name)
                    .map(|_| String::new())
                    .or_else(|| collect(name))
            };
            for c in &p.contributions {
                render(&c.value, &params).map_err(pctx)?;
            }
            for a in &p.actions {
                a.render(&params).map_err(pctx)?;
                if let Action::BytePatch { expect, replace, .. } = a {
                    let (e, r) = (hex::decode(expect), hex::decode(replace));
                    match (e, r) {
                        (Ok(e), Ok(r)) if e.len() == r.len() && !e.is_empty() => {}
                        _ => return Err(pctx(err!("expect/replace must be equal-length hex"))),
                    }
                }
            }
        }
        Ok(())
    }
}

/// All firmware definitions shipped with this build.
pub fn catalog() -> &'static [FirmwareDef] {
    static CATALOG: OnceLock<Vec<FirmwareDef>> = OnceLock::new();
    CATALOG.get_or_init(|| load().unwrap_or_else(|e| panic!("{e}")))
}

fn load() -> Result<Vec<FirmwareDef>> {
    let mut defs = Vec::new();
    for (file, text) in DEFINITIONS {
        let def: FirmwareDef = toml::from_str(text).map_err(|e| err!("{file}: {e}"))?;
        def.check()?;
        defs.push(def);
    }
    let mut seen = BTreeSet::new();
    for d in &defs {
        if !seen.insert(&d.sha256) || defs.iter().filter(|x| x.id == d.id).count() > 1 {
            return Err(err!("duplicate firmware id or sha256: {}", d.id));
        }
    }
    Ok(defs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads() {
        assert!(!load().unwrap().is_empty());
    }

    #[test]
    fn templates() {
        let f = |n: &str| (n == "a").then(|| "X".to_string());
        assert_eq!(render("1{{a}}2{{ a }}", &f).unwrap(), "1X2X");
        assert!(render("{{b}}", &f).is_err());
        assert!(render("{{a", &f).is_err());
    }
}
