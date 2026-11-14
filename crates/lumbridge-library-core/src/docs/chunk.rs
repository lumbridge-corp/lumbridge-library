//! Heading-aware markdown chunker: ≈900-token chunks with ≈15% overlap, fenced code kept whole
//! when possible, each chunk tagged with its `H1 > H2 > H3` breadcrumb.

use crate::rank::approx_tokens;

pub const TARGET_TOKENS: usize = 900;
pub const OVERLAP_TOKENS: usize = TARGET_TOKENS * 15 / 100;
/// A heading starts a new chunk once the current one has at least this much new text.
pub const MIN_SECTION_TOKENS: usize = TARGET_TOKENS / 4;
/// A fenced code block up to this size stays in one chunk even though it exceeds the target.
pub const MAX_CODE_TOKENS: usize = TARGET_TOKENS * 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ord: i32,
    pub heading: String,
    pub content: String,
    pub tokens: i32,
    /// 1-based inclusive line range of the body (frontmatter removed, as `docs::read` counts lines)
    /// this chunk covers, overlap included.
    pub line_start: i32,
    pub line_end: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Heading,
    Code,
    Text,
}

#[derive(Debug, Clone)]
struct Block {
    text: String,
    tokens: usize,
    kind: BlockKind,
    heading: String,
    /// 1-based inclusive body lines.
    start: usize,
    end: usize,
}

impl Block {
    fn new(text: String, kind: BlockKind, heading: &str, start: usize, end: usize) -> Self {
        Self { tokens: approx_tokens(&text), text, kind, heading: heading.to_string(), start, end }
    }
}

/// `(fence char, length)` if `line` opens a fenced code block.
pub(crate) fn fence_open(line: &str) -> Option<(char, usize)> {
    let t = line.trim_end_matches(['\n', '\r']);
    let indent = t.len() - t.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let t = &t[indent..];
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|&x| x == c).count();
    if n < 3 || (c == '`' && t[n..].contains('`')) {
        return None;
    }
    Some((c, n))
}

pub(crate) fn fence_closes(line: &str, open: (char, usize)) -> bool {
    let t = line.trim();
    let n = t.chars().take_while(|&x| x == open.0).count();
    n >= open.1 && t.chars().all(|x| x == open.0)
}

/// `(level, text)` for an ATX heading line.
fn heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_end_matches(['\n', '\r']);
    let indent = t.len() - t.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let t = &t[indent..];
    let level = t.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim().to_string();
    Some((level, text))
}

fn parse_blocks(body: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut crumb = String::new();
    let mut para = String::new();
    let mut para_lines = (0usize, 0usize);
    let mut code = String::new();
    let mut code_start = 0usize;
    let mut fence: Option<(char, usize)> = None;
    let mut last = 0usize;

    let flush_para = |para: &mut String, lines: (usize, usize), blocks: &mut Vec<Block>, crumb: &str| {
        let t = para.trim_end();
        if !t.trim().is_empty() {
            blocks.push(Block::new(t.to_string(), BlockKind::Text, crumb, lines.0, lines.1));
        }
        para.clear();
    };

    for (i, line) in body.split_inclusive('\n').enumerate() {
        let n = i + 1;
        last = n;
        if let Some(open) = fence {
            code.push_str(line);
            if fence_closes(line, open) {
                blocks.push(Block::new(code.trim_end().to_string(), BlockKind::Code, &crumb, code_start, n));
                code.clear();
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_open(line) {
            flush_para(&mut para, para_lines, &mut blocks, &crumb);
            fence = Some(open);
            code_start = n;
            code.push_str(line);
            continue;
        }
        if let Some((level, text)) = heading(line) {
            flush_para(&mut para, para_lines, &mut blocks, &crumb);
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, text));
            crumb = stack.iter().map(|(_, t)| t.as_str()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" > ");
            blocks.push(Block::new(line.trim_end().to_string(), BlockKind::Heading, &crumb, n, n));
            continue;
        }
        if line.trim().is_empty() {
            flush_para(&mut para, para_lines, &mut blocks, &crumb);
            continue;
        }
        if para.is_empty() {
            para_lines.0 = n;
        }
        para_lines.1 = n;
        para.push_str(line);
    }
    flush_para(&mut para, para_lines, &mut blocks, &crumb);
    if !code.trim().is_empty() {
        blocks.push(Block::new(code.trim_end().to_string(), BlockKind::Code, &crumb, code_start, last));
    }
    blocks
}

