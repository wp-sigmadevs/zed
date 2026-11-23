//! PhpStorm's Deployment dialog, Connection tab (plan 37 M1b): the server list with add, remove
//! and "use as default", the connection form, Test Connection, OK and Cancel. Servers go to the
//! user data dir (`servers.json`), passwords to the Keychain; nothing here writes a project file.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, Window, actions,
};
use ui::{Callout, Checkbox, Disclosure, IconButton, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{Password, ServerConfig, ServerStore, ServerType, Servers, secrets, spawn_operation};

actions!(
    zenith_deploy,
    [
        /// Opens the Deployment dialog: servers and their connection settings.
        OpenDeployment,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenDeployment, window, cx| {
            DeploymentDialog::open(workspace, None, window, cx);
        });
    })
    .detach();
}

/// The first visible folder of the workspace: the key for project defaults and for
/// "Visible only for this project".
pub(crate) fn project_root(workspace: &Workspace, cx: &App) -> Option<String> {
    let project = workspace.project().read(cx);
    let worktree = project.visible_worktrees(cx).next()?;
    Some(worktree.read(cx).abs_path().to_string_lossy().into_owned())
}

pub(crate) const INSECURE_FTP: &str = "Insecure: plain FTP sends the password and files in clear text. Use FTPS or SFTP when the server supports it.";
pub(crate) const NO_USERNAME: &str = "Username is not specified";

struct Fields {
    name: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    username: Entity<Editor>,
    password: Entity<Editor>,
    root_path: Entity<Editor>,
    web_url: Entity<Editor>,
}

impl Fields {
    fn all(&self) -> [&Entity<Editor>; 7] {
        [
            &self.name,
            &self.host,
            &self.port,
            &self.username,
            &self.password,
            &self.root_path,
            &self.web_url,
        ]
    }
}

enum TestStatus {
    Running,
    Passed(SharedString),
    Failed(SharedString),
}

pub struct DeploymentDialog {
    store: Entity<ServerStore>,
    /// The working copy; the store changes only on OK.
    servers: Servers,
    loaded: bool,
    /// Passwords typed in this dialog, by server id; written on OK.
    passwords: HashMap<String, String>,
    removed: Vec<String>,
    selected: Option<usize>,
    project: Option<String>,
    fields: Fields,
    port_invalid: bool,
    advanced_open: bool,
    test_status: Option<TestStatus>,
    error: Option<SharedString>,
    _test: Option<Task<()>>,
    _save: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for DeploymentDialog {}
impl EventEmitter<DismissEvent> for DeploymentDialog {}

impl Focusable for DeploymentDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.fields.name.focus_handle(cx)
    }
}

