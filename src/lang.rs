//! Détection de langage par extension de fichier.
//! Sert à router vers le bon parseur tree-sitter (Étape 2) et à filtrer
//! les fichiers pertinents pour le contexte code.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Lang {
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    CSharp,
    Rust,
    Cpp,
    Markdown,
    Sql,
    Json,
    Toml,
    Yaml,
    Html,
    Css,
    Other,
}

impl Lang {
    /// Détecte le langage depuis le nom de fichier (extension).
    pub fn from_path(path: &str) -> Lang {
        let lower = path.to_ascii_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        match ext {
            "ts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" => Lang::JavaScript,
            "jsx" => Lang::Jsx,
            "py" | "pyi" => Lang::Python,
            "cs" => Lang::CSharp,
            "rs" => Lang::Rust,
            // C et C++ : une seule grammaire (le C++ en est un sur-ensemble pour notre usage).
            "h" | "hpp" | "hh" | "hxx" | "inl" | "ipp" | "tpp" | "c" | "cc" | "cpp" | "cxx" | "c++" => Lang::Cpp,
            "md" | "mdx" | "markdown" => Lang::Markdown,
            "sql" => Lang::Sql,
            "json" => Lang::Json,
            "toml" => Lang::Toml,
            "yml" | "yaml" => Lang::Yaml,
            "html" | "htm" => Lang::Html,
            "css" | "scss" | "sass" | "less" => Lang::Css,
            _ => Lang::Other,
        }
    }

    /// Vrai si le fichier porte du code/doc à indexer pour le contexte
    /// (on ignore les binaires, images, etc.).
    pub fn is_indexable(&self) -> bool {
        !matches!(self, Lang::Other)
    }

    /// Vrai si un parseur tree-sitter de symboles existe (Étape 2).
    pub fn has_parser(&self) -> bool {
        matches!(
            self,
            Lang::TypeScript
                | Lang::Tsx
                | Lang::JavaScript
                | Lang::Jsx
                | Lang::Python
                | Lang::CSharp
                | Lang::Rust
                | Lang::Cpp
                | Lang::Markdown
        )
    }

    /// Encodage compact pour l'atlas (rkyv).
    pub fn as_u8(&self) -> u8 {
        match self {
            Lang::TypeScript => 0,
            Lang::Tsx => 1,
            Lang::JavaScript => 2,
            Lang::Jsx => 3,
            Lang::Python => 4,
            Lang::CSharp => 5,
            Lang::Rust => 6,
            Lang::Markdown => 7,
            Lang::Sql => 8,
            Lang::Json => 9,
            Lang::Toml => 10,
            Lang::Yaml => 11,
            Lang::Html => 12,
            Lang::Css => 13,
            Lang::Other => 14,
            Lang::Cpp => 15,
        }
    }

    pub fn from_u8(b: u8) -> Lang {
        match b {
            0 => Lang::TypeScript,
            1 => Lang::Tsx,
            2 => Lang::JavaScript,
            3 => Lang::Jsx,
            4 => Lang::Python,
            5 => Lang::CSharp,
            6 => Lang::Rust,
            7 => Lang::Markdown,
            8 => Lang::Sql,
            9 => Lang::Json,
            10 => Lang::Toml,
            11 => Lang::Yaml,
            12 => Lang::Html,
            13 => Lang::Css,
            15 => Lang::Cpp,
            _ => Lang::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Jsx => "jsx",
            Lang::Python => "python",
            Lang::CSharp => "csharp",
            Lang::Rust => "rust",
            Lang::Cpp => "cpp",
            Lang::Markdown => "markdown",
            Lang::Sql => "sql",
            Lang::Json => "json",
            Lang::Toml => "toml",
            Lang::Yaml => "yaml",
            Lang::Html => "html",
            Lang::Css => "css",
            Lang::Other => "other",
        }
    }
}
