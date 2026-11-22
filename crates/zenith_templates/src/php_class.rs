//! "New PHP Class" (plan 20 milestone 1): kind, name, namespace prefilled from
//! PSR-4, extends, implements, final/abstract and `declare(strict_types=1)`.

use crate::{
    create_file, project_templates_dir, psr4, resolve, selected_target_dir,
    template::{self, Rendered},
};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task,
    WeakEntity, Window, actions, rems,
};
use project::{Project, ProjectPath};
use std::{collections::HashMap, path::PathBuf};
use ui::{Button, Checkbox, prelude::*};
use workspace::{ModalView, Workspace};

actions!(
    zenith_templates,
    [
        /// Moves to the next field of the New PHP Class dialog.
        NextField,
        /// Moves to the previous field of the New PHP Class dialog.
        PreviousField
    ]
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PhpKind {
    #[default]
    Class,
    Interface,
    Trait,
    Enum,
}

impl PhpKind {
    const ALL: [Self; 4] = [Self::Class, Self::Interface, Self::Trait, Self::Enum];

    fn label(self) -> &'static str {
        match self {
            Self::Class => "Class",
            Self::Interface => "Interface",
            Self::Trait => "Trait",
            Self::Enum => "Enum",
        }
    }

    pub fn template_id(self) -> &'static str {
        match self {
            Self::Class => "php-class",
            Self::Interface => "php-interface",
            Self::Trait => "php-trait",
            Self::Enum => "php-enum",
        }
    }

    fn has_extends(self) -> bool {
        matches!(self, Self::Class | Self::Interface)
    }

    fn has_implements(self) -> bool {
        matches!(self, Self::Class | Self::Enum)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Modifier {
    #[default]
    None,
    Final,
    Abstract,
}

#[derive(Clone, Debug, Default)]
pub struct PhpClassSpec {
    pub kind: PhpKind,
    pub name: String,
    pub namespace: String,
    pub extends: String,
    pub implements: String,
    pub modifier: Modifier,
    pub strict_types: bool,
}

/// https://www.php.net/manual/en/reserved.keywords.php and
/// https://www.php.net/manual/en/reserved.other-reserved-words.php (hard reserved;
/// `array` and `callable` as of PHP 8.5): none can name a class-like.
const RESERVED: &[&str] = &[
    "__halt_compiler",
    "abstract",
    "and",
    "array",
    "as",
    "break",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "die",
    "do",
    "echo",
    "else",
    "elseif",
    "empty",
    "enddeclare",
    "endfor",
    "endforeach",
    "endif",
    "endswitch",
    "endwhile",
    "eval",
    "exit",
    "extends",
    "final",
    "finally",
    "fn",
    "for",
    "foreach",
    "function",
    "global",
    "goto",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "insteadof",
    "interface",
    "isset",
    "list",
    "match",
    "namespace",
    "new",
    "or",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "static",
    "switch",
    "throw",
    "trait",
    "try",
    "unset",
    "use",
    "var",
    "while",
    "xor",
    "yield",
    "__class__",
    "__dir__",
    "__file__",
    "__function__",
    "__line__",
    "__method__",
    "__property__",
    "__namespace__",
    "__trait__",
    "parent",
    "self",
    "int",
    "float",
    "bool",
    "string",
    "true",
    "false",
    "null",
    "void",
    "iterable",
    "object",
    "mixed",
    "never",
];

/// PHP's label rule, `[a-zA-Z_\x80-\xff][a-zA-Z0-9_\x80-\xff]*`
/// (https://www.php.net/manual/en/language.oop5.basic.php): any non-ASCII
/// character is allowed, as its UTF-8 bytes are all `\x80`-`\xff`.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic() || !c.is_ascii())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric() || !c.is_ascii())
}