impl DeploymentDialog {
    /// Opens the dialog, selecting `server_id` (or the project's default server).
    pub fn open(
        workspace: &mut Workspace,
        server_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = project_root(workspace, cx);
        let store = ServerStore::global(cx);
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(store, project, server_id, window, cx)
        });
    }

    fn new(
        store: Entity<ServerStore>,
        project: Option<String>,
        server_id: Option<String>,
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
        let fields = Fields {
            name: field("Server name", window, cx),
            host: field("ftp.example.com", window, cx),
            port: field("21", window, cx),
            username: field("User name", window, cx),
            password: field("Leave empty to keep the saved password", window, cx),
            root_path: field("/", window, cx),
            web_url: field("https://example.com", window, cx),
        };
        fields
            .password
            .update(cx, |editor, cx| editor.set_masked(true, cx));
        let mut subscriptions: Vec<Subscription> = fields
            .all()
            .into_iter()
            .map(|editor| {
                cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.read_fields(cx);
                    }
                })
            })
            .collect();
        subscriptions.push(cx.observe_in(&store, window, |this, store, window, cx| {
            if !this.loaded && store.read(cx).is_loaded() {
                this.copy_from_store(None, window, cx);
            }
        }));
        let mut this = Self {
            store,
            servers: Servers::default(),
            loaded: false,
            passwords: HashMap::new(),
            removed: Vec::new(),
            selected: None,
            project,
            fields,
            port_invalid: false,
            advanced_open: false,
            test_status: None,
            error: None,
            _test: None,
            _save: None,
            _subscriptions: subscriptions,
        };
        if this.store.read(cx).is_loaded() {
            this.copy_from_store(server_id, window, cx);
        }
        this
    }

    fn copy_from_store(
        &mut self,
        server_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        self.servers = store.servers().clone();
        self.error = store.load_error().map(|error| {
            SharedString::from(format!("The saved servers could not be read: {error}"))
        });
        self.loaded = true;
        let wanted = server_id.or_else(|| {
            self.project
                .as_deref()
                .and_then(|project| self.servers.project_defaults.get(project).cloned())
        });
        let index = wanted
            .and_then(|id| {
                self.servers
                    .servers
                    .iter()
                    .position(|server| server.id == id)
            })
            .or_else(|| (!self.servers.servers.is_empty()).then_some(0));
        self.select(index, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn servers(&self) -> &Servers {
        &self.servers
    }

    pub(crate) fn selected_server(&self) -> Option<&ServerConfig> {
        self.servers.servers.get(self.selected?)
    }

    fn selected_server_mut(&mut self) -> Option<&mut ServerConfig> {
        self.servers.servers.get_mut(self.selected?)
    }

    pub(crate) fn select(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = index.filter(|index| *index < self.servers.servers.len());
        self.test_status = None;
        self.port_invalid = false;
        let server = self.selected_server().cloned().unwrap_or_default();
        let password = self.passwords.get(&server.id).cloned().unwrap_or_default();
        let texts = [
            (&self.fields.name, server.name.clone()),
            (&self.fields.host, server.host.clone()),
            (&self.fields.port, server.port.to_string()),
            (&self.fields.username, server.username.clone()),
            (&self.fields.password, password),
            (&self.fields.root_path, server.root_path.clone()),
            (&self.fields.web_url, server.web_url),
        ];
        for (editor, text) in texts {
            editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
        }
        cx.notify();
    }

    /// Copies the editors into the selected server.
    fn read_fields(&mut self, cx: &mut Context<Self>) {
        let text = |editor: &Entity<Editor>, cx: &App| editor.read(cx).text(cx);
        let name = text(&self.fields.name, cx);
        let host = text(&self.fields.host, cx).trim().to_string();
        let port = text(&self.fields.port, cx);
        let username = text(&self.fields.username, cx).trim().to_string();
        let password = text(&self.fields.password, cx);
        let root_path = text(&self.fields.root_path, cx).trim().to_string();
        let web_url = text(&self.fields.web_url, cx).trim().to_string();
        let parsed_port = port.trim().parse::<u16>().ok().filter(|port| *port > 0);
        self.port_invalid = parsed_port.is_none();
        let Some(server) = self.selected_server_mut() else {
            return;
        };
        server.name = name;
        server.host = host;
        if let Some(port) = parsed_port {
            server.port = port;
        }
        server.username = username;
        server.root_path = root_path;
        server.web_url = web_url;
        let id = server.id.clone();
        if password.is_empty() {
            self.passwords.remove(&id);
        } else {
            self.passwords.insert(id, password);
        }
        cx.notify();
    }

    pub(crate) fn add_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut server = ServerConfig::new();
        let mut number = self.servers.servers.len() + 1;
        while self
            .servers
            .servers
            .iter()
            .any(|existing| existing.name == format!("Server {number}"))
        {
            number += 1;
        }
        server.name = format!("Server {number}");
        self.servers.servers.push(server);
        self.select(Some(self.servers.servers.len() - 1), window, cx);
        window.focus(&self.fields.host.focus_handle(cx), cx);
    }

    pub(crate) fn remove_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        let removed = self.servers.servers.remove(index);
        self.servers
            .project_defaults
            .retain(|_, server_id| *server_id != removed.id);
        self.passwords.remove(&removed.id);
        self.removed.push(removed.id);
        let next = if self.servers.servers.is_empty() {
            None
        } else {
            Some(index.min(self.servers.servers.len() - 1))
        };
        self.select(next, window, cx);
    }

    /// Marks the selected server as this project's default (PhpStorm's ✓).
    pub(crate) fn set_default(&mut self, cx: &mut Context<Self>) {
        let (Some(project), Some(server)) = (self.project.clone(), self.selected_server()) else {
            return;
        };
        let id = server.id.clone();
        self.servers.project_defaults.insert(project, id);
        cx.notify();
    }

    fn is_default(&self, server: &ServerConfig) -> bool {
        self.project
            .as_deref()
            .is_some_and(|project| self.servers.project_defaults.get(project) == Some(&server.id))
    }

    pub(crate) fn set_type(
        &mut self,
        server_type: ServerType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.selected_server_mut() else {
            return;
        };
        if server.port == server.server_type.default_port() {
            server.port = server_type.default_port();
        }
        server.server_type = server_type;
        if server_type == ServerType::Sftp {
            server.anonymous = false;
        }
        let port = server.port.to_string();
        self.fields
            .port
            .update(cx, |editor, cx| editor.set_text(port, window, cx));
        self.port_invalid = false;
        self.test_status = None;
        cx.notify();
    }

    fn update_selected(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut ServerConfig)) {
        if let Some(server) = self.selected_server_mut() {
            update(server);
            cx.notify();
        }
    }

    /// The warnings shown under the form, in order.
    pub(crate) fn warnings(&self) -> Vec<&'static str> {
        let Some(server) = self.selected_server() else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        if server.username.is_empty() && !server.anonymous {
            warnings.push(NO_USERNAME);
        }
        if server.server_type == ServerType::Ftp {
            warnings.push(INSECURE_FTP);
        }
        warnings
    }

    pub(crate) fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.selected_server().cloned() else {
            return;
        };
        let typed = self.passwords.get(&server.id).cloned();
        let store = self.store.read(cx);
        let needs_password = server.uses_password();
        // The stored password belongs to the saved host, port, type and username: never send it
        // to another.
        let saved_target = store
            .servers()
            .servers
            .iter()
            .any(|saved| saved.id == server.id && !saved.credentials_target_changed(&server));
        if needs_password && typed.is_none() && !saved_target {
            self.test_status = Some(TestStatus::Failed(
                "Type the password to test a changed host, port, type or username; the saved one is not sent to it."
                    .into(),
            ));
            cx.notify();
            return;
        }
        let session = match typed {
            Some(password) if server.uses_password() => {
                let mut server = server;
                server.password = Some(Password(password));
                spawn_operation(cx, (store.connect)(server))
            }
            _ => store.open_session(server, cx),
        };
        self.test_status = Some(TestStatus::Running);
        cx.notify();
        self._test = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let mut transport = session.await?;
                cx.update(|cx| {
                    spawn_operation(cx, async move {
                        let directory = transport.pwd().await;
                        transport.disconnect().await.log_err();
                        directory
                    })
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.test_status = Some(match result {
                    Ok(directory) => TestStatus::Passed(
                        format!("Connected. Initial directory: {directory}").into(),
                    ),
                    Err(error) => {
                        TestStatus::Failed(format!("Connection failed: {error:#}").into())
                    }
                });
                cx.notify();
            })
            .log_err();
        }));
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        if !self.loaded || self.port_invalid || self.store.read(cx).load_error().is_some() {
            return;
        }
        let keychain = self.store.read(cx).keychain.clone();
        let mut deletes: Vec<Task<anyhow::Result<()>>> = Vec::new();
        let mut writes: Vec<(ServerConfig, String)> = Vec::new();
        let mut session_changes: Vec<(ServerConfig, Option<Password>)> = Vec::new();
        let previous = self.store.read(cx).servers().clone();
        let mut cleared_credentials = false;
        for server in &self.servers.servers {
            let typed = self.passwords.get(&server.id).cloned();
            let saves_password = server.save_password && server.uses_password();
            let old = previous.servers.iter().find(|old| old.id == server.id);
            let was_saved = old.is_some_and(|old| old.save_password && old.uses_password());
            // Nothing typed for a new target: the old password must not follow the server there.
            let target_changed =
                typed.is_none() && old.is_some_and(|old| old.credentials_target_changed(server));
            match typed {
                Some(password) if saves_password => {
                    writes.push((server.clone(), password));
                    session_changes.push((server.clone(), None));
                }
                Some(password) => session_changes.push((server.clone(), Some(Password(password)))),
                None => {}
            }
            if target_changed && server.uses_password() {
                // Only tell the user when a password was actually there to lose.
                let had_session =
                    old.is_some_and(|old| self.store.read(cx).session_password_for(old).is_some());
                cleared_credentials |= was_saved || had_session;
            }
            if was_saved && (!saves_password || target_changed) {
                deletes.push(secrets::delete_password(&keychain, &server.id, cx));
            }
        }
        for id in &self.removed {
            deletes.push(secrets::delete_password(&keychain, id, cx));
        }
        let servers = self.servers.clone();
        self._save = Some(cx.spawn(async move |this, cx| {
            // Deletes first, then writes, and the new servers last: a failure leaves the saved
            // servers as they were and the saved password (if its delete did not run) intact.
            // The session passwords change together with the servers, never before.
            let result: anyhow::Result<()> = async {
                for delete in futures::future::join_all(deletes).await {
                    delete.map_err(|error| {
                        error.context(
                            "Nothing was saved: the Keychain could not remove a saved password (others may already be removed)",
                        )
                    })?;
                }
                for (server, password) in writes {
                    let write = cx.update(|cx| {
                        secrets::save_password(keychain.as_ref(), &server, &password, cx)
                    });
                    write.await.map_err(|error| {
                        error.context("Nothing was saved: the Keychain could not store the password")
                    })?;
                }
                let save = this.update(cx, |this, cx| {
                    this.store.update(cx, |store, cx| {
                        for (server, password) in session_changes {
                            store.set_session_password(&server, password);
                        }
                        store.save(servers, cx)
                    })
                })?;
                save.await
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok(()) if cleared_credentials => {
                    this.error = Some(
                        "Saved. The saved password was removed because the host, port, type or username changed; type a new one."
                            .into(),
                    );
                    cx.notify();
                }
                Ok(()) => cx.emit(DismissEvent),
                Err(error) => {
                    this.error = Some(format!("Saving failed: {error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_server_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_selection = self.selected.is_some();
        let can_default = has_selection && self.project.is_some();
        let toolbar = h_flex()
            .gap_0p5()
            .child(
                IconButton::new("zenith-deploy-add", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Add Server"))
                    .aria_label("Add Server")
                    .on_click(cx.listener(|this, _, window, cx| this.add_server(window, cx))),
            )
            .child(
                IconButton::new("zenith-deploy-remove", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .disabled(!has_selection)
                    .tooltip(Tooltip::text("Remove Server"))
                    .aria_label("Remove Server")
                    .on_click(cx.listener(|this, _, window, cx| this.remove_server(window, cx))),
            )
            .child(
                IconButton::new("zenith-deploy-default", IconName::Check)
                    .icon_size(IconSize::Small)
                    .disabled(!can_default)
                    .tooltip(Tooltip::text("Use as Default for This Project"))
                    .aria_label("Use as Default for This Project")
                    .on_click(cx.listener(|this, _, _, cx| this.set_default(cx))),
            );
        let rows =
            self.servers
                .servers
                .iter()
                .enumerate()
                .map(|(index, server)| {
                    let is_default = self.is_default(server);
                    let label = if server.name.is_empty() {
                        "Unnamed".to_string()
                    } else {
                        server.name.clone()
                    };
                    ListItem::new(("zenith-deploy-server", index))
                        .toggle_state(self.selected == Some(index))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(label).when(is_default, |label| {
                                        label.weight(gpui::FontWeight::BOLD)
                                    }),
                                )
                                .when(is_default, |row| {
                                    row.child(Label::new("(default)").color(Color::Muted))
                                }),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select(Some(index), window, cx)
                        }))
                });
        v_flex().w(rems(14.)).gap_1().child(toolbar).child(
            v_flex()
                .id("zenith-deploy-server-list")
                .h(rems(22.))
                .overflow_y_scroll()
                .border_1()
                .border_color(cx.theme().colors().border)
                .rounded_sm()
                .children(rows)
                .when(self.servers.servers.is_empty(), |list| {
                    list.child(
                        div()
                            .p_2()
                            .child(Label::new("No servers. Add one with +.").color(Color::Muted)),
                    )
                }),
        )
    }

    fn row(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(
                div()
                    .w(rems(7.))
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

    fn render_form(&self, server: &ServerConfig, cx: &mut Context<Self>) -> impl IntoElement {
        let types = h_flex()
            .gap_1()
            .child(
                div()
                    .w(rems(7.))
                    .child(Label::new("Type").color(Color::Muted)),
            )
            .children(
                [
                    (ServerType::Ftps, "FTPS"),
                    (ServerType::Ftp, "FTP"),
                    (ServerType::Sftp, "SFTP"),
                ]
                .into_iter()
                .map(|(server_type, label)| {
                    Button::new(
                        SharedString::from(format!("zenith-deploy-type-{label}")),
                        label,
                    )
                    .toggle_state(server.server_type == server_type)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_type(server_type, window, cx)
                    }))
                }),
            );
        let is_ftp = server.server_type != ServerType::Sftp;
        let project_only = server.project.is_some();
        let has_project = self.project.is_some();
        let advanced = &server.advanced;
        v_flex()
            .flex_1()
            .gap_1p5()
            .child(Self::row("Name", &self.fields.name, cx))
            .child(types)
            .child(Self::row("Host", &self.fields.host, cx))
            .child(Self::row("Port", &self.fields.port, cx))
            .when(self.port_invalid, |form| {
                form.child(Label::new("Port must be a number from 1 to 65535.").color(Color::Error))
            })
            .child(Self::row("Username", &self.fields.username, cx))
            .when(is_ftp, |form| {
                form.child(
                    Checkbox::new("zenith-deploy-anonymous", server.anonymous.into())
                        .label("Anonymous login")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.update_selected(cx, |server| server.anonymous = !server.anonymous)
                        })),
                )
            })
            .when(is_ftp && !server.anonymous, |form| {
                form.child(Self::row("Password", &self.fields.password, cx))
                    .child(
                        Checkbox::new("zenith-deploy-save-password", server.save_password.into())
                            .label("Save password in the Keychain")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_selected(cx, |server| {
                                    server.save_password = !server.save_password
                                })
                            })),
                    )
            })
            .when(!is_ftp, |form| {
                form.child(
                    Label::new("SFTP signs in with your SSH key or agent; the host key must already be in known_hosts.")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
            })
            .child(Self::row("Root path", &self.fields.root_path, cx))
            .child(Self::row("Web URL", &self.fields.web_url, cx))
            .child(
                Checkbox::new("zenith-deploy-project-only", project_only.into())
                    .label("Visible only for this project")
                    .disabled(!has_project && !project_only)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let project = this.project.clone();
                        this.update_selected(cx, |server| {
                            server.project = match server.project {
                                Some(_) => None,
                                None => project,
                            }
                        })
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Disclosure::new("zenith-deploy-advanced-toggle", self.advanced_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.advanced_open = !this.advanced_open;
                                cx.notify();
                            })),
                    )
                    .child(Label::new("Advanced")),
            )
            .when(self.advanced_open && is_ftp, |form| {
                form.child(
                    v_flex()
                        .pl_4()
                        .gap_1()
                        .child(
                            Checkbox::new("zenith-deploy-passive", advanced.passive.into())
                                .label("Passive mode")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_selected(cx, |server| {
                                        server.advanced.passive = !server.advanced.passive
                                    })
                                })),
                        )
                        .child(
                            Checkbox::new("zenith-deploy-list", advanced.use_list.into())
                                .label("Use LIST instead of MLSD")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_selected(cx, |server| {
                                        server.advanced.use_list = !server.advanced.use_list
                                    })
                                })),
                        ),
                )
            })
            .when(self.advanced_open, |form| {
                form.child(
                    div().pl_4().child(
                        Checkbox::new("zenith-deploy-hidden", advanced.hidden_files.into())
                            .label("Show hidden files")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_selected(cx, |server| {
                                    server.advanced.hidden_files = !server.advanced.hidden_files
                                })
                            })),
                    ),
                )
            })
            .children(self.warnings().into_iter().map(|warning| {
                let (id, severity) = if warning == INSECURE_FTP {
                    ("zenith-deploy-insecure", Severity::Error)
                } else {
                    ("zenith-deploy-no-username", Severity::Warning)
                };
                div().debug_selector(|| id.to_string()).child(
                    Callout::new()
                        .severity(severity)
                        .icon(IconName::Warning)
                        .title(warning),
                )
            }))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.test_status {
            None => None,
            Some(TestStatus::Running) => Some((
                IconName::ArrowCircle,
                Color::Muted,
                SharedString::from("Connecting…"),
            )),
            Some(TestStatus::Passed(message)) => {
                Some((IconName::Check, Color::Success, message.clone()))
            }
            Some(TestStatus::Failed(message)) => {
                Some((IconName::Warning, Color::Error, message.clone()))
            }
        };
        let can_save =
            self.loaded && !self.port_invalid && self.store.read(cx).load_error().is_none();
        v_flex()
            .gap_1()
            .when_some(status, |footer, (icon, color, message)| {
                footer.child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(icon).color(color).size(IconSize::Small))
                        .child(Label::new(message).color(color)),
                )
            })
            .when_some(self.error.clone(), |footer, error| {
                footer.child(Label::new(error).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("zenith-deploy-test", "Test Connection")
                            .disabled(self.selected.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Button::new("zenith-deploy-cancel", "Cancel").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.cancel(&menu::Cancel, window, cx)
                                }),
                            ))
                            .child(
                                Button::new("zenith-deploy-ok", "OK")
                                    .style(ButtonStyle::Filled)
                                    .disabled(!can_save)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    ),
            )
    }
}

