//! YAML frontmatter between `---` lines. Parsing never fails the sync: a bad block yields empty
//! frontmatter plus an error string the caller reports and stores in `documents.parse_error`.

use chrono::NaiveDate;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntitySpec {
    pub slug: Option<String>,
    pub kind: Option<String>,
    pub name: Option<String>,
    pub aliases: Vec<String>,
}

/// Typed view of the frontmatter fields the engine uses. `raw` keeps everything (stored as jsonb).
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub raw: Map<String, Value>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub status: Option<String>,
    pub categories: Vec<String>,
    pub created: Option<NaiveDate>,
    pub updated: Option<NaiveDate>,
    pub source: Option<String>,
    pub private: bool,
    pub tags: Vec<String>,
    pub rating: Option<i64>,
    pub aliases: Vec<String>,
    pub entity: Option<EntitySpec>,
}

/// Split `text` into (yaml, body). `yaml` is `None` when the file has no frontmatter. An opening
/// `---` without a closing line is an error; the whole text is then the body.
pub fn split(text: &str) -> (Option<&str>, &str, Option<String>) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = if let Some(r) = t.strip_prefix("---\n") {
        r
    } else if let Some(r) = t.strip_prefix("---\r\n") {
        r
    } else {
        return (None, t, None);
    };
    let mut pos = 0;
    for line in rest.split_inclusive('\n') {
        let l = line.trim_end_matches(['\n', '\r']);
        if l == "---" || l == "..." {
            return (Some(&rest[..pos]), &rest[pos + line.len()..], None);
        }
        pos += line.len();
    }
    (None, t, Some("frontmatter opened with --- but never closed".into()))
}

/// Parse a whole markdown file: frontmatter (typed + raw), body, and a parse error if any.
pub fn parse_document(text: &str) -> (Frontmatter, &str, Option<String>) {
    let (yaml, body, err) = split(text);
    let Some(yaml) = yaml else { return (Frontmatter::default(), body, err) };
    if yaml.trim().is_empty() {
        return (Frontmatter::default(), body, None);
    }
    match serde_yaml_ng::from_str::<Value>(yaml) {
        Ok(Value::Object(map)) => (from_map(map), body, None),
        Ok(Value::Null) => (Frontmatter::default(), body, None),
        Ok(other) => (Frontmatter::default(), body, Some(format!("frontmatter is not a mapping ({})", kind(&other)))),
        Err(e) => (Frontmatter::default(), body, Some(format!("frontmatter YAML: {e}"))),
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "mapping",
    }
}

fn from_map(raw: Map<String, Value>) -> Frontmatter {
    let s = |k: &str| raw.get(k).and_then(scalar_string).filter(|s| !s.is_empty());
    let entity = raw.get("entity").and_then(|v| match v {
        Value::Object(m) => Some(EntitySpec {
            slug: m.get("slug").and_then(scalar_string).filter(|s| !s.is_empty()),
            kind: m.get("kind").and_then(scalar_string).filter(|s| !s.is_empty()),
            name: m.get("name").and_then(scalar_string).filter(|s| !s.is_empty()),
            aliases: m.get("aliases").map(string_list).unwrap_or_default(),
        }),
        Value::Bool(true) => Some(EntitySpec::default()),
        Value::String(slug) if !slug.trim().is_empty() => {
            Some(EntitySpec { slug: Some(slug.trim().to_string()), ..Default::default() })
        }
        _ => None,
    });
    Frontmatter {
        title: s("title"),
        author: s("author").map(|a| a.to_lowercase()),
        status: s("status").map(|a| a.to_lowercase()),
        categories: raw.get("categories").map(string_list).unwrap_or_default(),
        created: raw.get("created").and_then(date),
        updated: raw.get("updated").and_then(date),
        source: s("source").or_else(|| s("url")),
        private: raw.get("private").is_some_and(truthy),
        tags: raw
            .get("tags")
            .map(string_list)
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.trim_start_matches('#').to_string())
            .collect(),
        rating: raw.get("rating").and_then(|v| match v {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.round() as i64)),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }),
        aliases: raw.get("aliases").map(string_list).unwrap_or_default(),
        entity,
        raw,
    }
}

fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        // Unquoted `[[GPUs]]` in YAML is a nested flow list: unwrap single-element lists.
        Value::Array(a) if a.len() == 1 => scalar_string(&a[0]),
        _ => None,
    }
}

/// Strip Obsidian link syntax from a frontmatter value: `[[GPUs]]` / `[[GPUs|gpus]]` → `GPUs`.
pub fn unlink(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")).unwrap_or(t);
    let t = t.split('|').next().unwrap_or(t);
    t.trim().to_string()
}

/// A list field that may be a YAML list, a single string, or a comma-separated string.
fn string_list(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Value::Array(items) => {
            for it in items {
                if let Some(s) = scalar_string(it) {
                    out.push(unlink(&s));
                }
            }
        }
        Value::String(s) => {
            if s.contains("[[") {
                out.push(unlink(s));
            } else {
                out.extend(s.split(',').map(unlink));
            }
        }
        other => {
            if let Some(s) = scalar_string(other) {
                out.push(unlink(&s));
            }
        }
    }
    out.retain(|s| !s.is_empty());
    out.dedup();
    out
}

fn date(v: &Value) -> Option<NaiveDate> {
    let s = scalar_string(v)?;
    let head = s.get(..10).unwrap_or(&s);
    NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.trim().to_lowercase().as_str(), "true" | "yes" | "y" | "1"),
        Value::Number(n) => n.as_i64() == Some(1),
        _ => false,
    }
}

/// Serialize a frontmatter mapping (keys in the given order) as a `---` block.
pub fn render(fields: &[(String, Value)]) -> anyhow::Result<String> {
    let mut map = serde_yaml_ng::Mapping::new();
    for (k, v) in fields {
        map.insert(serde_yaml_ng::Value::String(k.clone()), serde_yaml_ng::to_value(v)?);
    }
    let y = serde_yaml_ng::to_string(&map)?;
    Ok(format!("---\n{y}---\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields() {
        let text = "---\ntitle: GB10\nauthor: Agent\ncategories:\n  - \"[[GPUs]]\"\n  - [[Chips]]\ncreated: 2026-11-07\nprivate: true\ntags: [a, \"#b\"]\nrating: 6\naliases: [Spark chip]\nentity:\n  slug: nvidia-gb10\n  kind: chip\n  aliases: [GB10]\n---\n# Body\ntext\n";
        let (fm, body, err) = parse_document(text);
        assert!(err.is_none());
        assert_eq!(body, "# Body\ntext\n");
        assert_eq!(fm.title.as_deref(), Some("GB10"));
        assert_eq!(fm.author.as_deref(), Some("agent"));
        assert_eq!(fm.categories, vec!["GPUs", "Chips"]);
        assert_eq!(fm.created, NaiveDate::from_ymd_opt(2026, 11, 7));
        assert!(fm.private);
        assert_eq!(fm.tags, vec!["a", "b"]);
        assert_eq!(fm.rating, Some(6));
        assert_eq!(fm.aliases, vec!["Spark chip"]);
        let e = fm.entity.unwrap();
        assert_eq!(e.slug.as_deref(), Some("nvidia-gb10"));
        assert_eq!(e.kind.as_deref(), Some("chip"));
        assert_eq!(e.aliases, vec!["GB10"]);
        assert_eq!(fm.raw["rating"], 6);
    }

    #[test]
    fn bad_yaml_is_reported_not_fatal() {
        let (fm, body, err) = parse_document("---\ntitle: [unclosed\n---\nhello\n");
        assert!(err.is_some());
        assert!(fm.title.is_none());
        assert_eq!(body, "hello\n");
        let (_, body, err) = parse_document("---\ntitle: x\nno close\n");
        assert!(err.is_some());
        assert!(body.starts_with("---"));
        let (_, body, err) = parse_document("no frontmatter\n---\n");
        assert!(err.is_none());
        assert_eq!(body, "no frontmatter\n---\n");
    }

    #[test]
    fn renders_roundtrip() {
        let out = render(&[("title".into(), Value::from("A: b")), ("tags".into(), serde_json::json!(["x"]))]).unwrap();
        let (fm, body, err) = parse_document(&out);
        assert!(err.is_none(), "{out}");
        assert_eq!(fm.title.as_deref(), Some("A: b"));
        assert_eq!(body, "");
    }
}
