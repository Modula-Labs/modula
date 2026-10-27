//! Guessing an app's stack from its source, for the dashboard's service icon.
//!
//! Cheap file checks only (no parsing beyond a substring search of a few
//! manifest files, each read up to 256 KB). The most specific match wins:
//! a framework over its language.

use std::path::Path;

const MAX_MANIFEST: u64 = 256 * 1024;

/// (manifest, needle, stack) — first match wins, so frameworks come first.
#[rustfmt::skip]
const RULES: &[(&str, &str, &str)] = &[
    ("package.json", "\"next\"", "nextjs"),
    ("package.json", "\"nuxt\"", "nuxt"),
    ("package.json", "\"@remix-run/", "remix"),
    ("package.json", "\"@sveltejs/kit\"", "svelte"),
    ("package.json", "\"astro\"", "astro"),
    ("package.json", "\"@nestjs/core\"", "nestjs"),
    ("package.json", "\"express\"", "express"),
    ("package.json", "\"fastify\"", "fastify"),
    ("package.json", "\"hono\"", "hono"),
    ("package.json", "\"vue\"", "vue"),
    ("package.json", "\"react\"", "react"),
    ("package.json", "\"vite\"", "vite"),
    ("requirements.txt", "django", "django"),
    ("pyproject.toml", "django", "django"),
    ("requirements.txt", "fastapi", "fastapi"),
    ("pyproject.toml", "fastapi", "fastapi"),
    ("requirements.txt", "flask", "flask"),
    ("pyproject.toml", "flask", "flask"),
    ("Gemfile", "rails", "rails"),
    ("composer.json", "laravel/framework", "laravel"),
    ("Cargo.toml", "axum", "rust"),
    ("go.mod", "gin-gonic/gin", "go"),
];

/// (file that marks a language, stack) when no framework matched.
#[rustfmt::skip]
const LANGUAGES: &[(&str, &str)] = &[
    ("bun.lockb", "bun"),
    ("bun.lock", "bun"),
    ("deno.json", "deno"),
    ("package.json", "nodejs"),
    ("Cargo.toml", "rust"),
    ("go.mod", "go"),
    ("requirements.txt", "python"),
    ("pyproject.toml", "python"),
    ("Pipfile", "python"),
    ("Gemfile", "ruby"),
    ("composer.json", "php"),
    ("pom.xml", "java"),
    ("build.gradle", "java"),
    ("build.gradle.kts", "kotlin"),
    ("mix.exs", "elixir"),
    ("Package.swift", "swift"),
    ("index.html", "html"),
];

/// A short stack id (e.g. `nextjs`, `python`), or empty when unknown.
pub fn detect(dir: &Path) -> String {
    for (file, needle, stack) in RULES {
        if read_small(&dir.join(file)).is_some_and(|s| s.to_ascii_lowercase().contains(needle)) {
            return (*stack).to_owned();
        }
    }
    LANGUAGES
        .iter()
        .find(|(file, _)| dir.join(file).symlink_metadata().is_ok_and(|m| m.is_file()))
        .map(|(_, stack)| (*stack).to_owned())
        .unwrap_or_default()
}

fn read_small(path: &Path) -> Option<String> {
    let meta = path.symlink_metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frameworks_beat_languages() {
        let dir = std::env::temp_dir().join(format!("modula-stack-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect(&dir), "");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"react":"19"}}"#,
        )
        .unwrap();
        assert_eq!(detect(&dir), "react");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"next":"15","react":"19"}}"#,
        )
        .unwrap();
        assert_eq!(detect(&dir), "nextjs");
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(detect(&dir), "nodejs");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