pub fn is_class_name(name: &str) -> bool {
    is_identifier(name) && !RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

/// `Foo`, `Foo\Bar` or `\Foo\Bar`.
fn is_qualified_name(name: &str) -> bool {
    name.strip_prefix('\\')
        .unwrap_or(name)
        .split('\\')
        .all(is_identifier)
}

fn names(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

impl PhpClassSpec {
    pub fn namespace(&self) -> &str {
        self.namespace.trim().trim_matches('\\')
    }

    /// The first thing wrong with the spec, as shown under the fields.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Enter a name".into());
        }
        if !is_class_name(name) {
            return Err(format!(
                "{name:?} is not a valid PHP {} name",
                self.kind.label().to_lowercase()
            ));
        }
        let namespace = self.namespace();
        if !namespace.is_empty() && !namespace.split('\\').all(is_identifier) {
            return Err(format!("{namespace:?} is not a valid namespace"));
        }
        if self.kind.has_extends() {
            let extends = names(&self.extends);
            if self.kind == PhpKind::Class && extends.len() > 1 {
                return Err("A class extends one class".into());
            }
            if let Some(bad) = extends.iter().find(|name| !is_qualified_name(name)) {
                return Err(format!("{bad:?} is not a valid class name"));
            }
        }
        if self.kind.has_implements()
            && let Some(bad) = names(&self.implements)
                .into_iter()
                .find(|name| !is_qualified_name(name))
        {
            return Err(format!("{bad:?} is not a valid interface name"));
        }
        Ok(())
    }

    /// The standard variables plus the pieces the built-in PHP templates use:
    /// `${STRICT_TYPES}`, `${NAMESPACE_DECLARATION}`, `${MODIFIERS}`, `${EXTENDS}`,
    /// `${IMPLEMENTS}`. Fields the kind does not have render empty.
    pub fn variables(&self) -> HashMap<String, String> {
        let namespace = self.namespace();
        let mut variables = template::standard_variables(self.name.trim(), namespace);
        let list = |keyword: &str, list: &str, applies: bool| {
            let names = names(list);
            if applies && !names.is_empty() {
                format!(" {keyword} {}", names.join(", "))
            } else {
                String::new()
            }
        };
        let modifiers = match (self.kind, self.modifier) {
            (PhpKind::Class, Modifier::Final) => "final ",
            (PhpKind::Class, Modifier::Abstract) => "abstract ",
            _ => "",
        };
        variables.extend([
            (
                "STRICT_TYPES".to_string(),
                if self.strict_types {
                    "\ndeclare(strict_types=1);\n".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "NAMESPACE_DECLARATION".to_string(),
                if namespace.is_empty() {
                    String::new()
                } else {
                    format!("\nnamespace {namespace};\n")
                },
            ),
            ("MODIFIERS".to_string(), modifiers.to_string()),
            (
                "EXTENDS".to_string(),
                list("extends", &self.extends, self.kind.has_extends()),
            ),
            (
                "IMPLEMENTS".to_string(),
                list("implements", &self.implements, self.kind.has_implements()),
            ),
        ]);
        variables
    }
}

pub struct NewPhpClassDialog {
    kind: PhpKind,
    modifier: Modifier,
    strict_types: bool,
    name: Entity<Editor>,
    namespace: Entity<Editor>,
    extends: Entity<Editor>,
    implements: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    dir: ProjectPath,
    project_templates: Option<PathBuf>,
    _prefill_namespace: Task<()>,
}

impl ModalView for NewPhpClassDialog {}
impl EventEmitter<DismissEvent> for NewPhpClassDialog {}

impl Focusable for NewPhpClassDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl NewPhpClassDialog {
    pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let project = workspace.project().clone();
        let Some(dir) = selected_target_dir(workspace, cx) else {
            return;
        };
        let project_templates = project_templates_dir(&project, dir.worktree_id, cx);
        let workspace_handle = workspace.weak_handle();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(
                workspace_handle,
                project,
                dir,
                project_templates,
                window,
                cx,
            )
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        dir: ProjectPath,
        project_templates: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let name = field("Name", window, cx);
        let namespace = field("Namespace (empty for global)", window, cx);
        let extends = field("Parent", window, cx);
        let implements = field("Interfaces, comma-separated", window, cx);
        for editor in [&name, &namespace, &extends, &implements] {
            cx.subscribe(editor, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            })
            .detach();
        }

        // Only a local folder has a composer.json the app can read.
        let (fs, root, abs_dir) = {
            let project = project.read(cx);
            let root = project
                .worktree_for_id(dir.worktree_id, cx)
                .map(|worktree| worktree.read(cx).abs_path());
            let abs_dir = project
                .is_local()
                .then(|| project.absolute_path(&dir, cx))
                .flatten();
            (project.fs().clone(), root, abs_dir)
        };
        let prefill = cx.spawn_in(window, async move |this, cx| {
            let (Some(root), Some(abs_dir)) = (root, abs_dir) else {
                return;
            };
            let Some(found) = psr4::resolve(fs.as_ref(), &root, &abs_dir).await else {
                return;
            };
            // The dialog may be gone; a namespace typed meanwhile wins.
            this.update_in(cx, |this, window, cx| {
                if this.namespace.read(cx).is_empty(cx) {
                    this.namespace
                        .update(cx, |editor, cx| editor.set_text(found, window, cx));
                }
            })
            .ok();
        });

        Self {
            kind: PhpKind::Class,
            modifier: Modifier::None,
            strict_types: false,
            name,
            namespace,
            extends,
            implements,
            workspace,
            project,
            dir,
            project_templates,
            _prefill_namespace: prefill,
        }
    }

    fn spec(&self, cx: &App) -> PhpClassSpec {
        PhpClassSpec {
            kind: self.kind,
            name: self.name.read(cx).text(cx),
            namespace: self.namespace.read(cx).text(cx),
            extends: self.extends.read(cx).text(cx),
            implements: self.implements.read(cx).text(cx),
            modifier: self.modifier,
            strict_types: self.strict_types,
        }
    }

    fn target(&self, spec: &PhpClassSpec, cx: &App) -> Result<ProjectPath, String> {
        spec.validate()?;
        resolve(
            self.project.read(cx),
            &self.dir,
            spec.name.trim(),
            "php",
            cx,
        )
        .map_err(str::to_string)
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.spec(cx);
        let Ok(target) = self.target(&spec, cx) else {
            return;
        };
        let variables = spec.variables();
        let template_id = spec.kind.template_id();
        let fs = self.project.read(cx).fs().clone();
        let project_templates = self.project_templates.clone();
        let content = async move {
            let templates = template::load_all(fs, project_templates).await;
            let template = templates
                .iter()
                .find(|template| template.id == template_id)
                .ok_or_else(|| anyhow::anyhow!("the {template_id} template is missing"))?;
            anyhow::Ok::<Rendered>(template::render(&template.body, &variables))
        };
        create_file(
            self.workspace.clone(),
            &self.project,
            target,
            content,
            window,
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// The fields shown for the current kind, in tab order.
    fn fields(&self) -> Vec<&Entity<Editor>> {
        let mut fields = vec![&self.name, &self.namespace];
        if self.kind.has_extends() {
            fields.push(&self.extends);
        }
        if self.kind.has_implements() {
            fields.push(&self.implements);
        }
        fields
    }

    fn move_focus(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields();
        let current = fields
            .iter()
            .position(|field| field.focus_handle(cx).is_focused(window))
            .unwrap_or(0);
        let next = (current as isize + step).rem_euclid(fields.len() as isize) as usize;
        if let Some(field) = fields.get(next) {
            window.focus(&field.focus_handle(cx), cx);
        }
    }

    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn previous_field(&mut self, _: &PreviousField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    fn row(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(
                div()
                    .w(rems(6.))
                    .child(Label::new(label).color(Color::Muted)),
            )
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(editor.clone()),
            )
    }
}

impl Render for NewPhpClassDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let spec = self.spec(cx);
        let (status, color) = match self.target(&spec, cx) {
            Ok(path) => (
                format!(
                    "Creates {}",
                    path.path.display(self.project.read(cx).path_style(cx))
                ),
                Color::Muted,
            ),
            Err(message) if spec.name.trim().is_empty() => (message, Color::Muted),
            Err(message) => (message, Color::Error),
        };
        let kinds = h_flex().gap_1().children(PhpKind::ALL.iter().map(|&kind| {
            Button::new(
                SharedString::from(format!("zenith-php-kind-{}", kind.label())),
                kind.label(),
            )
            .toggle_state(self.kind == kind)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.kind = kind;
                cx.notify();
            }))
        }));
        let is_class = self.kind == PhpKind::Class;
        let modifier = |label: &'static str, value: Modifier, cx: &mut Context<Self>| {
            Checkbox::new(label, (self.modifier == value).into())
                .label(label)
                .disabled(!is_class)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.modifier = if this.modifier == value {
                        Modifier::None
                    } else {
                        value
                    };
                    cx.notify();
                }))
        };
        let final_box = modifier("final", Modifier::Final, cx);
        let abstract_box = modifier("abstract", Modifier::Abstract, cx);
        v_flex()
            .key_context("ZenithNewPhpClass")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::previous_field))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(Label::new("New PHP Class").weight(gpui::FontWeight::BOLD))
            .child(kinds)
            .child(Self::row("Name", &self.name, cx))
            .child(Self::row("Namespace", &self.namespace, cx))
            .when(self.kind.has_extends(), |this| {
                this.child(Self::row("Extends", &self.extends, cx))
            })
            .when(self.kind.has_implements(), |this| {
                this.child(Self::row("Implements", &self.implements, cx))
            })
            .child(
                h_flex().gap_3().child(final_box).child(abstract_box).child(
                    Checkbox::new("zenith-php-strict-types", self.strict_types.into())
                        .label("declare(strict_types=1)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.strict_types = !this.strict_types;
                            cx.notify();
                        })),
                ),
            )
            .child(Label::new(status).color(color))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(spec: &PhpClassSpec) -> Rendered {
        let template = template::builtin()
            .into_iter()
            .find(|template| template.id == spec.kind.template_id())
            .unwrap();
        template::render(&template.body, &spec.variables())
    }

    fn spec(kind: PhpKind, name: &str) -> PhpClassSpec {
        PhpClassSpec {
            kind,
            name: name.into(),
            ..PhpClassSpec::default()
        }
    }

    #[test]
    fn test_golden_class_with_everything() {
        let rendered = render(&PhpClassSpec {
            namespace: "\\App\\Models\\".into(),
            extends: "Model".into(),
            implements: "Countable, \\JsonSerializable,".into(),
            modifier: Modifier::Final,
            strict_types: true,
            ..spec(PhpKind::Class, "User")
        });
        let expected = "<?php\n\ndeclare(strict_types=1);\n\nnamespace App\\Models;\n\n\
final class User extends Model implements Countable, \\JsonSerializable\n{\n    \n}\n";
        assert_eq!(rendered.text, expected);
        assert_eq!(rendered.cursor, expected.find("    \n}").map(|at| at + 4));
    }

    #[test]
    fn test_golden_plain_class_in_the_global_namespace() {
        let rendered = render(&spec(PhpKind::Class, "Foo"));
        assert_eq!(rendered.text, "<?php\n\nclass Foo\n{\n    \n}\n");
        assert_eq!(rendered.cursor, Some("<?php\n\nclass Foo\n{\n    ".len()));
    }

    #[test]
    fn test_golden_abstract_class() {
        let rendered = render(&PhpClassSpec {
            modifier: Modifier::Abstract,
            namespace: "App".into(),
            ..spec(PhpKind::Class, "Base")
        });
        assert_eq!(
            rendered.text,
            "<?php\n\nnamespace App;\n\nabstract class Base\n{\n    \n}\n"
        );
    }

    #[test]
    fn test_golden_interface_extends_several_and_ignores_class_fields() {
        let rendered = render(&PhpClassSpec {
            namespace: "App\\Contracts".into(),
            extends: "A, B".into(),
            implements: "Ignored".into(),
            modifier: Modifier::Final,
            ..spec(PhpKind::Interface, "Repository")
        });
        assert_eq!(
            rendered.text,
            "<?php\n\nnamespace App\\Contracts;\n\ninterface Repository extends A, B\n{\n    \n}\n"
        );
    }

    #[test]
    fn test_golden_trait() {
        let rendered = render(&PhpClassSpec {
            strict_types: true,
            extends: "Ignored".into(),
            ..spec(PhpKind::Trait, "HasSlug")
        });
        assert_eq!(
            rendered.text,
            "<?php\n\ndeclare(strict_types=1);\n\ntrait HasSlug\n{\n    \n}\n"
        );
    }

    #[test]
    fn test_golden_enum_implements() {
        let rendered = render(&PhpClassSpec {
            namespace: "App\\Enums".into(),
            implements: "HasLabel".into(),
            ..spec(PhpKind::Enum, "Status")
        });
        assert_eq!(
            rendered.text,
            "<?php\n\nnamespace App\\Enums;\n\nenum Status implements HasLabel\n{\n    \n}\n"
        );
    }

    #[test]
    fn test_name_validation() {
        for good in [
            "Foo",
            "_foo",
            "Foo2",
            "Ünïcode",
            "Enum",
            "Resource",
            "ReadonlyThing",
        ] {
            assert!(is_class_name(good), "{good} was refused");
        }
        for bad in [
            "",
            "2Foo",
            "Foo-Bar",
            "Foo Bar",
            "Foo\\Bar",
            "Foo.php",
            "class",
            "CLASS",
            "Self",
            "parent",
            "int",
            "Array",
            "callable",
            "readonly",
            "never",
            "__CLASS__",
            "list",
        ] {
            assert!(!is_class_name(bad), "{bad:?} was accepted");
        }
    }

    #[test]
    fn test_spec_validation() {
        assert!(spec(PhpKind::Class, " Foo ").validate().is_ok());
        assert!(spec(PhpKind::Class, "").validate().is_err());
        let with = |edit: fn(&mut PhpClassSpec)| {
            let mut spec = spec(PhpKind::Class, "Foo");
            edit(&mut spec);
            spec.validate()
        };
        assert!(with(|s| s.namespace = "App\\Http".into()).is_ok());
        assert!(with(|s| s.namespace = "\\App\\".into()).is_ok());
        assert!(with(|s| s.namespace = "App\\\\Http".into()).is_err());
        assert!(with(|s| s.namespace = "App\\1Http".into()).is_err());
        assert!(with(|s| s.extends = "\\Illuminate\\Model".into()).is_ok());
        assert!(with(|s| s.extends = "A, B".into()).is_err(), "one parent");
        assert!(with(|s| s.extends = "Bad-Name".into()).is_err());
        assert!(with(|s| s.implements = "A, B\\C".into()).is_ok());
        assert!(with(|s| s.implements = "A; B".into()).is_err());
        // Fields the kind does not have are not checked.
        let mut trait_spec = spec(PhpKind::Trait, "T");
        trait_spec.extends = "not valid!".into();
        assert!(trait_spec.validate().is_ok());
    }
}
