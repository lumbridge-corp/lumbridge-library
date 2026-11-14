//! SPDX detection without heavy deps: phrase heuristics over LICENSE/COPYING files plus manifest
//! fields (Cargo.toml, package.json, pyproject.toml). Unknown stays None (= all rights reserved).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LicenseInfo {
    pub spdx: Option<String>,
    pub confidence: Option<f32>,
    pub copyleft: Option<bool>,
    /// Where it came from: file names and/or manifest.
    pub sources: Vec<String>,
}

/// Is a root-level path a license file?
pub fn is_license_file(path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    let l = path.to_ascii_lowercase();
    l.starts_with("license") || l.starts_with("licence") || l.starts_with("copying") || l == "unlicense"
}

/// Identify one license text.
pub fn identify_text(text: &str) -> Option<(&'static str, f32)> {
    let t: String = text.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let has = |p: &str| t.contains(p);
    // Order matters: AGPL and LGPL texts mention the GPL.
    if has("gnu affero general public license") {
        return Some(("AGPL-3.0", 0.9));
    }
    if has("gnu lesser general public license") {
        return Some(if has("version 3") { ("LGPL-3.0", 0.9) } else { ("LGPL-2.1", 0.85) });
    }
    if has("gnu library general public license") {
        return Some(("LGPL-2.0", 0.85));
    }
    if has("gnu general public license") {
        if has("version 3") {
            return Some(("GPL-3.0", 0.9));
        }
        if has("version 2") {
            return Some(("GPL-2.0", 0.9));
        }
        return Some(("GPL-3.0", 0.6));
    }
    if has("mozilla public license") {
        return Some(if has("2.0") { ("MPL-2.0", 0.9) } else { ("MPL-1.1", 0.7) });
    }
    if has("apache license") && (has("version 2.0") || has("apache-2.0")) {
        return Some(("Apache-2.0", 0.9));
    }
    if has("boost software license") {
        return Some(("BSL-1.0", 0.9));
    }
    if has("this is free and unencumbered software released into the public domain") {
        return Some(("Unlicense", 0.95));
    }
    if has("cc0 1.0 universal") || has("creative commons zero") || has("creativecommons.org/publicdomain/zero") {
        return Some(("CC0-1.0", 0.9));
    }
    if has("permission is hereby granted, free of charge") {
        return Some(("MIT", 0.9));
    }
    if has("permission to use, copy, modify, and/or distribute this software for any purpose")
        || has("permission to use, copy, modify, and distribute this software for any purpose with or without fee")
    {
        return Some(("ISC", 0.85));
    }
    if has("redistribution and use in source and binary forms") {
        if has("neither the name") || has("endorse or promote products") {
            return Some(("BSD-3-Clause", 0.85));
        }
        return Some(("BSD-2-Clause", 0.8));
    }
    if has("altered source versions must be plainly marked") {
        return Some(("Zlib", 0.85));
    }
    None
}

/// Normalise a manifest expression: "MIT/Apache-2.0" → "MIT OR Apache-2.0".
pub fn normalise_expr(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.replace(" / ", " OR ").replace('/', " OR ");
    let s = s.split_whitespace().map(|w| match w.to_ascii_lowercase().as_str() {
        "or" => "OR".to_string(),
        "and" => "AND".to_string(),
        "with" => "WITH".to_string(),
        _ => w.to_string(),
    });
    Some(s.collect::<Vec<_>>().join(" "))
}

fn ident_is_copyleft(id: &str) -> bool {
    let u = id.to_ascii_uppercase();
    u.contains("GPL") || u.starts_with("MPL") || u.starts_with("EPL") || u.starts_with("CDDL") || u.starts_with("EUPL")
        || u.starts_with("OSL") || u.starts_with("CC-BY-SA")
}

/// Alternatives of an `OR` expression (parentheses ignored).
pub fn alternatives(expr: &str) -> Vec<String> {
    expr.replace(['(', ')'], " ")
        .split(" OR ")
        .map(|a| a.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|a| !a.is_empty())
        .collect()
}

/// Copyleft if every alternative carries a copyleft license (so `MIT OR GPL-2.0` is not).
pub fn is_copyleft(expr: &str) -> bool {
    let alts = alternatives(expr);
    !alts.is_empty()
        && alts.iter().all(|a| a.split(' ').filter(|w| !matches!(*w, "AND" | "WITH")).any(ident_is_copyleft))
}

/// May a distilled component under `component` be taken from a source under `source`?
/// Equal expressions, or one of the source's OR-alternatives.
pub fn compatible(source: &str, component: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    if norm(source) == norm(component) {
        return true;
    }
    let alts = alternatives(source);
    let comp_alts = alternatives(component);
    !comp_alts.is_empty() && comp_alts.iter().all(|c| alts.iter().any(|a| norm(a) == norm(c)))
}

pub fn from_cargo_toml(text: &str) -> Option<String> {
    let v: toml::Value = toml::from_str(text).ok()?;
    let pkg = v.get("package").and_then(|p| p.get("license")).and_then(|l| l.as_str());
    let ws = v.get("workspace").and_then(|w| w.get("package")).and_then(|p| p.get("license")).and_then(|l| l.as_str());
    pkg.or(ws).and_then(normalise_expr)
}

