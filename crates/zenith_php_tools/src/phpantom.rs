//! Points PHPantom (built from `main`, plan 42) at the user's library roots through its
//! GLOBAL `.phpantom.toml`, and at the Zenith-built binary through
//! `lsp.phpantom.binary.path`. Stub packages are not written: PHPantom discovers
//! Composer-installed ones per project, and a global path would let any opened repo poison it. Everything here is additive and idempotent: user content in
//! the file is never overwritten or reordered, and only entries Zenith added itself (recorded
//! in a sidecar file) are ever removed.

use gpui::Entity;
use gpui::{App, AppContext as _, ReadGlobal as _};
use project::Project;
use settings::SettingsStore;
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Mutex,
};
use toml_edit::{Array, DocumentMut, Item, Table};

/// Two windows may sync at once; the file is read, edited and written as one step.
static SYNC_LOCK: Mutex<()> = Mutex::new(());

/// `~/.local/share/zenith/tools/phpantom/current/phpantom_lsp`: the binary the owner or lead
/// built from `main` and symlinked as `current`.
fn binary_path(home: &Path) -> PathBuf {
    home.join(".local/share/zenith/tools/phpantom/current/phpantom_lsp")
}

/// Entries Zenith added to the config file: `{"include": [...]}`. Older versions also
/// recorded `"stubs"`; those are removed from the config once, then forgotten.
fn managed_entries_file(home: &Path) -> PathBuf {
    home.join(".local/share/zenith/tools/phpantom/managed-entries.json")
}

/// `<XDG_CONFIG_HOME or ~/.config>/phpantom_lsp/.phpantom.toml`. PHPantom uses XDG on macOS
/// too, not `Library/Application Support`.
fn config_file(home: &Path, xdg: Option<PathBuf>) -> PathBuf {
    xdg.filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("phpantom_lsp/.phpantom.toml")
}

#[derive(Default, PartialEq)]
struct Managed {
    include: Vec<String>,
    /// Legacy `[stubs] paths` entries to migrate away; never populated by a new sync.
    legacy_stubs: Vec<String>,
}

fn load_managed(path: &Path) -> Managed {
    let value = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_default();
    let list = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Managed {
        include: list("include"),
        legacy_stubs: list("stubs"),
    }
}

/// Writes `text` to a temp file next to `path`, then renames it over `path`: PHPantom polls
/// the file, and a torn read at startup would mean defaults for the whole session.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let temp = parent.join(name);
    std::fs::write(&temp, text)
        .and_then(|()| std::fs::rename(&temp, path))
        .map_err(|error| {
            std::fs::remove_file(&temp).ok();
            format!("writing {}: {error}", path.display())
        })
}

/// Adds `add` to the `key` array of `[table]`, removes the previously managed entries that
/// `keep` rejects, and returns the entries Zenith now manages. Entries already present
/// (the user's own) are not claimed, so they are never removed later.
fn sync_list(
    doc: &mut DocumentMut,
    table: &str,
    key: &str,
    add: &[String],
    previous: &[String],
    keep: impl Fn(&String) -> bool,
) -> Result<Vec<String>, String> {
    let removable: Vec<&String> = previous.iter().filter(|entry| !keep(entry)).collect();
    let mut managed: Vec<String> = previous
        .iter()
        .filter(|entry| keep(entry))
        .cloned()
        .collect();
    if add.is_empty() && removable.is_empty() {
        return Ok(managed);
    }
    if doc.get(table).is_none() {
        if add.is_empty() {
            return Ok(managed);
        }
        doc.insert(table, Item::Table(Table::new()));
    }
    let section = doc
        .get_mut(table)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("`{table}` is not a table"))?;
    if section.get(key).is_none() {
        if add.is_empty() {
            return Ok(managed);
        }
        section.insert(key, Item::Value(Array::new().into()));
    }
    let array = section
        .get_mut(key)
        .and_then(Item::as_array_mut)
        .ok_or_else(|| format!("`{table}.{key}` is not an array"))?;
    array.retain(|value| {
        value
            .as_str()
            .is_none_or(|entry| !removable.iter().any(|removed| removed.as_str() == entry))
    });
    for path in add {
        if !array.iter().any(|value| value.as_str() == Some(path)) {
            array.push(path.as_str());
            if !managed.contains(path) {
                managed.push(path.clone());
            }
        }
    }
    Ok(managed)
}

/// Brings the global `.phpantom.toml` in line with the wanted `include` list. A file that does not parse is
/// left untouched. Writes only when the content changes.
fn sync_config(home: &Path, xdg: Option<PathBuf>, include: &[String]) -> Result<(), String> {
    let _guard = SYNC_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let config = config_file(home, xdg);
    let sidecar = managed_entries_file(home);
    let previous = load_managed(&sidecar);
    let text = match std::fs::read_to_string(&config) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading {}: {error}", config.display())),
    };
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        format!(
            "{} is not valid TOML, left untouched: {error}",
            config.display()
        )
    })?;
    // One-time migration: drop only the stub entries an older Zenith added.
    if !previous.legacy_stubs.is_empty() {
        sync_list(
            &mut doc,
            "stubs",
            "paths",
            &[],
            &previous.legacy_stubs,
            |_| false,
        )
        .ok();
    }
    let managed = Managed {
        include: sync_list(
            &mut doc,
            "indexing",
            "include",
            include,
            &previous.include,
            |entry| include.contains(entry),
        )?,
        legacy_stubs: Vec::new(),
    };
    let new_text = doc.to_string();
    if new_text != text {
        write_atomic(&config, &new_text)?;
    }
    if managed != previous {
        write_atomic(
            &sidecar,
            &serde_json::json!({ "include": managed.include }).to_string(),
        )?;
    }
    Ok(())
}

/// Runs [`sync_config`] on the background executor, then points `lsp.phpantom.binary.path` at
/// the Zenith-built binary when it exists and the user set no path. Called at startup, when
/// worktrees change and when settings change; cheap and idempotent.
pub(crate) fn sync(project: &Entity<Project>, cx: &mut App) {
    // User settings not loaded yet: the library list would read as empty and prune entries.
    let Some(user) = SettingsStore::global(cx).raw_user_settings() else {
        return;
    };
    let mut include: Vec<String> = Vec::new();
    for path in user
        .content
        .zenith
        .as_ref()
        .and_then(|zenith| zenith.external_libraries.as_ref())
        .and_then(|libraries| libraries.paths.as_ref())
        .into_iter()
        .flatten()
    {
        if Path::new(path).is_absolute() && !include.contains(path) {
            include.push(path.clone());
        }
    }
    let binary_unset = user
        .content
        .project
        .lsp
        .0
        .get("phpantom")
        .and_then(|lsp| lsp.binary.as_ref())
        .and_then(|binary| binary.path.as_ref())
        .is_none();
    let fs = project.read(cx).fs().clone();
    let home = crate::home_dir();
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    cx.spawn(async move |cx| {
        let binary = binary_path(&home);
        let set_binary = cx
            .background_spawn(async move {
                if let Err(error) = sync_config(&home, xdg, &include) {
                    log::warn!("phpantom config: {error}");
                }
                binary_unset
                    .then(|| binary.is_file().then(|| binary.display().to_string()))
                    .flatten()
            })
            .await;
        if let Some(path) = set_binary {
            cx.update(|cx| {
                settings::update_settings_file(fs, cx, move |content, _| {
                    let lsp = content.project.lsp.0.entry("phpantom".into()).or_default();
                    let binary = lsp.binary.get_or_insert_default();
                    if binary.path.is_none() {
                        binary.path = Some(path);
                    }
                });
            });
        }
    })
    .detach();
}