/// Pair each piece (a trimmed, in-order substring of `text`) with its 0-based line offsets in `text`.
fn locate(text: &str, pieces: Vec<String>) -> Vec<(String, usize, usize)> {
    let mut cursor = 0usize;
    let mut line = 0usize;
    let mut out = Vec::with_capacity(pieces.len());
    for p in pieces {
        match text[cursor..].find(p.as_str()) {
            Some(pos) => {
                let start = line + text[cursor..cursor + pos].matches('\n').count();
                let end = start + p.matches('\n').count();
                cursor += pos + p.len();
                line = end;
                out.push((p, start, end));
            }
            None => out.push((p, line, line)),
        }
    }
    out
}

/// Split text into pieces of at most `max` tokens: by line, then sentence, then hard by chars.
/// Returns (piece, first line offset, last line offset), offsets 0-based within `text`.
fn split_text(text: &str, max: usize) -> Vec<(String, usize, usize)> {
    let max_chars = max * 4;
    let mut units: Vec<&str> = Vec::new();
    for line in text.split_inclusive('\n') {
        if approx_tokens(line) <= max {
            units.push(line);
            continue;
        }
        let mut start = 0;
        for (i, _) in line.match_indices(". ") {
            units.push(&line[start..i + 2]);
            start = i + 2;
        }
        units.push(&line[start..]);
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for u in units {
        if approx_tokens(u) > max {
            if !cur.trim().is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.clear();
            let chars: Vec<char> = u.chars().collect();
            for piece in chars.chunks(max_chars) {
                out.push(piece.iter().collect());
            }
            continue;
        }
        if !cur.is_empty() && approx_tokens(&cur) + approx_tokens(u) > max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(u);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    let pieces = out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    locate(text, pieces)
}

/// Split an oversized fenced block by lines, re-opening and closing the fence in every piece.
/// Returns (piece, first line offset, last line offset), offsets 0-based within `text`.
fn split_code(text: &str, max: usize) -> Vec<(String, usize, usize)> {
    let mut lines = text.lines();
    let open = lines.next().unwrap_or("```").to_string();
    let open_fence = fence_open(&open).unwrap_or(('`', 3));
    let close: String = std::iter::repeat_n(open_fence.0, open_fence.1).collect();
    let mut inner: Vec<&str> = lines.collect();
    let closed = inner.last().is_some_and(|l| fence_closes(l, open_fence));
    if closed {
        inner.pop();
    }
    let overhead = approx_tokens(&open) + approx_tokens(&close) + 1;
    let budget = max.saturating_sub(overhead).max(1);
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut cur = String::new();
    let mut span: Option<(usize, usize)> = None;
    // Inner line i sits at offset i + 1 (the opening fence is offset 0).
    let pieces = inner.iter().enumerate().flat_map(|(i, l)| {
        let chars: Vec<char> = l.chars().collect();
        if chars.is_empty() {
            return vec![(i + 1, "\n".to_string())];
        }
        chars.chunks(budget * 4).map(|p| (i + 1, p.iter().collect::<String>() + "\n")).collect()
    });
    for (li, l) in pieces {
        if let Some((a, b)) = span
            && !cur.is_empty()
            && approx_tokens(&cur) + approx_tokens(&l) > budget
        {
            out.push((format!("{open}\n{}{close}", cur), a, b));
            cur.clear();
            span = None;
        }
        span = Some((span.map_or(li, |s| s.0), li));
        cur.push_str(&l);
    }
    if let (false, Some((a, b))) = (cur.is_empty(), span) {
        out.push((format!("{open}\n{}{close}", cur), a, b));
    }
    if let Some(first) = out.first_mut() {
        first.1 = 0;
    }
    if let (true, Some(last)) = (closed, out.last_mut()) {
        last.2 = inner.len() + 1;
    }
    out
}

/// Break oversized blocks so every block (except mid-size code) fits the target.
fn normalise_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        let pieces = match b.kind {
            BlockKind::Code if b.tokens > MAX_CODE_TOKENS => split_code(&b.text, TARGET_TOKENS),
            BlockKind::Text | BlockKind::Heading if b.tokens > TARGET_TOKENS => split_text(&b.text, TARGET_TOKENS),
            _ => {
                out.push(b);
                continue;
            }
        };
        let kind = if b.kind == BlockKind::Code { BlockKind::Code } else { BlockKind::Text };
        out.extend(pieces.into_iter().map(|(t, s, e)| {
            Block::new(t, kind, &b.heading, (b.start + s).min(b.end), (b.start + e).min(b.end))
        }));
    }
    out
}

