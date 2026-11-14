//! What gets indexed: language by file name, and the skip rules (vendored, generated, lockfiles,
//! fixtures, binaries, huge files).

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    C,
    Cpp,
    Bash,
    Java,
    Markdown,
    Text,
    /// Indexed by line windows only (no grammar compiled in).
    Other(&'static str),
}

impl Lang {
    pub fn name(&self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Go => "go",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Bash => "bash",
            Lang::Java => "java",
            Lang::Markdown => "markdown",
            Lang::Text => "text",
            Lang::Other(n) => n,
        }
    }

    pub fn has_grammar(&self) -> bool {
        !matches!(self, Lang::Markdown | Lang::Text | Lang::Other(_))
    }

    /// Fence tag for markdown output.
    pub fn fence(&self) -> &'static str {
        match self {
            Lang::Text => "",
            l => l.name(),
        }
    }
}

pub fn fence_for(language: Option<&str>) -> &str {
    match language {
        Some("text") | None => "",
        Some(l) => l,
    }
}

/// Language of an indexable file, or None (= not indexed).
pub fn detect(path: &str) -> Option<Lang> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let lower = file.to_ascii_lowercase();
    match lower.as_str() {
        "makefile" | "gnumakefile" => return Some(Lang::Other("make")),
        "dockerfile" | "containerfile" => return Some(Lang::Other("dockerfile")),
        "cmakelists.txt" => return Some(Lang::Other("cmake")),
        "justfile" => return Some(Lang::Other("just")),
        "build" | "build.bazel" | "workspace" => return Some(Lang::Other("starlark")),
        _ => {}
    }
    if lower.starts_with("readme") && !lower.contains('.') {
        return Some(Lang::Text);
    }
    let ext = Path::new(&lower).extension()?.to_str()?;
    Some(match ext {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "go" => Lang::Go,
        "c" | "h" => Lang::C,
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "inl" | "cu" | "cuh" => Lang::Cpp,
        "sh" | "bash" | "zsh" => Lang::Bash,
        "java" => Lang::Java,
        "md" | "markdown" | "mdx" => Lang::Markdown,
        "txt" | "rst" | "adoc" => Lang::Text,
        "toml" => Lang::Other("toml"),
        "yaml" | "yml" => Lang::Other("yaml"),
        "zig" => Lang::Other("zig"),
        "kt" | "kts" => Lang::Other("kotlin"),
        "swift" => Lang::Other("swift"),
        "rb" => Lang::Other("ruby"),
        "php" => Lang::Other("php"),
        "cs" => Lang::Other("csharp"),
        "scala" => Lang::Other("scala"),
        "lua" => Lang::Other("lua"),
        "hs" => Lang::Other("haskell"),
        "ml" | "mli" => Lang::Other("ocaml"),
        "ex" | "exs" => Lang::Other("elixir"),
        "erl" => Lang::Other("erlang"),
        "clj" | "cljs" => Lang::Other("clojure"),
        "dart" => Lang::Other("dart"),
        "nim" => Lang::Other("nim"),
        "jl" => Lang::Other("julia"),
        "r" => Lang::Other("r"),
        "sql" => Lang::Other("sql"),
        "proto" => Lang::Other("protobuf"),
        "graphql" | "gql" => Lang::Other("graphql"),
        "nix" => Lang::Other("nix"),
        "vue" => Lang::Other("vue"),
        "svelte" => Lang::Other("svelte"),
        "css" | "scss" => Lang::Other("css"),
        "html" | "htm" => Lang::Other("html"),
        "cmake" => Lang::Other("cmake"),
        "ps1" => Lang::Other("powershell"),
        "fish" => Lang::Other("fish"),
        "wgsl" | "glsl" | "hlsl" | "metal" => Lang::Other("shader"),
        "s" | "asm" => Lang::Other("asm"),
        _ => return None,
    })
}

const SKIP_DIRS: &[&str] = &[
    "vendor",
    "vendored",
    "third_party",
    "third-party",
    "thirdparty",
    "node_modules",
    "bower_components",
    "generated",
    "__generated__",
    "testdata",
    "test-data",
    "test_data",
    "fixtures",
    "__fixtures__",
    "__snapshots__",
    "dist",
    ".git",
    ".github", // workflows are noise for code search
    ".venv",
    "venv",
    "site-packages",
];

const LOCKFILES: &[&str] = &[
    "cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "poetry.lock",
    "pipfile.lock",
    "uv.lock",
    "go.sum",
    "gemfile.lock",
    "composer.lock",
    "flake.lock",
    "mix.lock",
    "pubspec.lock",
    "podfile.lock",
];