impl Render for DeploymentDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let form = match self.selected_server().cloned() {
            Some(server) => self.render_form(&server, cx).into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new(if self.loaded {
                        "Select a server or add one."
                    } else {
                        "Loading servers…"
                    })
                    .color(Color::Muted),
                )
                .into_any_element(),
        };
        v_flex()
            .key_context("DeploymentDialog")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(48.))
            .p_3()
            .gap_2()
            .child(Label::new("Deployment").weight(gpui::FontWeight::BOLD))
            .child(
                Label::new("Connection")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(self.render_server_list(cx))
                    .child(form),
            )
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeRemote, init, server, workspace};
    use gpui::{TestAppContext, VisualTestContext};

    async fn open_dialog<'a>(
        servers: Servers,
        remote: &FakeRemote,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<DeploymentDialog>,
        &'a mut VisualTestContext,
    ) {
        let cache = tempfile::tempdir().expect("tempdir");
        open_dialog_in(servers, remote, cache.path().to_path_buf(), cx).await
    }

    async fn open_dialog_in<'a>(
        servers: Servers,
        remote: &FakeRemote,
        cache_dir: std::path::PathBuf,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<DeploymentDialog>,
        &'a mut VisualTestContext,
    ) {
        let app_state = init(cx, servers, remote, cache_dir);
        let (workspace, cx) = workspace(&app_state, cx).await;
        cx.dispatch_action(OpenDeployment);
        cx.run_until_parked();
        let dialog = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .expect("the dialog opens as a modal");
        (workspace, dialog, cx)
    }

    #[gpui::test]
    async fn add_remove_and_default_server(cx: &mut TestAppContext) {
        let (workspace, dialog, cx) =
            open_dialog(Servers::default(), &FakeRemote::default(), cx).await;
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.add_server(window, cx);
            dialog.add_server(window, cx);
        });
        let (names, selected) = dialog.read_with(cx, |dialog, _| {
            let names: Vec<_> = dialog
                .servers()
                .servers
                .iter()
                .map(|server| server.name.clone())
                .collect();
            (names, dialog.selected_server().cloned().expect("selected"))
        });
        assert_eq!(names, ["Server 1", "Server 2"]);
        assert_eq!(
            selected.server_type,
            ServerType::Ftps,
            "FTPS is the default"
        );
        assert_eq!(selected.port, 21);

        dialog.update(cx, |dialog, cx| dialog.set_default(cx));
        let defaults = dialog.read_with(cx, |dialog, _| dialog.servers().project_defaults.clone());
        assert_eq!(
            defaults.get("/p").map(String::as_str),
            Some(selected.id.as_str())
        );

        dialog.update_in(cx, |dialog, window, cx| dialog.remove_server(window, cx));
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.servers().servers.len(), 1);
            assert!(
                dialog.servers().project_defaults.is_empty(),
                "a removed server is no project's default"
            );
            assert_eq!(
                dialog.selected_server().map(|server| server.name.as_str()),
                Some("Server 1")
            );
        });

        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
        assert_eq!(saved.servers.len(), 1);
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<DeploymentDialog>(cx))
                .is_none(),
            "OK closes the dialog"
        );
    }

    #[gpui::test]
    async fn username_warning_and_insecure_ftp_callout(cx: &mut TestAppContext) {
        let (_, dialog, cx) = open_dialog(Servers::default(), &FakeRemote::default(), cx).await;
        dialog.update_in(cx, |dialog, window, cx| dialog.add_server(window, cx));
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.warnings()),
            [NO_USERNAME]
        );
        assert!(cx.debug_bounds("zenith-deploy-no-username").is_some());
        assert!(cx.debug_bounds("zenith-deploy-insecure").is_none());

        dialog.update_in(cx, |dialog, window, cx| {
            dialog.set_type(ServerType::Ftp, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zenith-deploy-insecure").is_some());
        assert!(
            dialog
                .read_with(cx, |dialog, _| dialog.warnings())
                .contains(&INSECURE_FTP)
        );

        let username = dialog.read_with(cx, |dialog, _| dialog.fields.username.clone());
        username.update_in(cx, |editor, window, cx| {
            editor.set_text("deploy", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog
                .selected_server()
                .map(|server| server.username.clone())),
            Some("deploy".to_string())
        );
        assert!(cx.debug_bounds("zenith-deploy-no-username").is_none());

        dialog.update_in(cx, |dialog, window, cx| {
            dialog.set_type(ServerType::Sftp, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zenith-deploy-insecure").is_none());
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.selected_server().map(|s| s.port)),
            Some(22),
            "the port follows the type while it is the default"
        );
    }

    #[gpui::test]
    async fn test_connection_uses_the_typed_password_and_reports_the_result(
        cx: &mut TestAppContext,
    ) {
        let remote = FakeRemote::default();
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &remote, cx).await;
        let password = dialog.read_with(cx, |dialog, _| dialog.fields.password.clone());
        password.update_in(cx, |editor, window, cx| {
            editor.set_text("s3cret", window, cx)
        });
        dialog.update(cx, |dialog, cx| dialog.test_connection(cx));
        cx.run_until_parked();
        assert_eq!(
            remote.log(),
            [
                "connect live.example.com password=Some(\"s3cret\")",
                "disconnect"
            ]
        );
        dialog.read_with(cx, |dialog, _| match &dialog.test_status {
            Some(TestStatus::Passed(message)) => assert!(message.contains("/www"), "{message}"),
            _ => panic!("expected a passed test"),
        });
    }

    /// Plainly fake; built here so no test spells a secret literal.
    fn fake_secret() -> String {
        format!("not-a-real-value-{}", 1)
    }

    #[gpui::test]
    async fn saving_a_changed_target_without_a_typed_secret_forgets_the_session_secret(
        cx: &mut TestAppContext,
    ) {
        let mut saved = server("Live", None);
        saved.save_password = false;
        let saved_server = saved.clone();
        let servers = Servers {
            servers: vec![saved],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| {
            store.set_session_password(&saved_server, Some(Password(fake_secret())))
        });
        let usable = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store.session_password_for(&store.servers().servers[0])
            })
        };

        // A rename keeps the target: the secret stays.
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "Renamed".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(usable(cx).is_some());

        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(
            usable(cx).is_none(),
            "the secret typed for the old host is not usable at the new one"
        );
        dialog.read_with(cx, |dialog, _| {
            let message = dialog.error.as_ref().map(|error| error.to_string());
            assert!(
                message.is_some_and(|message| message.contains("password was removed")),
                "the user is told why"
            );
        });
    }

    /// Two saved servers: `edited` (host changed, a password typed, saved with the given
    /// `save_password`) and `removed`, whose Keychain delete fails. Returns the Save outcome.
    async fn aborted_save(
        save_password: bool,
        cx: &mut TestAppContext,
    ) -> (
        Entity<DeploymentDialog>,
        Entity<ServerStore>,
        std::sync::Arc<crate::secrets::fake::FakeKeychain>,
        ServerConfig,
        &mut VisualTestContext,
    ) {
        let mut edited = server("Live", None);
        edited.save_password = save_password;
        let doomed = server("Doomed", None);
        let servers = Servers {
            servers: vec![edited.clone(), doomed.clone()],
            ..Servers::default()
        };
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        keychain.put(&edited, "not-a-real-value-0");
        keychain.put(&doomed, "not-a-real-value-0");
        *keychain.fail_delete.lock().expect("lock") = true;
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| store.keychain = keychain.clone());
        dialog.update(cx, |dialog, cx| {
            dialog
                .servers
                .servers
                .retain(|server| server.id != doomed.id);
            dialog.removed.push(doomed.id.clone());
            dialog.servers.servers[0].host = "elsewhere.example.com".into();
            dialog.passwords.insert(edited.id.clone(), fake_secret());
            dialog.save(cx);
        });
        cx.run_until_parked();
        (dialog, store, keychain, edited, cx)
    }

    #[gpui::test]
    async fn an_aborted_save_leaves_the_old_target_without_a_secret_for_the_new_one(
        cx: &mut TestAppContext,
    ) {
        let (dialog, store, _, edited, cx) = aborted_save(false, cx).await;
        let (host, usable) = store.read_with(cx, |store, _| {
            let saved = &store.servers().servers[0];
            (saved.host.clone(), store.session_password_for(saved))
        });
        assert_eq!(host, edited.host, "the old target is still the saved one");
        assert!(
            usable.is_none(),
            "the new target's secret is not used at the old one"
        );
        dialog.read_with(cx, |dialog, _| {
            let message = dialog.error.as_ref().map(|error| error.to_string());
            assert!(
                message.is_some_and(|message| message.contains("Nothing was saved")),
                "the user is told the save was aborted"
            );
        });
    }

    #[gpui::test]
    async fn an_aborted_save_leaves_the_saved_keychain_entry_alone(cx: &mut TestAppContext) {
        let (_, _, keychain, edited, _) = aborted_save(true, cx).await;
        assert_eq!(
            keychain.account_of(&edited),
            Some(crate::secrets::target_account(&edited)),
            "the working saved password still belongs to the saved target"
        );
    }

    #[gpui::test]
    async fn no_note_is_shown_when_there_was_no_password_to_remove(cx: &mut TestAppContext) {
        let mut unsaved = server("Live", None);
        unsaved.save_password = false;
        let servers = Servers {
            servers: vec![unsaved],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| assert!(dialog.error.is_none()));
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<DeploymentDialog>(cx))
                .is_none()
        );
    }

    #[gpui::test]
    async fn changing_the_target_of_a_passwordless_server_saves_quietly(cx: &mut TestAppContext) {
        let mut anonymous = server("Live", None);
        anonymous.anonymous = true;
        let servers = Servers {
            servers: vec![anonymous],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| assert!(dialog.error.is_none()));
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<DeploymentDialog>(cx))
                .is_none(),
            "no password was removed, so there is nothing to tell the user"
        );
    }

    #[gpui::test]
    async fn test_connection_never_sends_the_saved_secret_to_a_changed_target(
        cx: &mut TestAppContext,
    ) {
        let remote = FakeRemote::default();
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let saved_server = servers.servers[0].clone();
        let (_, dialog, cx) = open_dialog(servers, &remote, cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| {
            store.set_session_password(&saved_server, Some(Password(fake_secret())))
        });
        let original = dialog.read_with(cx, |dialog, _| dialog.selected_server().cloned().unwrap());

        // Control: the saved target still gets the stored secret.
        dialog.update(cx, |dialog, cx| dialog.test_connection(cx));
        cx.run_until_parked();
        let log = remote.log();
        assert_eq!(log.len(), 2, "connect and disconnect");
        assert!(
            log[0].contains(&fake_secret()),
            "the connect carried the secret: {log:?}"
        );
        remote.log.lock().expect("lock").clear();

        let changes: [fn(&mut ServerConfig); 4] = [
            |server| server.host = "elsewhere.example.com".into(),
            |server| server.port += 1,
            |server| server.server_type = ServerType::Ftp,
            |server| server.username = "someone-else".into(),
        ];
        for change in changes {
            // Each case starts from the saved server: changes must not stack.
            dialog.update(cx, |dialog, cx| {
                dialog.update_selected(cx, |server| *server = original.clone());
                dialog.update_selected(cx, change);
                dialog.test_connection(cx);
            });
            cx.run_until_parked();
            dialog.read_with(cx, |dialog, _| match &dialog.test_status {
                Some(TestStatus::Failed(message)) => {
                    assert!(message.contains("Type the password"), "{message}")
                }
                _ => panic!("expected the changed target to be refused"),
            });
            assert!(
                remote.log().is_empty(),
                "nothing connects: {:?}",
                remote.log()
            );
        }
    }

    #[gpui::test]
    async fn removing_a_server_deletes_its_cached_copies(cx: &mut TestAppContext) {
        let cache = tempfile::tempdir().expect("tempdir");
        let live = server("Live", None);
        let cached = cache.path().join(&live.id);
        std::fs::create_dir_all(&cached).expect("mkdir");
        std::fs::write(cached.join("wp-config.php"), "<?php").expect("write");
        let servers = Servers {
            servers: vec![live],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog_in(
            servers,
            &FakeRemote::default(),
            cache.path().to_path_buf(),
            cx,
        )
        .await;
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_server(window, cx);
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(!cached.exists());
    }

    #[gpui::test]
    async fn a_failed_test_connection_shows_the_error(cx: &mut TestAppContext) {
        let remote = FakeRemote {
            fail_connect: true,
            ..FakeRemote::default()
        };
        let mut live = server("Live", None);
        live.anonymous = true;
        let servers = Servers {
            servers: vec![live],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &remote, cx).await;
        dialog.update(cx, |dialog, cx| dialog.test_connection(cx));
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| match &dialog.test_status {
            Some(TestStatus::Failed(message)) => {
                assert!(message.contains("530 Login incorrect"), "{message}")
            }
            _ => panic!("expected a failed test"),
        });
    }
}