/// Trailing text of a finished chunk, re-used at the start of the next one.
fn overlap_tail(blocks: &[Block]) -> Vec<Block> {
    let mut tail: Vec<Block> = Vec::new();
    let mut used = 0;
    for b in blocks.iter().rev() {
        if b.kind != BlockKind::Text {
            break;
        }
        if used + b.tokens <= OVERLAP_TOKENS {
            used += b.tokens;
            tail.push(b.clone());
            continue;
        }
        if tail.is_empty() {
            // Take the last words of a long paragraph.
            let budget_chars = (OVERLAP_TOKENS - used) * 4;
            let words: Vec<&str> = b.text.split_whitespace().collect();
            let mut take = Vec::new();
            let mut n = 0;
            for w in words.iter().rev() {
                if n + w.len() + 1 > budget_chars {
                    break;
                }
                n += w.len() + 1;
                take.push(*w);
            }
            take.reverse();
            if !take.is_empty() {
                // Where the taken words begin, for the line range.
                let mut pos = b.text.len();
                for w in take.iter().rev() {
                    pos = b.text[..pos].rfind(w).unwrap_or(pos);
                }
                let start = (b.start + b.text[..pos].matches('\n').count()).min(b.end);
                tail.push(Block::new(take.join(" "), BlockKind::Text, &b.heading, start, b.end));
            }
        }
        break;
    }
    tail.reverse();
    tail
}

/// Chunk a markdown body (frontmatter already removed). Pure; never splits inside a fenced code
/// block unless the block alone exceeds [`MAX_CODE_TOKENS`].
pub fn chunk_markdown(body: &str) -> Vec<Chunk> {
    let blocks = normalise_blocks(parse_blocks(body));
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut cur: Vec<Block> = Vec::new();
    let mut cur_tokens = 0usize;
    let mut new_tokens = 0usize;
    let mut first_new: Option<usize> = None;

    let finish = |cur: &[Block], first_new: Option<usize>, chunks: &mut Vec<Chunk>| {
        let Some(first) = first_new else { return };
        let content = cur.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
        let heading = cur[first].heading.clone();
        let line_start = cur.iter().map(|b| b.start).min().unwrap_or(1);
        let line_end = cur.iter().map(|b| b.end).max().unwrap_or(line_start);
        chunks.push(Chunk {
            ord: chunks.len() as i32,
            tokens: approx_tokens(&content) as i32,
            heading,
            content,
            line_start: line_start as i32,
            line_end: line_end as i32,
        });
    };

    for b in blocks {
        let heading_break = b.kind == BlockKind::Heading && new_tokens >= MIN_SECTION_TOKENS;
        let full = first_new.is_some() && cur_tokens + b.tokens > TARGET_TOKENS;
        if heading_break || full {
            finish(&cur, first_new, &mut chunks);
            let tail = if heading_break { Vec::new() } else { overlap_tail(&cur) };
            cur_tokens = tail.iter().map(|b| b.tokens).sum();
            cur = tail;
            new_tokens = 0;
            first_new = None;
        }
        if first_new.is_none() {
            first_new = Some(cur.len());
        }
        cur_tokens += b.tokens;
        new_tokens += b.tokens;
        cur.push(b);
    }
    finish(&cur, first_new, &mut chunks);
    chunks
}


#[cfg(test)]
mod tests {
    use super::*;

