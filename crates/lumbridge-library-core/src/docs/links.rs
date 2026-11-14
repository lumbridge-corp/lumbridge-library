//! `[[wikilinks]]`: `[[target]]`, `[[target|alias]]`, `[[target#heading]]`, `![[embed]]`.
//! Links inside fenced code blocks and inline code spans are ignored.

use super::chunk::{fence_closes, fence_open};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// Note name or path, without heading or alias (`NVIDIA GB10`, `References/NVIDIA GB10`).
    pub target: String,
    pub heading: Option<String>,
    pub alias: Option<String>,
    pub embed: bool,
    /// What is stored in `kb.links.dst_text`: target plus `#heading`, no alias.
    pub dst_text: String,
}

impl WikiLink {
    pub fn kind(&self) -> &'static str {
        if self.embed { "embed" } else { "wikilink" }
    }
}

fn parse_inner(inner: &str, embed: bool) -> Option<WikiLink> {
    let (target_part, alias) = match inner.split_once('|') {
        Some((t, a)) => (t, Some(a.trim().to_string()).filter(|a| !a.is_empty())),
        None => (inner, None),
    };
    let target_part = target_part.trim();
    let (target, heading) = match target_part.split_once('#') {
        Some((t, h)) => (t.trim(), Some(h.trim().to_string()).filter(|h| !h.is_empty())),
        None => (target_part, None),
    };
    let target = target.strip_suffix(".md").unwrap_or(target).trim();
    if target.is_empty() {
        return None; // [[#heading]] points into the same note
    }
    let dst_text = match &heading {
        Some(h) => format!("{target}#{h}"),
        None => target.to_string(),
    };
    Some(WikiLink { target: target.to_string(), heading, alias, embed, dst_text })
}

fn scan_line(line: &str, out: &mut Vec<WikiLink>) {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'`' {
            // Skip an inline code span delimited by the same number of backticks.
            let n = b[i..].iter().take_while(|&&c| c == b'`').count();
            let ticks = &line[i..i + n];
            match line[i + n..].find(ticks) {
                Some(end) => i += n + end + n,
                None => i += n,
            }
            continue;
        }
        if b[i] == b'[' && b.get(i + 1) == Some(&b'[') {
            let embed = i > 0 && b[i - 1] == b'!';
            if let Some(end) = line[i + 2..].find("]]") {
                let inner = &line[i + 2..i + 2 + end];
                if !inner.contains('[')
                    && let Some(l) = parse_inner(inner, embed)
                {
                    out.push(l);
                }
                i += 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
}

/// All wikilinks in a markdown body, in document order (duplicates kept).
pub fn parse_wikilinks(body: &str) -> Vec<WikiLink> {
    let mut out = Vec::new();
    let mut fence = None;
    for line in body.lines() {
        if let Some(open) = fence {
            if fence_closes(line, open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_open(line) {
            fence = Some(open);
            continue;
        }
        if line.contains("[[") {
            scan_line(line, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forms() {
        let body = "See [[NVIDIA GB10]] and [[References/vLLM|the engine]], [[GB10#Memory]], ![[chart.png]] ![[Note#Sec|x]].\n\
                    `[[not a link]]` and ``a [[nope]] b``\n```\n[[in code]]\n```\n[[#local]] [[]] [[a.md]]";
        let l = parse_wikilinks(body);
        let t: Vec<&str> = l.iter().map(|l| l.dst_text.as_str()).collect();
        assert_eq!(t, vec!["NVIDIA GB10", "References/vLLM", "GB10#Memory", "chart.png", "Note#Sec", "a"]);
        assert_eq!(l[1].alias.as_deref(), Some("the engine"));
        assert_eq!(l[2].heading.as_deref(), Some("Memory"));
        assert_eq!(l[2].target, "GB10");
        assert!(l[3].embed && l[4].embed && !l[0].embed);
        assert_eq!(l[4].kind(), "embed");
    }
}