pub fn from_package_json(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    match v.get("license")? {
        serde_json::Value::String(s) if !s.eq_ignore_ascii_case("UNLICENSED") && !s.starts_with("SEE ") => normalise_expr(s),
        serde_json::Value::Object(o) => o.get("type").and_then(|t| t.as_str()).and_then(normalise_expr),
        _ => None,
    }
}

pub fn from_pyproject(text: &str) -> Option<String> {
    let v: toml::Value = toml::from_str(text).ok()?;
    let project = v.get("project").or_else(|| v.get("tool").and_then(|t| t.get("poetry")));
    if let Some(p) = project {
        match p.get("license") {
            Some(toml::Value::String(s)) if s.len() < 80 => return normalise_expr(s),
            Some(toml::Value::Table(t)) => {
                if let Some(s) = t.get("text").and_then(|x| x.as_str()) {
                    if s.len() < 80 {
                        return normalise_expr(s);
                    }
                    return identify_text(s).map(|(id, _)| id.to_string());
                }
            }
            _ => {}
        }
        if let Some(cls) = p.get("classifiers").and_then(|c| c.as_array()) {
            for c in cls.iter().filter_map(|c| c.as_str()) {
                let id = match c {
                    c if c.contains("MIT License") => "MIT",
                    c if c.contains("Apache Software License") => "Apache-2.0",
                    c if c.contains("BSD License") => "BSD-3-Clause",
                    c if c.contains("GNU General Public License v3") => "GPL-3.0",
                    c if c.contains("GNU General Public License v2") => "GPL-2.0",
                    c if c.contains("Mozilla Public License 2.0") => "MPL-2.0",
                    c if c.contains("ISC License") => "ISC",
                    _ => continue,
                };
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Combine manifest + license files into one answer.
pub fn combine(manifest: Option<(String, String)>, files: Vec<(String, &'static str, f32)>) -> LicenseInfo {
    let mut ids: Vec<&'static str> = files.iter().map(|f| f.1).collect();
    ids.sort();
    ids.dedup();
    let mut sources: Vec<String> = files.iter().map(|f| f.0.clone()).collect();
    if let Some((expr, from)) = manifest {
        sources.push(from);
        let alts = alternatives(&expr);
        let agrees = !ids.is_empty()
            && ids.iter().all(|id| alts.iter().any(|a| a.split(' ').any(|w| w.trim_end_matches("-only").trim_end_matches("-or-later") == *id || w == *id)));
        let conf = if agrees { 0.95 } else if ids.is_empty() { 0.85 } else { 0.7 };
        let copyleft = is_copyleft(&expr);
        return LicenseInfo { spdx: Some(expr), confidence: Some(conf), copyleft: Some(copyleft), sources };
    }
    match ids.len() {
        0 => LicenseInfo { spdx: None, confidence: None, copyleft: None, sources },
        1 => {
            let conf = files.iter().map(|f| f.2).fold(0.0, f32::max);
            let id = ids[0].to_string();
            LicenseInfo { copyleft: Some(is_copyleft(&id)), spdx: Some(id), confidence: Some(conf * 0.95), sources }
        }
        _ => {
            // Several license files at the root usually means a dual license (MIT + Apache-2.0).
            let expr = ids.join(" OR ");
            LicenseInfo { copyleft: Some(is_copyleft(&expr)), spdx: Some(expr), confidence: Some(0.6), sources }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies() {
        assert_eq!(identify_text("Permission is hereby granted, free of charge, to any person").unwrap().0, "MIT");
        assert_eq!(identify_text("GNU LESSER GENERAL PUBLIC LICENSE Version 3, 29 June 2007 ... GNU General Public License").unwrap().0, "LGPL-3.0");
        assert_eq!(identify_text("GNU GENERAL PUBLIC LICENSE\n Version 2, June 1991").unwrap().0, "GPL-2.0");
        assert_eq!(identify_text("Apache License\nVersion 2.0, January 2004").unwrap().0, "Apache-2.0");
        assert_eq!(identify_text("hello").map(|x| x.0), None);
    }

    #[test]
    fn expressions() {
        assert_eq!(normalise_expr("MIT/Apache-2.0").unwrap(), "MIT OR Apache-2.0");
        assert!(!is_copyleft("MIT OR Apache-2.0"));
        assert!(is_copyleft("GPL-3.0-only"));
        assert!(!is_copyleft("MIT OR GPL-2.0"));
        assert!(is_copyleft("LGPL-2.1 AND MIT"));
        assert!(compatible("MIT OR Apache-2.0", "MIT"));
        assert!(compatible("MIT", "MIT"));
        assert!(!compatible("Apache-2.0", "MIT"));
        assert_eq!(from_cargo_toml("[package]\nname='x'\nlicense = \"Unlicense/MIT\"\n").unwrap(), "Unlicense OR MIT");
        assert_eq!(from_package_json(r#"{"license":"ISC"}"#).unwrap(), "ISC");
        assert_eq!(from_pyproject("[project]\nlicense = {text = \"BSD-3-Clause\"}\n").unwrap(), "BSD-3-Clause");
        let c = combine(Some(("MIT OR Apache-2.0".into(), "Cargo.toml".into())), vec![("LICENSE-MIT".into(), "MIT", 0.9), ("LICENSE-APACHE".into(), "Apache-2.0", 0.9)]);
        assert_eq!(c.confidence, Some(0.95));
        let c = combine(None, vec![("COPYING".into(), "GPL-3.0", 0.9)]);
        assert_eq!(c.copyleft, Some(true));
    }
}
