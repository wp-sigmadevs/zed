//! The template format (plan 20): a folder per template, named by its id, holding
//! `manifest.json` (name, extension, prompts) and `template.<extension>`, the body
//! with `${VARIABLES}`. Built-ins live in `assets/zenith/templates/`; the user's
//! and the project's folders replace them by id.

use anyhow::{Context as _, Result, bail};
use fs::Fs;
use futures::StreamExt as _;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use time::{OffsetDateTime, UtcOffset};
use util::ResultExt as _;

pub const CURSOR: &str = "CURSOR";

/// A value the user is asked for after the file name, as `${NAME}` in the body.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Prompt {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub default: String,
}

impl Prompt {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Deserialize)]
struct Manifest {
    name: String,
    extension: String,
    #[serde(default)]
    variables: Vec<Prompt>,
}

impl Manifest {
    fn parse(json: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(json).context("reading manifest.json")?;
        if manifest.name.trim().is_empty() {
            bail!("the template has no name");
        }
        if !valid_extension(&manifest.extension) {
            bail!("{:?} is not a file extension", manifest.extension);
        }
        if let Some(prompt) = manifest
            .variables
            .iter()
            .find(|prompt| !is_variable_name(&prompt.name) || prompt.name == CURSOR)
        {
            bail!("{:?} is not a variable name (A-Z, 0-9, _)", prompt.name);
        }
        Ok(manifest)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub extension: String,
    pub prompts: Vec<Prompt>,
    pub body: String,
}

impl Template {
    fn new(id: &str, manifest: Manifest, body: String) -> Self {
        Self {
            id: id.to_string(),
            name: manifest.name,
            extension: manifest.extension,
            prompts: manifest.variables,
            body,
        }
    }
}

/// The extension is appended to a typed name, so it must not add a separator.
fn valid_extension(extension: &str) -> bool {
    !extension.is_empty()
        && !extension.starts_with('.')
        && !extension.ends_with('.')
        && !extension.contains("..")
        && extension
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

pub fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

macro_rules! builtin {
    ($id:literal, $extension:literal) => {
        (
            $id,
            include_str!(concat!(
                "../../../assets/zenith/templates/",
                $id,
                "/manifest.json"
            )),
            include_str!(concat!(
                "../../../assets/zenith/templates/",
                $id,
                "/template.",
                $extension
            )),
        )
    };
}

/// Compiled in so the "+" popup works without reading the disk.
const BUILTIN: &[(&str, &str, &str)] = &[
    builtin!("php-file", "php"),
    builtin!("php-class", "php"),
    builtin!("php-interface", "php"),
    builtin!("php-trait", "php"),
    builtin!("php-enum", "php"),
    builtin!("javascript", "js"),
    builtin!("typescript", "ts"),
    builtin!("html", "html"),
    builtin!("stylesheet", "css"),
    builtin!("scss", "scss"),
    builtin!("less", "less"),
    builtin!("json", "json"),
    builtin!("markdown", "md"),
];

pub fn builtin() -> Vec<Template> {
    BUILTIN
        .iter()
        .filter_map(|(id, manifest, body)| {
            Manifest::parse(manifest)
                .with_context(|| format!("built-in template {id}"))
                .log_err()
                .map(|manifest| Template::new(id, manifest, body.to_string()))
        })
        .collect()
}

/// `~/.config/zenith/templates/`.
pub fn user_dir() -> PathBuf {
    paths::config_dir().join("templates")
}

/// Every template folder in `dir`; a missing `dir` is no templates, a broken
/// folder is logged and skipped.
pub async fn load_dir(fs: &dyn Fs, dir: &Path) -> Vec<Template> {
    if !fs.is_dir(dir).await {
        return Vec::new();
    }
    let Some(mut paths) = fs.read_dir(dir).await.log_err() else {
        return Vec::new();
    };
    let mut templates = Vec::new();
    while let Some(path) = paths.next().await {
        let Some(path) = path.log_err() else {
            continue;
        };
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !fs.is_dir(&path).await {
            continue;
        }
        let id = id.to_string();
        if let Some(template) = load_one(fs, &path, &id)
            .await
            .with_context(|| format!("template {}", path.display()))
            .log_err()
        {
            templates.push(template);
        }
    }
    templates.sort_by(|a, b| a.id.cmp(&b.id));
    templates
}

async fn load_one(fs: &dyn Fs, folder: &Path, id: &str) -> Result<Template> {
    let manifest = Manifest::parse(&fs.load(&folder.join("manifest.json")).await?)?;
    let body = fs
        .load(&folder.join(format!("template.{}", manifest.extension)))
        .await?;
    Ok(Template::new(id, manifest, body))
}

/// Later layers replace earlier ones by id; new ids are appended in order.
pub fn merge(layers: impl IntoIterator<Item = Vec<Template>>) -> Vec<Template> {
    let mut merged: Vec<Template> = Vec::new();
    for template in layers.into_iter().flatten() {
        match merged.iter_mut().find(|known| known.id == template.id) {
            Some(slot) => *slot = template,
            None => merged.push(template),
        }
    }
    merged
}

/// Built-ins, then the user's folder, then the project's (`None` when the project
/// is untrusted or remote).
pub async fn load_all(fs: Arc<dyn Fs>, project_dir: Option<PathBuf>) -> Vec<Template> {
    let user = load_dir(fs.as_ref(), &user_dir()).await;
    let project = match project_dir {
        Some(dir) => load_dir(fs.as_ref(), &dir).await,
        None => Vec::new(),
    };
    merge([builtin(), user, project])
}

#[derive(Debug, PartialEq)]
pub struct Rendered {
    pub text: String,
    /// Byte offset of the first `${CURSOR}`.
    pub cursor: Option<usize>,
}

/// Replaces each `${NAME}` that `variables` knows. Unknown ones stay as written,
/// so `${value}` in a JavaScript template literal survives; values are not
/// scanned again.
pub fn render(body: &str, variables: &HashMap<String, String>) -> Rendered {
    let mut text = String::with_capacity(body.len());
    let mut cursor = None;
    let mut rest = body;
    while let Some(start) = rest.find("${") {
        text.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name = after
            .find('}')
            .map(|end| &after[..end])
            .filter(|name| is_variable_name(name));
        match name {
            Some(CURSOR) => {
                cursor.get_or_insert(text.len());
                rest = &after[CURSOR.len() + 1..];
            }
            Some(name) if variables.contains_key(name) => {
                text.push_str(&variables[name]);
                rest = &after[name.len() + 1..];
            }
            _ => {
                text.push_str("${");
                rest = after;
            }
        }
    }
    text.push_str(rest);
    Rendered { text, cursor }
}

/// `${NAME}`, `${CLASS_NAME}`, `${NAMESPACE}`, `${DATE}`, `${YEAR}`, `${USER}`.
pub fn standard_variables(name: &str, namespace: &str) -> HashMap<String, String> {
    let now = OffsetDateTime::now_utc()
        .to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    HashMap::from_iter(
        [
            ("NAME", name.to_string()),
            ("CLASS_NAME", name.to_string()),
            ("NAMESPACE", namespace.to_string()),
            (
                "DATE",
                format!(
                    "{:04}-{:02}-{:02}",
                    now.year(),
                    u8::from(now.month()),
                    now.day()
                ),
            ),
            ("YEAR", now.year().to_string()),
            ("USER", user),
        ]
        .map(|(key, value)| (key.to_string(), value)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use util::path;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn test_every_builtin_parses() {
        assert_eq!(builtin().len(), BUILTIN.len());
    }

    #[test]
    fn test_render_replaces_known_keeps_unknown_and_finds_the_cursor() {
        let rendered = render(
            "a ${NAME} ${CURSOR}b `${value}` ${MISSING} ${NAME",
            &vars(&[("NAME", "Foo")]),
        );
        assert_eq!(rendered.text, "a Foo b `${value}` ${MISSING} ${NAME");
        assert_eq!(rendered.cursor, Some(6));
    }

    #[test]
    fn test_render_does_not_expand_values_again() {
        let rendered = render("${A}", &vars(&[("A", "${B}"), ("B", "x")]));
        assert_eq!(rendered.text, "${B}");
        assert_eq!(rendered.cursor, None);
    }

    #[test]
    fn test_manifest_validation() {
        assert!(Manifest::parse(r#"{"name":"X","extension":"php"}"#).is_ok());
        assert!(Manifest::parse(r#"{"name":"X","extension":"blade.php"}"#).is_ok());
        for bad in [
            r#"{"name":"X","extension":"../php"}"#,
            r#"{"name":"X","extension":"a/b"}"#,
            r#"{"name":"X","extension":""}"#,
            r#"{"name":"","extension":"php"}"#,
            r#"{"name":"X","extension":"php","variables":[{"name":"lower"}]}"#,
            r#"{"name":"X","extension":"php","variables":[{"name":"CURSOR"}]}"#,
            r#"{"extension":"php"}"#,
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
    }

    #[gpui::test]
    async fn test_user_and_project_templates_replace_builtins_by_id(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/user"),
            json!({
                "php-file": {
                    "manifest.json": r#"{"name":"PHP File","extension":"php"}"#,
                    "template.php": "<?php // mine\n",
                },
                "wp-plugin": {
                    "manifest.json": r#"{"name":"WP Plugin","extension":"php","variables":[{"name":"TEXT_DOMAIN","label":"Text domain"}]}"#,
                    "template.php": "<?php /* ${TEXT_DOMAIN} */\n",
                },
                "broken": { "manifest.json": "{" },
                "no-body": { "manifest.json": r#"{"name":"N","extension":"txt"}"# },
            }),
        )
        .await;
        fs.insert_tree(
            path!("/project"),
            json!({
                "wp-plugin": {
                    "manifest.json": r#"{"name":"Project Plugin","extension":"php"}"#,
                    "template.php": "<?php\n",
                },
            }),
        )
        .await;
        let user = load_dir(fs.as_ref(), Path::new(path!("/user"))).await;
        assert_eq!(
            user.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["php-file", "wp-plugin"]
        );
        assert_eq!(user[1].prompts[0].label(), "Text domain");
        assert!(
            load_dir(fs.as_ref(), Path::new(path!("/missing")))
                .await
                .is_empty()
        );

        let project = load_dir(fs.as_ref(), Path::new(path!("/project"))).await;
        let merged = merge([builtin(), user, project]);
        assert_eq!(merged.len(), BUILTIN.len() + 1);
        assert_eq!(merged[0].body, "<?php // mine\n");
        assert_eq!(
            merged.last().map(|t| t.name.as_str()),
            Some("Project Plugin")
        );
    }
}