    fn para(words: usize, tag: &str) -> String {
        (0..words).map(|i| format!("{tag}{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn small_doc_is_one_chunk_with_breadcrumb() {
        let c = chunk_markdown("# GB10\n\n## Memory\n\n### Bandwidth\n\n273 GB/s LPDDR5X.\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].heading, "GB10");
        assert!(c[0].content.contains("273 GB/s"));
    }

    #[test]
    fn breadcrumbs_follow_heading_levels() {
        let body = format!(
            "# A\n\n{}\n\n## B\n\n{}\n\n### C\n\n{}\n\n## D\n\n{}\n",
            para(250, "a"),
            para(250, "b"),
            para(250, "c"),
            para(250, "d")
        );
        let c = chunk_markdown(&body);
        let heads: Vec<&str> = c.iter().map(|c| c.heading.as_str()).collect();
        assert_eq!(heads, vec!["A", "A > B", "A > B > C", "A > D"]);
    }

    #[test]
    fn long_section_splits_with_overlap_near_target() {
        let paras: Vec<String> = (0..30).map(|i| para(60, &format!("p{i}w"))).collect();
        let body = format!("# Long\n\n{}\n", paras.join("\n\n"));
        let c = chunk_markdown(&body);
        assert!(c.len() >= 3, "{}", c.len());
        for ch in &c {
            assert!(ch.tokens as usize <= TARGET_TOKENS + 10, "chunk too big: {}", ch.tokens);
            assert_eq!(ch.heading, "Long");
        }
        // Overlap: the last paragraph of chunk 0 starts chunk 1.
        let last_para = c[0].content.rsplit("\n\n").next().unwrap();
        assert!(c[1].content.starts_with(last_para));
    }

    #[test]
    fn code_fence_is_not_split() {
        let code: String = (0..150).map(|i| format!("let x{i} = {i}; // # not a heading\n")).collect();
        assert!(approx_tokens(&code) > TARGET_TOKENS && approx_tokens(&code) < MAX_CODE_TOKENS);
        let body = format!("# Code\n\n{}\n\n```rust\n{code}```\n\nafter\n", para(100, "t"));
        let c = chunk_markdown(&body);
        let with_code: Vec<&Chunk> = c.iter().filter(|c| c.content.contains("let x0 =")).collect();
        assert_eq!(with_code.len(), 1);
        let ch = with_code[0];
        assert!(ch.content.contains("let x149 ="));
        assert!(ch.content.contains("```rust\n"));
        assert!(c.iter().all(|c| c.heading == "Code"));
    }

    #[test]
    fn huge_code_fence_splits_with_fences_reopened() {
        let code: String = (0..2000).map(|i| format!("line {i} of a very long generated file\n")).collect();
        let body = format!("```txt\n{code}```\n");
        let c = chunk_markdown(&body);
        assert!(c.len() > 2);
        for ch in &c {
            assert!(ch.content.starts_with("```txt"), "{}", &ch.content[..20]);
            assert!(ch.content.trim_end().ends_with("```"));
        }
    }

    /// Every line of a chunk's content appears inside its `line_start..=line_end` body slice.
    fn assert_lines_cover(body: &str, chunks: &[Chunk]) {
        let lines: Vec<&str> = body.lines().collect();
        for c in chunks {
            assert!(c.line_start >= 1 && c.line_start <= c.line_end && c.line_end as usize <= lines.len(), "{c:?}");
            // Whitespace-normalised: an overlap tail re-joins the last words of a paragraph with spaces.
            let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
            let slice = flat(&lines[c.line_start as usize - 1..c.line_end as usize].join("\n"));
            for l in c.content.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("```")) {
                assert!(slice.contains(&flat(l)),"chunk {} line {l:?} not in lines {}..={}", c.ord, c.line_start, c.line_end);
            }
        }
    }

    #[test]
    fn line_ranges_match_body_lines() {
        let body = format!(
            "intro line\n\n# A\n\n{}\n\n## B\n\n{}\nsecond line of b\n\n```rust\nfn x() {{}}\n```\n\n### C\n\n{}\n",
            para(250, "a"),
            para(250, "b"),
            para(250, "c")
        );
        let c = chunk_markdown(&body);
        assert!(c.len() >= 3);
        assert_eq!(c[0].line_start, 1);
        assert_eq!(c.last().unwrap().line_end as usize, body.lines().count());
        assert_lines_cover(&body, &c);
        let b = c.iter().find(|c| c.heading == "A > B").unwrap();
        assert_eq!(body.lines().nth(b.line_start as usize - 1), Some("## B"));

        // Overlapping splits of a long section: ranges overlap and stay accurate.
        let paras: Vec<String> = (0..30).map(|i| para(60, &format!("p{i}w"))).collect();
        let body = format!("first\n# Long\n\n{}\n", paras.join("\n\n"));
        let c = chunk_markdown(&body);
        assert_lines_cover(&body, &c);
        assert!(c[1].line_start <= c[0].line_end, "overlap shares lines: {:?} {:?}", (c[0].line_start, c[0].line_end), (c[1].line_start, c[1].line_end));

        // Hard-split giant code fence and giant multi-line paragraph.
        let code: String = (0..2000).map(|i| format!("line {i} of a very long generated file\n")).collect();
        let body = format!("# Code\n\n```txt\n{code}```\n\nafter\n");
        let c = chunk_markdown(&body);
        assert_lines_cover(&body, &c);
        let lines: Vec<String> = (0..400).map(|i| format!("row {i} {}", para(8, "w"))).collect();
        let body = format!("# Rows\n{}\n", lines.join("\n"));
        let c = chunk_markdown(&body);
        assert!(c.len() >= 2);
        assert_lines_cover(&body, &c);
    }

    #[test]
    fn giant_paragraph_and_empty() {
        assert!(chunk_markdown("").is_empty());
        assert!(chunk_markdown("\n\n  \n").is_empty());
        let c = chunk_markdown(&"x".repeat(20_000));
        assert!(c.len() >= 5);
        assert!(c.iter().all(|c| c.tokens as usize <= TARGET_TOKENS + 1));
        assert_eq!(c.iter().map(|c| c.ord).collect::<Vec<_>>(), (0..c.len() as i32).collect::<Vec<_>>());
    }
}
