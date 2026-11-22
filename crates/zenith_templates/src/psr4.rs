//! The PHP namespace a directory maps to under Composer's PSR-4 autoloading
//! (`autoload.psr-4` and `autoload-dev.psr-4` in the nearest `composer.json`).
//! Candidate for `zenith_core::detect::composer` (plan 15) once a second crate
//! needs it; `zenith_refactor::derive_psr4_namespace` is still a heuristic.

use anyhow::Result;
use fs::Fs;
use serde_json::Value;
use std::path::Path;
use util::ResultExt as _;

/// A namespace prefix (no trailing `\`) and a directory relative to the
/// `composer.json` folder (`/`-separated, `""` for that folder).
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub prefix: String,
    pub dir: String,
}

/// `autoload` mappings first, then `autoload-dev`; a prefix may list several
/// directories.
pub fn parse(composer_json: &str) -> Result<Vec<Mapping>> {
    let json: Value = serde_json::from_str(composer_json)?;
    let mut mappings = Vec::new();
    for section in ["autoload", "autoload-dev"] {
        let Some(map) = json
            .get(section)
            .and_then(|section| section.get("psr-4"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (prefix, dirs) in map {
            let dirs = match dirs {
                Value::String(dir) => vec![dir.as_str()],
                Value::Array(dirs) => dirs.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for dir in dirs {
                mappings.push(Mapping {
                    prefix: prefix.trim_matches('\\').to_string(),
                    dir: normalize(dir),
                });
            }
        }
    }
    Ok(mappings)
}

fn normalize(dir: &str) -> String {
    let dir = dir.trim().replace('\\', "/");
    let mut dir = dir.as_str();
    while let Some(rest) = dir.strip_prefix("./") {
        dir = rest;
    }
    let dir = dir.trim_end_matches('/');
    if dir == "." {
        String::new()
    } else {
        dir.to_string()
    }
}

/// The namespace for `dir` (relative to the `composer.json` folder): the most
/// specific mapped directory containing it wins, the first listed on a tie.
pub fn namespace_for(mappings: &[Mapping], dir: &str) -> Option<String> {
    let dir = normalize(dir);
    let mut best: Option<(&Mapping, &str)> = None;
    for mapping in mappings {
        let rest = if mapping.dir.is_empty() {
            dir.as_str()
        } else if dir == mapping.dir {
            ""
        } else if let Some(rest) = dir
            .strip_prefix(mapping.dir.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
        {
            rest
        } else {
            continue;
        };
        if best.is_none_or(|(known, _)| mapping.dir.len() > known.dir.len()) {
            best = Some((mapping, rest));
        }
    }
    let (mapping, rest) = best?;
    let parts = std::iter::once(mapping.prefix.as_str())
        .chain(rest.split('/'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    Some(parts.join("\\"))
}

/// Walks up from `dir` to `root` and asks each `composer.json` on the way; the
/// nearest one that maps `dir` answers. `None` when none does.
pub async fn resolve(fs: &dyn Fs, root: &Path, dir: &Path) -> Option<String> {
    for folder in dir.ancestors() {
        if !folder.starts_with(root) {
            break;
        }
        let Ok(text) = fs.load(&folder.join("composer.json")).await else {
            continue;
        };
        let Some(mappings) = parse(&text).log_err() else {
            continue;
        };
        let relative = dir
            .strip_prefix(folder)
            .ok()?
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if let Some(namespace) = namespace_for(&mappings, &relative) {
            return Some(namespace);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use util::path;

    const COMPOSER: &str = r#"{
        "autoload": {
            "psr-4": {
                "App\\": "app/",
                "Acme\\Billing\\": ["src/Billing", "lib/billing/"],
                "Acme\\": "./src"
            }
        },
        "autoload-dev": {
            "psr-4": { "Tests\\": "tests/", "Database\\Factories\\": "database/factories/" }
        }
    }"#;

    #[test]
    fn test_parse_reads_both_sections_and_lists() {
        let mappings = parse(COMPOSER).unwrap();
        let pairs = mappings
            .iter()
            .map(|m| (m.prefix.as_str(), m.dir.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            [
                ("App", "app"),
                ("Acme\\Billing", "src/Billing"),
                ("Acme\\Billing", "lib/billing"),
                ("Acme", "src"),
                ("Tests", "tests"),
                ("Database\\Factories", "database/factories"),
            ]
        );
        assert!(parse("{}").unwrap().is_empty());
        assert!(parse("{").is_err());
    }

    #[test]
    fn test_namespace_for_prefers_the_most_specific_directory() {
        let mappings = parse(COMPOSER).unwrap();
        let ns = |dir| namespace_for(&mappings, dir);
        assert_eq!(ns("app").as_deref(), Some("App"));
        assert_eq!(
            ns("app/Http/Controllers").as_deref(),
            Some("App\\Http\\Controllers")
        );
        assert_eq!(
            ns("src/Billing/Invoices").as_deref(),
            Some("Acme\\Billing\\Invoices")
        );
        assert_eq!(ns("lib/billing").as_deref(), Some("Acme\\Billing"));
        assert_eq!(ns("src/Shipping").as_deref(), Some("Acme\\Shipping"));
        assert_eq!(ns("tests/Unit").as_deref(), Some("Tests\\Unit"));
        assert_eq!(
            ns("database/factories").as_deref(),
            Some("Database\\Factories")
        );
        // A sibling whose name only starts with a mapped directory is not inside it.
        assert_eq!(ns("application"), None);
        assert_eq!(ns(""), None);
        assert_eq!(ns("resources/views"), None);
    }

    #[test]
    fn test_an_empty_prefix_or_root_directory_is_a_fallback() {
        let mappings = parse(r#"{"autoload":{"psr-4":{"": "src/", "Lib\\": ""}}}"#).unwrap();
        assert_eq!(namespace_for(&mappings, "src/Foo").as_deref(), Some("Foo"));
        assert_eq!(namespace_for(&mappings, "src").as_deref(), Some(""));
        assert_eq!(
            namespace_for(&mappings, "other/x").as_deref(),
            Some("Lib\\other\\x")
        );
    }

    #[gpui::test]
    async fn test_resolve_uses_the_nearest_composer_json_that_maps_the_dir(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "composer.json": COMPOSER,
                "app": { "Models": {} },
                "packages": {
                    "geo": {
                        "composer.json": r#"{"autoload":{"psr-4":{"Geo\\": "src/"}}}"#,
                        "src": { "Maps": {} },
                        "docs": {},
                    }
                },
                "plain": {},
            }),
        )
        .await;
        let root = Path::new(path!("/root"));
        let ns = async |dir: &str| super::resolve(fs.as_ref(), root, Path::new(dir)).await;
        assert_eq!(
            ns(path!("/root/app/Models")).await.as_deref(),
            Some("App\\Models")
        );
        assert_eq!(
            ns(path!("/root/packages/geo/src/Maps")).await.as_deref(),
            Some("Geo\\Maps")
        );
        // Not mapped by the package: the project's composer.json is asked next.
        assert_eq!(ns(path!("/root/packages/geo/docs")).await, None);
        assert_eq!(ns(path!("/root/plain")).await, None);

        let empty = fs::FakeFs::new(cx.executor());
        empty
            .insert_tree(path!("/bare"), json!({ "src": {} }))
            .await;
        assert_eq!(
            super::resolve(
                empty.as_ref(),
                Path::new(path!("/bare")),
                Path::new(path!("/bare/src"))
            )
            .await,
            None
        );
    }
}