/// Why a path is skipped before reading it, or None to index.
pub fn skip_reason(path: &str, size: u64, max_bytes: u64) -> Option<&'static str> {
    if size > max_bytes {
        return Some("too_large");
    }
    let lower = path.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('/').collect();
    let file = parts.pop().unwrap_or("");
    if parts.iter().any(|d| SKIP_DIRS.contains(d)) {
        return Some("vendored_or_generated_dir");
    }
    if LOCKFILES.contains(&file) {
        return Some("lockfile");
    }
    if file.starts_with("license")
        || file.starts_with("licence")
        || file.starts_with("copying")
        || file == "notice"
        || file == "unlicense"
    {
        return Some("license_text");
    }
    let generated = file.ends_with(".min.js")
        || file.ends_with(".min.css")
        || file.ends_with(".min.mjs")
        || file.ends_with(".bundle.js")
        || file.ends_with(".map")
        || file.ends_with(".pb.go")
        || file.ends_with(".pb.cc")
        || file.ends_with(".pb.h")
        || file.ends_with("_pb2.py")
        || file.ends_with("_pb2_grpc.py")
        || file.ends_with(".g.dart")
        || file.ends_with(".freezed.dart")
        || file.contains(".generated.")
        || file.ends_with("_generated.go")
        || file.ends_with("_generated.rs")
        || file.ends_with(".snap");
    if generated {
        return Some("generated");
    }
    match detect(path) {
        None => return Some("unsupported_type"),
        // Plain text is prose only at the top level or under docs/ (elsewhere it is usually data:
        // benchmark haystacks, corpora, expected outputs).
        Some(Lang::Text) if !(parts.is_empty() || parts.iter().any(|d| matches!(*d, "doc" | "docs" | "documentation"))) => {
            return Some("data_text");
        }
        _ => {}
    }
    None
}

/// Content checks after reading: binary (NUL), not UTF-8, generated header, minified.
pub fn content_skip_reason(path: &str, bytes: &[u8]) -> Option<&'static str> {
    // Any NUL: binary (and Postgres text cannot hold NUL anyway).
    if bytes.contains(&0) {
        return Some("binary");
    }
    let Ok(text) = std::str::from_utf8(bytes) else { return Some("not_utf8") };
    let first: String = text.lines().take(5).collect::<Vec<_>>().join("\n").to_ascii_lowercase();
    if first.contains("@generated")
        || first.contains("do not edit")
        || first.contains("code generated by")
        || first.contains("autogenerated by")
        || first.contains("automatically generated")
    {
        return Some("generated");
    }
    let lines = text.lines().count().max(1);
    let lower = path.to_ascii_lowercase();
    let minifiable = [".js", ".mjs", ".cjs", ".css", ".json", ".html"].iter().any(|e| lower.ends_with(e));
    if minifiable && text.len() / lines > 400 {
        return Some("minified");
    }
    if text.len() > 20_000 && text.len() / lines > 1000 {
        return Some("minified");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_skips() {
        assert_eq!(detect("src/lib.rs"), Some(Lang::Rust));
        assert_eq!(detect("web/App.tsx"), Some(Lang::Tsx));
        assert_eq!(detect("README"), Some(Lang::Text));
        assert_eq!(detect("logo.png"), None);
        assert!(skip_reason("vendor/x/y.go", 10, 1 << 19).is_some());
        assert!(skip_reason("Cargo.lock", 10, 1 << 19).is_some());
        assert!(skip_reason("a/b.min.js", 10, 1 << 19).is_some());
        assert!(skip_reason("src/big.rs", 600_000, 512 * 1024).is_some());
        assert!(skip_reason("tests/fixtures/a.rs", 10, 1 << 19).is_some());
        assert!(skip_reason("src/lib.rs", 10, 1 << 19).is_none());
        assert!(skip_reason("benchmarks/haystacks/a.txt", 10, 1 << 19).is_some());
        assert!(skip_reason("docs/guide.txt", 10, 1 << 19).is_none());
        assert!(skip_reason("CHANGELOG.txt", 10, 1 << 19).is_none());
        assert_eq!(content_skip_reason("a.rs", b"fn x() {}\0"), Some("binary"));
        assert_eq!(content_skip_reason("a.rs", b"// @generated by foo\nfn x() {}"), Some("generated"));
        assert_eq!(content_skip_reason("a.rs", b"fn x() {}\n"), None);
    }
}
