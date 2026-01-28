use crate::{
    remote_connections::{
        Connection, RemoteConnectionModal, RemoteConnectionPrompt, RemoteSettings, SshConnection,
        SshConnectionHeader, connect, determine_paths_with_positions, open_remote_project,
        upsert_dev_container_connection,
    },
    ssh_config::{SshConfigEntry, parse_ssh_config_entries, parse_ssh_config_hosts},
};
use dev_container::{
    DevContainerBuildStep, DevContainerLogLine, DevContainerLogStream, DevContainerProgressEvent,
    start_dev_container_with_progress,
};
use editor::{Editor, EditorEvent};
use file_finder::OpenPathDelegate;
use futures::{
    FutureExt, StreamExt as _,
    channel::{mpsc, oneshot},
    future::Shared,
    select,
};
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FutureExt as _, PathPromptOptions, PromptLevel, ScrollHandle,
    Subscription, Task, WeakEntity, Window, canvas,
};
use language::{Point, language_settings::SoftWrap};
use log::info;
use paths::{global_ssh_config_file, user_ssh_config_file};
use picker::Picker;
use project::{Fs, Project};
use remote::{
    DockerHost, RemoteClient, RemoteConnectionOptions, SshConnectionOptions, WslConnectionOptions,
    parse_port_forward_spec, remote_client::ConnectionIdentifier,
};
use settings::{
    DevContainerConnection, DevContainerHost, RemoteProject, RemoteSettingsContent, Settings as _,
    SettingsStore, SshPortForwardOption, update_settings_file, watch_config_file,
};
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{self, AtomicUsize},
    },
    time::Duration,
};
use ui::{
    ButtonStyle, Callout, CommonAnimationExt, ContextMenu, CopyButton, IconButtonShape, KeyBinding,
    List, ListItem, ListSeparator, Modal, ModalHeader, Navigable, NavigableEntry, PopoverMenu,
    Section, TintColor, ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip,
    WithScrollbar, prelude::*,
};
use util::{
    ResultExt,
    command::new_smol_command,
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
    shell::ShellKind,
};
use workspace::{
    ModalView, OpenOptions, Toast, Workspace,
    notifications::{DetachAndPromptErr, NotificationId},
    open_remote_project_with_existing_connection,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteProjectsTab {
    Ssh,
    DevContainers,
    Wsl,
}

pub struct RemoteServerProjects {
    mode: Mode,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    retained_connections: Vec<Entity<RemoteClient>>,
    ssh_config_updates: Task<()>,
    ssh_config_servers: BTreeSet<SharedString>,
    ssh_config_entries: HashMap<String, SshConfigEntry>,
    create_new_window: bool,
    selected_tab: RemoteProjectsTab,
    selected_entry: Option<RemoteEntryKey>,
    ssh_search_editor: Entity<Editor>,
    wsl_search_editor: Entity<Editor>,
    dev_container_search_editor: Entity<Editor>,
    ssh_page: usize,
    wsl_page: usize,
    dev_container_page: usize,
    _ssh_search_subscription: Subscription,
    _wsl_search_subscription: Subscription,
    _dev_container_search_subscription: Subscription,
    saved_connections_search_subscription: Option<Subscription>,
    ssh_config_search_subscription: Option<Subscription>,
    _subscription: Subscription,
}

const START_PROXY_TIMEOUT: Duration = Duration::from_secs(90);
const START_PROXY_TIMEOUT_WITH_BUILD: Duration = Duration::from_secs(6 * 60);
const DEV_CONTAINER_PROBE_TIMEOUT: Duration = Duration::from_secs(6);
const DEV_CONTAINER_PROBE_CONCURRENCY: usize = 6;
const SAVED_CONNECTIONS_LIMIT: usize = 5;
const REMOTE_SERVERS_PAGE_SIZE: usize = 5;

struct CreateRemoteServer {
    input_mode: CreateRemoteServerInputMode,
    address_editor: Entity<Editor>,
    form: CreateRemoteServerForm,
    show_advanced: bool,
    saved_connections_search_editor: Entity<Editor>,
    saved_connections_page: usize,
    ssh_config_hosts_page: usize,
    ssh_config_search_editor: Entity<Editor>,
    quick_pick_tab: QuickPickTab,
    form_errors: CreateRemoteServerFormErrors,
    password_visible: bool,
    password_keychain_status: PasswordKeychainStatus,
    password_keychain_url: Option<String>,
    password_autoload_url: Option<String>,
    address_error: Option<SharedString>,
    ssh_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<Option<()>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CreateRemoteServerInputMode {
    Form,
    Command,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuickPickTab {
    SshConfig,
    SavedConnections,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PasswordKeychainStatus {
    Unknown,
    Loading,
    Saved,
    NotSaved,
    Error,
}

#[derive(Clone)]
struct CreateRemoteServerForm {
    host_editor: Entity<Editor>,
    username_editor: Entity<Editor>,
    port_editor: Entity<Editor>,
    nickname_editor: Entity<Editor>,
    password_editor: Entity<Editor>,
    identity_file_editor: Entity<Editor>,
    jump_host_editor: Entity<Editor>,
    port_forwards_editor: Entity<Editor>,
}

#[derive(Clone, Default)]
struct CreateRemoteServerFormErrors {
    host: Option<SharedString>,
    port: Option<SharedString>,
    port_forwards: Vec<SharedString>,
}

impl CreateRemoteServerFormErrors {
    fn is_empty(&self) -> bool {
        self.host.is_none() && self.port.is_none() && self.port_forwards.is_empty()
    }
}

impl CreateRemoteServer {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let address_editor = cx.new(|cx| Editor::single_line(window, cx));
        let host_editor = cx.new(|cx| Editor::single_line(window, cx));
        let username_editor = cx.new(|cx| Editor::single_line(window, cx));
        let port_editor = cx.new(|cx| Editor::single_line(window, cx));
        let nickname_editor = cx.new(|cx| Editor::single_line(window, cx));
        let password_editor = cx.new(|cx| Editor::single_line(window, cx));
        let identity_file_editor = cx.new(|cx| Editor::single_line(window, cx));
        let jump_host_editor = cx.new(|cx| Editor::single_line(window, cx));
        let port_forwards_editor = cx.new(|cx| Editor::single_line(window, cx));
        let saved_connections_search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search saved connections...", window, cx);
            editor
        });
        let ssh_config_search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search SSH config hosts...", window, cx);
            editor
        });

        address_editor.update(cx, |this, cx| {
            this.focus_handle(cx).focus(window, cx);
        });

        host_editor.update(cx, |this, cx| {
            this.set_placeholder_text("example.com", window, cx);
        });
        username_editor.update(cx, |this, cx| {
            this.set_placeholder_text("user", window, cx);
        });
        port_editor.update(cx, |this, cx| {
            this.set_placeholder_text("22", window, cx);
        });
        nickname_editor.update(cx, |this, cx| {
            this.set_placeholder_text("Defaults to user@host[:port]", window, cx);
        });
        password_editor.update(cx, |this, cx| {
            this.set_placeholder_text("password", window, cx);
            this.set_masked(true, cx);
        });
        identity_file_editor.update(cx, |this, cx| {
            this.set_placeholder_text("~/.ssh/id_ed25519", window, cx);
        });
        jump_host_editor.update(cx, |this, cx| {
            this.set_placeholder_text("bastion.example.com", window, cx);
        });
        port_forwards_editor.update(cx, |this, cx| {
            this.set_placeholder_text("8080:localhost:80", window, cx);
        });

        host_editor.focus_handle(cx).focus(window, cx);
        Self {
            input_mode: CreateRemoteServerInputMode::Form,
            address_editor,
            form: CreateRemoteServerForm {
                host_editor,
                username_editor,
                port_editor,
                nickname_editor,
                password_editor,
                identity_file_editor,
                jump_host_editor,
                port_forwards_editor,
            },
            show_advanced: false,
            saved_connections_search_editor,
            saved_connections_page: 0,
            ssh_config_hosts_page: 0,
            ssh_config_search_editor,
            quick_pick_tab: QuickPickTab::SavedConnections,
            form_errors: CreateRemoteServerFormErrors::default(),
            password_visible: false,
            password_keychain_status: PasswordKeychainStatus::Unknown,
            password_keychain_url: None,
            password_autoload_url: None,
            address_error: None,
            ssh_prompt: None,
            _creating: None,
        }
    }

    fn rebuild_with(
        &self,
        address_error: Option<SharedString>,
        ssh_prompt: Option<Entity<RemoteConnectionPrompt>>,
        creating: Option<Task<Option<()>>>,
        form_errors: Option<CreateRemoteServerFormErrors>,
    ) -> Self {
        Self {
            input_mode: self.input_mode,
            address_editor: self.address_editor.clone(),
            form: self.form.clone(),
            show_advanced: self.show_advanced,
            saved_connections_search_editor: self.saved_connections_search_editor.clone(),
            saved_connections_page: self.saved_connections_page,
            ssh_config_hosts_page: self.ssh_config_hosts_page,
            ssh_config_search_editor: self.ssh_config_search_editor.clone(),
            quick_pick_tab: self.quick_pick_tab,
            form_errors: form_errors.unwrap_or_else(|| self.form_errors.clone()),
            password_visible: self.password_visible,
            password_keychain_status: self.password_keychain_status,
            password_keychain_url: self.password_keychain_url.clone(),
            password_autoload_url: self.password_autoload_url.clone(),
            address_error,
            ssh_prompt,
            _creating: creating,
        }
    }

    fn snapshot(&self) -> Self {
        Self {
            input_mode: self.input_mode,
            address_editor: self.address_editor.clone(),
            form: self.form.clone(),
            show_advanced: self.show_advanced,
            saved_connections_search_editor: self.saved_connections_search_editor.clone(),
            saved_connections_page: self.saved_connections_page,
            ssh_config_hosts_page: self.ssh_config_hosts_page,
            ssh_config_search_editor: self.ssh_config_search_editor.clone(),
            quick_pick_tab: self.quick_pick_tab,
            form_errors: self.form_errors.clone(),
            password_visible: self.password_visible,
            password_keychain_status: self.password_keychain_status,
            password_keychain_url: self.password_keychain_url.clone(),
            password_autoload_url: self.password_autoload_url.clone(),
            address_error: self.address_error.clone(),
            ssh_prompt: self.ssh_prompt.clone(),
            _creating: None,
        }
    }

    fn set_read_only(&self, read_only: bool, cx: &mut App) {
        let set_read_only = |editor: &Entity<Editor>, cx: &mut App| {
            editor.update(cx, |editor, _| editor.set_read_only(read_only));
        };
        set_read_only(&self.address_editor, cx);
        set_read_only(&self.form.host_editor, cx);
        set_read_only(&self.form.username_editor, cx);
        set_read_only(&self.form.port_editor, cx);
        set_read_only(&self.form.nickname_editor, cx);
        set_read_only(&self.form.password_editor, cx);
        set_read_only(&self.form.identity_file_editor, cx);
        set_read_only(&self.form.jump_host_editor, cx);
        set_read_only(&self.form.port_forwards_editor, cx);
        set_read_only(&self.saved_connections_search_editor, cx);
        set_read_only(&self.ssh_config_search_editor, cx);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DevContainerCreationProgress {
    Initial,
    Creating,
    Success,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevContainerStepStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug)]
struct DevContainerUiStep {
    label: SharedString,
    status: DevContainerStepStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevContainerBuildStatus {
    Running,
    Success,
    Failed,
}

#[derive(Clone)]
struct DevContainerBuildState {
    steps: [DevContainerUiStep; 3],
    prepare_completed: usize,
    selected_step: usize,
    status: DevContainerBuildStatus,
    log_editor: Entity<Editor>,
    log_contents: String,
    back_entry: NavigableEntry,
    close_entry: NavigableEntry,
    copy_entry: NavigableEntry,
    finish_entry: NavigableEntry,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct DevContainerKey {
    container_id: String,
    use_podman: bool,
    host: Option<DevContainerHost>,
}

impl DevContainerKey {
    fn from_connection(connection: &DevContainerConnection) -> Self {
        Self {
            container_id: connection.container_id.clone(),
            use_podman: connection.use_podman,
            host: connection.host.clone(),
        }
    }
}

fn dev_container_key_from_remote_options(
    options: &RemoteConnectionOptions,
) -> Option<DevContainerKey> {
    let RemoteConnectionOptions::Docker(options) = options else {
        return None;
    };

    let host = match &options.host {
        DockerHost::Local => None,
        DockerHost::Wsl(options) => Some(DevContainerHost::Wsl {
            distro_name: options.distro_name.clone(),
            user: options.user.clone(),
        }),
        DockerHost::Ssh(options) => Some(DevContainerHost::Ssh {
            host: options.host.to_string(),
            username: options.username.clone(),
            port: options.port,
            args: options.args.clone().unwrap_or_default(),
        }),
    };

    Some(DevContainerKey {
        container_id: options.container_id.clone(),
        use_podman: options.use_podman,
        host,
    })
}

fn devcontainer_error_hints(message: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let message_lower = message.to_ascii_lowercase();

    let push_hint = |hint: &str, hints: &mut Vec<String>| {
        if !hints.iter().any(|existing| existing == hint) {
            hints.push(hint.to_string());
        }
    };

    if message.contains("Dev Container CLI not available")
        || message_lower.contains("devcontainer cli")
    {
        push_hint(
            "Install the Dev Container CLI on the project host (WSL/SSH): npm install -g @devcontainers/cli",
            &mut hints,
        );
        push_hint("Verify it is on PATH: devcontainer --version", &mut hints);
        push_hint(
            "Quick WSL check: wsl -d <distro> -- sh -lc 'command -v devcontainer'",
            &mut hints,
        );
        push_hint(
            "Ensure PATH is set for login shells (e.g. ~/.bash_profile, ~/.profile, ~/.zprofile).",
            &mut hints,
        );
        push_hint(
            "If using mise, add ~/.local/share/mise/bin and ~/.local/share/mise/shims to PATH in your login shell config.",
            &mut hints,
        );
        push_hint(
            "Run the commands on the host where the project lives, not on Windows.",
            &mut hints,
        );
    }

    if message.contains("Docker CLI not found")
        || message_lower.contains("docker cli not available")
        || message_lower.contains("unable to run docker")
    {
        push_hint(
            "Install Docker or Podman on the project host and ensure it is on PATH.",
            &mut hints,
        );
        push_hint(
            "Verify it is available: docker --version (or podman --version).",
            &mut hints,
        );
        push_hint(
            "Run the commands on the host where the project lives, not on Windows.",
            &mut hints,
        );
    }

    if message.contains("No valid dev container definition") {
        push_hint(
            "Ensure .devcontainer/devcontainer.json exists in the project root.",
            &mut hints,
        );
    }

    if message.contains("Failed to parse file .devcontainer/devcontainer.json") {
        push_hint(
            "Fix JSON syntax errors in .devcontainer/devcontainer.json.",
            &mut hints,
        );
    }

    if message_lower.contains("failed to download the remote server binary")
        || message_lower.contains("cannot upload local files to a container over ssh")
        || message_lower.contains("uploading a local binary over ssh is not supported")
    {
        push_hint(
            "Ensure the container has outbound internet access and curl or wget installed.",
            &mut hints,
        );
    }

    if message.contains("Dev Containers are only supported for local, WSL, or SSH projects") {
        push_hint(
            "Open the project as Local, WSL, or SSH before creating a dev container.",
            &mut hints,
        );
    }

    hints
}

#[derive(Clone)]
struct CreateRemoteDevContainer {
    // 3 Navigable Options
    // - Create from devcontainer.json
    // - Edit devcontainer.json
    // - Go back
    entries: [NavigableEntry; 3],
    progress: DevContainerCreationProgress,
    build_state: Option<DevContainerBuildState>,
}

impl CreateRemoteDevContainer {
    fn new(window: &mut Window, cx: &mut Context<RemoteServerProjects>) -> Self {
        let entries = std::array::from_fn(|_| NavigableEntry::focusable(cx));
        entries[0].focus_handle.focus(window, cx);
        Self {
            entries,
            progress: DevContainerCreationProgress::Initial,
            build_state: None,
        }
    }

    fn with_progress(
        mut self,
        progress: DevContainerCreationProgress,
        window: &mut Window,
        cx: &mut Context<RemoteServerProjects>,
    ) -> Self {
        self.progress = progress;
        if !matches!(self.progress, DevContainerCreationProgress::Initial)
            && self.build_state.is_none()
        {
            self.build_state = Some(DevContainerBuildState::new(window, cx));
        }
        self
    }
}

impl DevContainerBuildState {
    fn step_index(step: DevContainerBuildStep) -> usize {
        match step {
            DevContainerBuildStep::CheckDocker | DevContainerBuildStep::CheckDevcontainerCli => 0,
            DevContainerBuildStep::DevcontainerUp => 1,
            DevContainerBuildStep::ReadConfiguration => 2,
        }
    }

    fn new(window: &mut Window, cx: &mut Context<RemoteServerProjects>) -> Self {
        let log_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(String::new(), window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
            editor.hide_minimap_by_default(window, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_input_enabled(false);
            editor.set_use_autoclose(false);
            editor.set_read_only(true);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor
        });

        let steps = [
            DevContainerUiStep {
                label: SharedString::from("1. Environment checks"),
                status: DevContainerStepStatus::Pending,
            },
            DevContainerUiStep {
                label: SharedString::from("2. Build dev container"),
                status: DevContainerStepStatus::Pending,
            },
            DevContainerUiStep {
                label: SharedString::from("3. Read config and connect"),
                status: DevContainerStepStatus::Pending,
            },
        ];

        Self {
            steps,
            prepare_completed: 0,
            selected_step: 0,
            status: DevContainerBuildStatus::Running,
            log_editor,
            log_contents: String::new(),
            back_entry: NavigableEntry::focusable(cx),
            close_entry: NavigableEntry::focusable(cx),
            copy_entry: NavigableEntry::focusable(cx),
            finish_entry: NavigableEntry::focusable(cx),
        }
    }

    fn start_step(&mut self, step: DevContainerBuildStep) {
        self.selected_step = Self::step_index(step);
        match step {
            DevContainerBuildStep::CheckDocker | DevContainerBuildStep::CheckDevcontainerCli => {
                self.steps[0].status = DevContainerStepStatus::Running;
            }
            DevContainerBuildStep::DevcontainerUp => {
                self.steps[1].status = DevContainerStepStatus::Running;
            }
            DevContainerBuildStep::ReadConfiguration => {
                self.steps[2].status = DevContainerStepStatus::Running;
            }
        }
    }

    fn complete_step(&mut self, step: DevContainerBuildStep) {
        self.selected_step = Self::step_index(step);
        match step {
            DevContainerBuildStep::CheckDocker | DevContainerBuildStep::CheckDevcontainerCli => {
                self.prepare_completed += 1;
                if self.prepare_completed >= 2 {
                    self.steps[0].status = DevContainerStepStatus::Completed;
                }
            }
            DevContainerBuildStep::DevcontainerUp => {
                self.steps[1].status = DevContainerStepStatus::Completed;
            }
            DevContainerBuildStep::ReadConfiguration => {
                self.steps[2].status = DevContainerStepStatus::Completed;
                self.status = DevContainerBuildStatus::Success;
            }
        }
    }

    fn fail_step(&mut self, step: DevContainerBuildStep) {
        self.selected_step = Self::step_index(step);
        match step {
            DevContainerBuildStep::CheckDocker | DevContainerBuildStep::CheckDevcontainerCli => {
                self.steps[0].status = DevContainerStepStatus::Failed;
            }
            DevContainerBuildStep::DevcontainerUp => {
                self.steps[1].status = DevContainerStepStatus::Failed;
            }
            DevContainerBuildStep::ReadConfiguration => {
                self.steps[2].status = DevContainerStepStatus::Failed;
            }
        }
        self.status = DevContainerBuildStatus::Failed;
    }

    fn append_log_line(
        &mut self,
        line: DevContainerLogLine,
        window: &mut Window,
        cx: &mut Context<RemoteServerProjects>,
    ) {
        let mut formatted = match line.stream {
            DevContainerLogStream::Info => format!("[info] {}", line.line),
            DevContainerLogStream::Stdout => line.line,
            DevContainerLogStream::Stderr => format!("[stderr] {}", line.line),
        };
        if !formatted.ends_with('\n') {
            formatted.push('\n');
        }
        self.log_contents.push_str(&formatted);
        let log_contents = self.log_contents.clone();
        self.log_editor.update(cx, |editor, cx| {
            editor.set_text(log_contents, window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
    }

    fn select_previous_step(&mut self) {
        if self.selected_step > 0 {
            self.selected_step -= 1;
        }
    }
}

#[cfg(target_os = "windows")]
struct AddWslDistro {
    picker: Entity<Picker<crate::wsl_picker::WslPickerDelegate>>,
    connection_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<()>>,
}

#[cfg(target_os = "windows")]
impl AddWslDistro {
    fn new(window: &mut Window, cx: &mut Context<RemoteServerProjects>) -> Self {
        use crate::wsl_picker::{WslDistroSelected, WslPickerDelegate, WslPickerDismissed};

        let delegate = WslPickerDelegate::new();
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).modal(false));

        cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &WslDistroSelected, window, cx| {
                this.confirm(&menu::Confirm, window, cx);
            },
        )
        .detach();

        cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &WslPickerDismissed, window, cx| {
                this.cancel(&menu::Cancel, window, cx);
            },
        )
        .detach();

        AddWslDistro {
            picker,
            connection_prompt: None,
            _creating: None,
        }
    }
}

enum ProjectPickerData {
    Ssh {
        connection_string: SharedString,
        nickname: Option<SharedString>,
    },
    Wsl {
        distro_name: SharedString,
    },
    DevContainer {
        name: SharedString,
    },
}

struct ProjectPicker {
    data: ProjectPickerData,
    picker: Entity<Picker<OpenPathDelegate>>,
    _path_task: Shared<Task<Option<()>>>,
}

struct EditNicknameState {
    index: SshServerIndex,
    editor: Entity<Editor>,
}

struct EditDevContainerNameState {
    index: DevContainerIndex,
    editor: Entity<Editor>,
}

impl EditNicknameState {
    fn new(index: SshServerIndex, window: &mut Window, cx: &mut App) -> Self {
        let this = Self {
            index,
            editor: cx.new(|cx| Editor::single_line(window, cx)),
        };
        let starting_text = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(index.0)
            .and_then(|state| state.nickname)
            .filter(|text| !text.is_empty());
        this.editor.update(cx, |this, cx| {
            this.set_placeholder_text("Add a nickname for this server", window, cx);
            if let Some(starting_text) = starting_text {
                this.set_text(starting_text, window, cx);
            }
        });
        this.editor.focus_handle(cx).focus(window, cx);
        this
    }
}

impl EditDevContainerNameState {
    fn new(index: DevContainerIndex, window: &mut Window, cx: &mut App) -> Self {
        let this = Self {
            index,
            editor: cx.new(|cx| Editor::single_line(window, cx)),
        };
        let starting_text = RemoteSettings::get_global(cx)
            .dev_container_connections()
            .nth(index.0)
            .map(|state| state.name)
            .filter(|text| !text.is_empty());
        this.editor.update(cx, |this, cx| {
            this.set_placeholder_text("Rename this dev container", window, cx);
            if let Some(starting_text) = starting_text {
                this.set_text(starting_text, window, cx);
            }
        });
        this.editor.focus_handle(cx).focus(window, cx);
        this
    }
}

impl Focusable for ProjectPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ProjectPicker {
    fn new(
        create_new_window: bool,
        index: ServerIndex,
        connection: RemoteConnectionOptions,
        project: Entity<Project>,
        home_dir: RemotePathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<RemoteServerProjects>,
    ) -> Entity<Self> {
        let (tx, rx) = oneshot::channel();
        let lister = project::DirectoryLister::Project(project.clone());
        let delegate = file_finder::OpenPathDelegate::new(tx, lister, false, cx)
            .browse_directories()
            .show_hidden()
            .with_home_dir(home_dir.to_string());

        let picker = cx.new(|cx| {
            let picker = Picker::uniform_list(delegate, window, cx)
                .width(rems(34.))
                .modal(false);
            picker.set_query(home_dir.to_string(), window, cx);
            picker
        });

        let data = match &connection {
            RemoteConnectionOptions::Ssh(connection) => ProjectPickerData::Ssh {
                connection_string: connection.connection_string().into(),
                nickname: connection.nickname.clone().map(|nick| nick.into()),
            },
            RemoteConnectionOptions::Wsl(connection) => ProjectPickerData::Wsl {
                distro_name: connection.distro_name.clone().into(),
            },
            RemoteConnectionOptions::Docker(connection) => ProjectPickerData::DevContainer {
                name: connection.name.clone().into(),
            },
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(options) => ProjectPickerData::Ssh {
                connection_string: format!("mock-{}", options.id).into(),
                nickname: None,
            },
        };
        let _path_task = cx
            .spawn_in(window, {
                let workspace = workspace;
                async move |this, cx| {
                    let Ok(Some(paths)) = rx.await else {
                        workspace
                            .update_in(cx, |workspace, window, cx| {
                                let fs = workspace.project().read(cx).fs().clone();
                                let weak = cx.entity().downgrade();
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    RemoteServerProjects::new(
                                        create_new_window,
                                        fs,
                                        window,
                                        weak,
                                        cx,
                                    )
                                });
                            })
                            .log_err()?;
                        return None;
                    };

                    let app_state = workspace
                        .read_with(cx, |workspace, _| workspace.app_state().clone())
                        .ok()?;

                    let remote_connection = project.read_with(cx, |project, cx| {
                        project.remote_client()?.read(cx).connection()
                    })?;

                    let (paths, paths_with_positions) =
                        determine_paths_with_positions(&remote_connection, paths).await;

                    cx.update(|_, cx| {
                        let fs = app_state.fs.clone();
                        update_settings_file(fs, cx, {
                            let paths = paths
                                .iter()
                                .map(|path| path.to_string_lossy().into_owned())
                                .collect();
                            move |settings, _| match index {
                                ServerIndex::Ssh(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .ssh_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        server.projects.insert(RemoteProject { paths });
                                    };
                                }
                                ServerIndex::Wsl(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .wsl_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        server.projects.insert(RemoteProject { paths });
                                    };
                                }
                                ServerIndex::DevContainer(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .dev_container_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        server.projects.insert(RemoteProject { paths });
                                    };
                                }
                            }
                        });
                    })
                    .log_err();

                    let options = cx
                        .update(|_, cx| (app_state.build_window_options)(None, cx))
                        .log_err()?;
                    let window = cx
                        .open_window(options, |window, cx| {
                            cx.new(|cx| {
                                telemetry::event!("SSH Project Created");
                                Workspace::new(None, project.clone(), app_state.clone(), window, cx)
                            })
                        })
                        .log_err()?;

                    let items = open_remote_project_with_existing_connection(
                        connection, project, paths, app_state, window, cx,
                    )
                    .await
                    .log_err();

                    if let Some(items) = items {
                        for (item, path) in items.into_iter().zip(paths_with_positions) {
                            let Some(item) = item else {
                                continue;
                            };
                            let Some(row) = path.row else {
                                continue;
                            };
                            if let Some(active_editor) = item.downcast::<Editor>() {
                                window
                                    .update(cx, |_, window, cx| {
                                        active_editor.update(cx, |editor, cx| {
                                            let row = row.saturating_sub(1);
                                            let col = path.column.unwrap_or(0).saturating_sub(1);
                                            editor.go_to_singleton_buffer_point(
                                                Point::new(row, col),
                                                window,
                                                cx,
                                            );
                                        });
                                    })
                                    .ok();
                            }
                        }
                    }

                    this.update(cx, |_, cx| {
                        cx.emit(DismissEvent);
                    })
                    .ok();
                    Some(())
                }
            })
            .shared();
        cx.new(|_| Self {
            _path_task,
            picker,
            data,
        })
    }
}

impl gpui::Render for ProjectPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(match &self.data {
                ProjectPickerData::Ssh {
                    connection_string,
                    nickname,
                } => SshConnectionHeader {
                    connection_string: connection_string.clone(),
                    paths: Default::default(),
                    nickname: nickname.clone(),
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx),
                ProjectPickerData::Wsl { distro_name } => SshConnectionHeader {
                    connection_string: distro_name.clone(),
                    paths: Default::default(),
                    nickname: None,
                    is_wsl: true,
                    is_devcontainer: false,
                }
                .render(window, cx),
                ProjectPickerData::DevContainer { name } => SshConnectionHeader {
                    connection_string: name.clone(),
                    paths: Default::default(),
                    nickname: None,
                    is_wsl: false,
                    is_devcontainer: true,
                }
                .render(window, cx),
            })
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.picker.clone()),
            )
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct SshServerIndex(usize);
impl std::fmt::Display for SshServerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct WslServerIndex(usize);
impl std::fmt::Display for WslServerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct DevContainerIndex(usize);
impl std::fmt::Display for DevContainerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
enum ServerIndex {
    Ssh(SshServerIndex),
    Wsl(WslServerIndex),
    DevContainer(DevContainerIndex),
}
impl From<SshServerIndex> for ServerIndex {
    fn from(index: SshServerIndex) -> Self {
        Self::Ssh(index)
    }
}
impl From<WslServerIndex> for ServerIndex {
    fn from(index: WslServerIndex) -> Self {
        Self::Wsl(index)
    }
}
impl From<DevContainerIndex> for ServerIndex {
    fn from(index: DevContainerIndex) -> Self {
        Self::DevContainer(index)
    }
}

#[derive(Clone)]
enum RemoteEntry {
    Project {
        select: NavigableEntry,
        open_folder: NavigableEntry,
        projects: Vec<(NavigableEntry, RemoteProject)>,
        configure: NavigableEntry,
        connection: Connection,
        index: ServerIndex,
    },
    SshConfig {
        select: NavigableEntry,
        open_folder: NavigableEntry,
        host: SharedString,
    },
}

impl RemoteEntry {
    fn is_from_zed(&self) -> bool {
        matches!(self, Self::Project { .. })
    }

    fn select_entry(&self) -> &NavigableEntry {
        match self {
            Self::Project { select, .. } => select,
            Self::SshConfig { select, .. } => select,
        }
    }

    fn can_configure(&self) -> bool {
        matches!(
            self,
            Self::Project {
                connection: Connection::Ssh(_) | Connection::Wsl(_) | Connection::DevContainer(_),
                ..
            }
        )
    }

    fn matches_tab(&self, tab: RemoteProjectsTab) -> bool {
        match tab {
            RemoteProjectsTab::Ssh => matches!(
                self,
                RemoteEntry::Project {
                    connection: Connection::Ssh(_),
                    ..
                } | RemoteEntry::SshConfig { .. }
            ),
            RemoteProjectsTab::DevContainers => matches!(
                self,
                RemoteEntry::Project {
                    connection: Connection::DevContainer(_),
                    ..
                }
            ),
            RemoteProjectsTab::Wsl => matches!(
                self,
                RemoteEntry::Project {
                    connection: Connection::Wsl(_),
                    ..
                }
            ),
        }
    }

    fn connection(&self) -> Cow<'_, Connection> {
        match self {
            Self::Project { connection, .. } => Cow::Borrowed(connection),
            Self::SshConfig { host, .. } => Cow::Owned(
                SshConnection {
                    host: host.to_string(),
                    ..SshConnection::default()
                }
                .into(),
            ),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum RemoteEntryKey {
    Ssh {
        host: SharedString,
        username: Option<SharedString>,
        port: Option<u16>,
    },
    Wsl {
        distro: SharedString,
        user: Option<SharedString>,
    },
    DevContainer {
        container_id: SharedString,
        use_podman: bool,
        host: Option<DevContainerHost>,
    },
    SshConfig(SharedString),
}

impl RemoteEntryKey {
    fn from_entry(entry: &RemoteEntry) -> Self {
        match entry {
            RemoteEntry::Project { connection, .. } => match connection {
                Connection::Ssh(connection) => Self::Ssh {
                    host: SharedString::from(connection.host.clone()),
                    username: connection.username.clone().map(SharedString::from),
                    port: connection.port,
                },
                Connection::Wsl(connection) => Self::Wsl {
                    distro: SharedString::from(connection.distro_name.clone()),
                    user: connection.user.clone().map(SharedString::from),
                },
                Connection::DevContainer(connection) => Self::DevContainer {
                    container_id: SharedString::from(connection.container_id.clone()),
                    use_podman: connection.use_podman,
                    host: connection.host.clone(),
                },
            },
            RemoteEntry::SshConfig { host, .. } => Self::SshConfig(host.clone()),
        }
    }

    fn matches(&self, entry: &RemoteEntry) -> bool {
        match (self, entry) {
            (
                Self::Ssh {
                    host,
                    username,
                    port,
                },
                RemoteEntry::Project {
                    connection: Connection::Ssh(connection),
                    ..
                },
            ) => {
                host.as_ref() == connection.host.as_str()
                    && username.as_ref().map(|value| value.as_ref())
                        == connection.username.as_deref()
                    && *port == connection.port
            }
            (
                Self::Wsl { distro, user },
                RemoteEntry::Project {
                    connection: Connection::Wsl(connection),
                    ..
                },
            ) => {
                distro.as_ref() == connection.distro_name.as_str()
                    && user.as_ref().map(|value| value.as_ref()) == connection.user.as_deref()
            }
            (
                Self::DevContainer {
                    container_id,
                    use_podman,
                    host,
                },
                RemoteEntry::Project {
                    connection: Connection::DevContainer(connection),
                    ..
                },
            ) => {
                container_id.as_ref() == connection.container_id.as_str()
                    && *use_podman == connection.use_podman
                    && host == &connection.host
            }
            (
                Self::SshConfig(host),
                RemoteEntry::SshConfig {
                    host: entry_host, ..
                },
            ) => host == entry_host,
            _ => false,
        }
    }
}

impl RemoteProjectsTab {
    fn empty_message(self) -> &'static str {
        match self {
            RemoteProjectsTab::Ssh => "No SSH servers registered yet.",
            RemoteProjectsTab::DevContainers => "No dev containers registered yet.",
            RemoteProjectsTab::Wsl => "No WSL distros registered yet.",
        }
    }
}

#[derive(Clone)]
struct DefaultState {
    scroll_handle: ScrollHandle,
    add_new_server: NavigableEntry,
    add_new_devcontainer: NavigableEntry,
    refresh_devcontainer: NavigableEntry,
    add_new_wsl: NavigableEntry,
    servers: Vec<RemoteEntry>,
}

impl DefaultState {
    fn new(ssh_config_servers: &BTreeSet<SharedString>, cx: &mut App) -> Self {
        let handle = ScrollHandle::new();
        let add_new_server = NavigableEntry::new(&handle, cx);
        let add_new_devcontainer = NavigableEntry::new(&handle, cx);
        let refresh_devcontainer = NavigableEntry::new(&handle, cx);
        let add_new_wsl = NavigableEntry::new(&handle, cx);

        let ssh_settings = RemoteSettings::get_global(cx);
        let read_ssh_config = ssh_settings.read_ssh_config;

        let ssh_servers = ssh_settings
            .ssh_connections()
            .enumerate()
            .map(|(index, connection)| {
                let select = NavigableEntry::new(&handle, cx);
                let open_folder = NavigableEntry::new(&handle, cx);
                let configure = NavigableEntry::new(&handle, cx);
                let projects = connection
                    .projects
                    .iter()
                    .map(|project| (NavigableEntry::new(&handle, cx), project.clone()))
                    .collect();
                RemoteEntry::Project {
                    select,
                    open_folder,
                    configure,
                    projects,
                    index: ServerIndex::Ssh(SshServerIndex(index)),
                    connection: connection.into(),
                }
            });

        let wsl_servers = ssh_settings
            .wsl_connections()
            .enumerate()
            .map(|(index, connection)| {
                let select = NavigableEntry::new(&handle, cx);
                let open_folder = NavigableEntry::new(&handle, cx);
                let configure = NavigableEntry::new(&handle, cx);
                let projects = connection
                    .projects
                    .iter()
                    .map(|project| (NavigableEntry::new(&handle, cx), project.clone()))
                    .collect();
                RemoteEntry::Project {
                    select,
                    open_folder,
                    configure,
                    projects,
                    index: ServerIndex::Wsl(WslServerIndex(index)),
                    connection: connection.into(),
                }
            });

        let dev_container_servers =
            ssh_settings
                .dev_container_connections()
                .enumerate()
                .map(|(index, connection)| {
                    let select = NavigableEntry::new(&handle, cx);
                    let open_folder = NavigableEntry::new(&handle, cx);
                    let configure = NavigableEntry::new(&handle, cx);
                    let projects = connection
                        .projects
                        .iter()
                        .map(|project| (NavigableEntry::new(&handle, cx), project.clone()))
                        .collect();
                    RemoteEntry::Project {
                        select,
                        open_folder,
                        configure,
                        projects,
                        index: ServerIndex::DevContainer(DevContainerIndex(index)),
                        connection: connection.into(),
                    }
                });

        let mut servers = ssh_servers
            .chain(wsl_servers)
            .chain(dev_container_servers)
            .collect::<Vec<RemoteEntry>>();

        if read_ssh_config {
            let mut extra_servers_from_config = ssh_config_servers.clone();
            for server in &servers {
                if let RemoteEntry::Project {
                    connection: Connection::Ssh(ssh_options),
                    ..
                } = server
                {
                    extra_servers_from_config.remove(&SharedString::new(ssh_options.host.clone()));
                }
            }
            servers.extend(extra_servers_from_config.into_iter().map(|host| {
                RemoteEntry::SshConfig {
                    select: NavigableEntry::new(&handle, cx),
                    open_folder: NavigableEntry::new(&handle, cx),
                    host,
                }
            }));
        }

        Self {
            scroll_handle: handle,
            add_new_server,
            add_new_devcontainer,
            refresh_devcontainer,
            add_new_wsl,
            servers,
        }
    }
}

#[derive(Clone)]
enum ViewServerOptionsState {
    Ssh {
        connection: SshConnectionOptions,
        server_index: SshServerIndex,
        entries: [NavigableEntry; 4],
    },
    Wsl {
        connection: WslConnectionOptions,
        server_index: WslServerIndex,
        entries: [NavigableEntry; 2],
    },
    DevContainer {
        connection: DevContainerConnection,
        server_index: DevContainerIndex,
        entries: [NavigableEntry; 8],
    },
}

impl ViewServerOptionsState {
    fn entries(&self) -> &[NavigableEntry] {
        match self {
            Self::Ssh { entries, .. } => entries,
            Self::Wsl { entries, .. } => entries,
            Self::DevContainer { entries, .. } => entries,
        }
    }
}

enum Mode {
    Default(DefaultState),
    ViewServerOptions(ViewServerOptionsState),
    EditNickname(EditNicknameState),
    EditDevContainerName(EditDevContainerNameState),
    ProjectPicker(Entity<ProjectPicker>),
    CreateRemoteServer(CreateRemoteServer),
    CreateRemoteDevContainer(CreateRemoteDevContainer),
    #[cfg(target_os = "windows")]
    AddWslDistro(AddWslDistro),
}

impl Mode {
    fn default_mode(ssh_config_servers: &BTreeSet<SharedString>, cx: &mut App) -> Self {
        Self::Default(DefaultState::new(ssh_config_servers, cx))
    }
}

impl RemoteServerProjects {
    #[cfg(target_os = "windows")]
    pub fn wsl(
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::AddWslDistro(AddWslDistro::new(window, cx)),
            create_new_window,
            fs,
            window,
            workspace,
            cx,
        )
    }

    pub fn new(
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::default_mode(&BTreeSet::new(), cx),
            create_new_window,
            fs,
            window,
            workspace,
            cx,
        )
    }

    /// Creates a new RemoteServerProjects modal that opens directly in dev container creation mode.
    /// Used when suggesting dev container connection from toast notification.
    pub fn new_dev_container(
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::CreateRemoteDevContainer(
                CreateRemoteDevContainer::new(window, cx).with_progress(
                    DevContainerCreationProgress::Creating,
                    window,
                    cx,
                ),
            ),
            false,
            fs,
            window,
            workspace,
            cx,
        )
    }

    pub fn popover(
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        create_new_window: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let server = Self::new_inner(
                Mode::default_mode(&BTreeSet::new(), cx),
                create_new_window,
                fs,
                window,
                workspace,
                cx,
            );
            server.focus_handle(cx).focus(window, cx);
            server
        })
    }

    fn new_inner(
        mode: Mode,
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut read_ssh_config = RemoteSettings::get_global(cx).read_ssh_config;
        let ssh_config_updates = if read_ssh_config {
            spawn_ssh_config_watch(fs.clone(), cx)
        } else {
            Task::ready(())
        };

        let mut base_style = window.text_style();
        base_style.refine(&gpui::TextStyleRefinement {
            color: Some(cx.theme().colors().editor_foreground),
            ..Default::default()
        });

        let _subscription =
            cx.observe_global_in::<SettingsStore>(window, move |recent_projects, _, cx| {
                let new_read_ssh_config = RemoteSettings::get_global(cx).read_ssh_config;
                if read_ssh_config != new_read_ssh_config {
                    read_ssh_config = new_read_ssh_config;
                    if read_ssh_config {
                        recent_projects.ssh_config_updates = spawn_ssh_config_watch(fs.clone(), cx);
                    } else {
                        recent_projects.ssh_config_servers.clear();
                        recent_projects.ssh_config_entries.clear();
                        recent_projects.ssh_config_updates = Task::ready(());
                    }
                }
            });

        let selected_tab = match &mode {
            Mode::CreateRemoteDevContainer(_) => RemoteProjectsTab::DevContainers,
            #[cfg(target_os = "windows")]
            Mode::AddWslDistro(_) => RemoteProjectsTab::Wsl,
            _ => RemoteProjectsTab::Ssh,
        };

        let ssh_search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter SSH servers...", window, cx);
            editor
        });
        let wsl_search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter WSL distros...", window, cx);
            editor
        });
        let dev_container_search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter dev containers...", window, cx);
            editor
        });

        let _ssh_search_subscription = cx.subscribe_in(
            &ssh_search_editor,
            window,
            |this, _, event: &EditorEvent, _window, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.ssh_page = 0;
                    cx.notify();
                }
            },
        );
        let _wsl_search_subscription = cx.subscribe_in(
            &wsl_search_editor,
            window,
            |this, _, event: &EditorEvent, _window, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.wsl_page = 0;
                    cx.notify();
                }
            },
        );
        let _dev_container_search_subscription = cx.subscribe_in(
            &dev_container_search_editor,
            window,
            |this, _, event: &EditorEvent, _window, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.dev_container_page = 0;
                    cx.notify();
                }
            },
        );

        Self {
            mode,
            focus_handle,
            workspace,
            retained_connections: Vec::new(),
            ssh_config_updates,
            ssh_config_servers: BTreeSet::new(),
            ssh_config_entries: HashMap::new(),
            create_new_window,
            selected_tab,
            selected_entry: None,
            ssh_search_editor,
            wsl_search_editor,
            dev_container_search_editor,
            ssh_page: 0,
            wsl_page: 0,
            dev_container_page: 0,
            _ssh_search_subscription,
            _wsl_search_subscription,
            _dev_container_search_subscription,
            saved_connections_search_subscription: None,
            ssh_config_search_subscription: None,
            _subscription,
        }
    }

    fn new_create_remote_server_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CreateRemoteServer {
        let state = CreateRemoteServer::new(window, cx);
        let editor = state.saved_connections_search_editor.clone();
        self.saved_connections_search_subscription = Some(cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &EditorEvent, _window, cx| {
                if let EditorEvent::BufferEdited = event {
                    if let Mode::CreateRemoteServer(state) = &mut this.mode {
                        state.saved_connections_page = 0;
                    }
                    cx.notify();
                }
            },
        ));
        let editor = state.ssh_config_search_editor.clone();
        self.ssh_config_search_subscription = Some(cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &EditorEvent, _window, cx| {
                if let EditorEvent::BufferEdited = event {
                    if let Mode::CreateRemoteServer(state) = &mut this.mode {
                        state.ssh_config_hosts_page = 0;
                    }
                    cx.notify();
                }
            },
        ));
        state
    }

    fn project_picker(
        create_new_window: bool,
        index: ServerIndex,
        connection_options: remote::RemoteConnectionOptions,
        project: Entity<Project>,
        home_dir: RemotePathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let fs = project.read(cx).fs().clone();
        let mut this = Self::new(create_new_window, fs, window, workspace.clone(), cx);
        this.mode = Mode::ProjectPicker(ProjectPicker::new(
            create_new_window,
            index,
            connection_options,
            project,
            home_dir,
            workspace,
            window,
            cx,
        ));
        cx.notify();

        this
    }

    fn create_ssh_server(
        &mut self,
        state: &CreateRemoteServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = get_text(&state.address_editor, cx);
        if input.is_empty() {
            return;
        }

        let mut connection_options = match SshConnectionOptions::parse_command_line(&input) {
            Ok(c) => c,
            Err(e) => {
                self.mode = Mode::CreateRemoteServer(state.rebuild_with(
                    Some(format!("could not parse: {:?}", e).into()),
                    None,
                    None,
                    Some(CreateRemoteServerFormErrors::default()),
                ));
                return;
            }
        };
        if connection_options.nickname.is_none() {
            connection_options.nickname = Self::default_ssh_nickname(
                &connection_options.host.to_string(),
                connection_options.username.as_deref(),
                connection_options.port,
            );
        }
        let cleared_state = state.rebuild_with(
            None,
            None,
            None,
            Some(CreateRemoteServerFormErrors::default()),
        );
        self.start_ssh_connection(&cleared_state, connection_options, window, cx);
    }

    fn use_ssh_config_host(
        &mut self,
        host: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        state.form.host_editor.update(cx, |editor, cx| {
            editor.set_text(host.to_string(), window, cx);
        });
        let mut new_state = state.rebuild_with(
            None,
            None,
            None,
            Some(CreateRemoteServerFormErrors::default()),
        );
        new_state.input_mode = CreateRemoteServerInputMode::Form;
        self.mode = Mode::CreateRemoteServer(new_state);
        state.form.host_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn use_ssh_config_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        let host = get_text(&state.form.host_editor, cx);
        if host.is_empty() {
            return;
        }
        let Some(entry) = self.ssh_config_entries.get(host.as_str()) else {
            return;
        };

        let mut new_errors = state.form_errors.clone();
        new_errors.host = None;
        if let Some(user) = entry.user.as_deref() {
            let current = get_text(&state.form.username_editor, cx);
            if current.is_empty() {
                state.form.username_editor.update(cx, |editor, cx| {
                    editor.set_text(user, window, cx);
                });
            }
        }

        if let Some(port) = entry.port {
            let current = get_text(&state.form.port_editor, cx);
            if current.is_empty() {
                state.form.port_editor.update(cx, |editor, cx| {
                    editor.set_text(port.to_string(), window, cx);
                });
                new_errors.port = None;
            }
        }

        let new_state = state.rebuild_with(None, None, None, Some(new_errors));
        self.mode = Mode::CreateRemoteServer(new_state);
        cx.notify();
    }

    fn create_ssh_server_from_form(
        &mut self,
        state: &CreateRemoteServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection_options = match self.build_ssh_connection_from_form(state, cx) {
            Ok(options) => options,
            Err(errors) => {
                let mut new_state = state.rebuild_with(None, None, None, Some(errors));
                if !new_state.form_errors.port_forwards.is_empty() {
                    new_state.show_advanced = true;
                }
                self.mode = Mode::CreateRemoteServer(new_state);
                return;
            }
        };

        let cleared_state = state.rebuild_with(
            None,
            None,
            None,
            Some(CreateRemoteServerFormErrors::default()),
        );
        self.start_ssh_connection(&cleared_state, connection_options, window, cx);
    }

    fn build_ssh_connection_from_form(
        &self,
        state: &CreateRemoteServer,
        cx: &mut Context<Self>,
    ) -> Result<SshConnectionOptions, CreateRemoteServerFormErrors> {
        let mut errors = CreateRemoteServerFormErrors::default();
        let mut host = get_text(&state.form.host_editor, cx);
        if host.is_empty() {
            errors.host = Some("Host is required".into());
        }

        let mut username =
            Some(get_text(&state.form.username_editor, cx)).filter(|t| !t.is_empty());
        if username.is_none() {
            if let Some((user, host_part)) = split_user_host(&host) {
                username = Some(user);
                host = host_part;
            }
        }

        let port = {
            let port_text = get_text(&state.form.port_editor, cx);
            if port_text.is_empty() {
                None
            } else {
                match port_text.parse::<u16>() {
                    Ok(port) if port > 0 => Some(port),
                    Ok(_) => {
                        errors.port = Some("Port must be between 1 and 65535".into());
                        None
                    }
                    Err(_) => {
                        errors.port = Some("Port must be a number".into());
                        None
                    }
                }
            }
        };

        let identity_file = get_text(&state.form.identity_file_editor, cx);
        let jump_host = get_text(&state.form.jump_host_editor, cx);
        let port_forwards = get_text(&state.form.port_forwards_editor, cx);
        let password = state.form.password_editor.read(cx).text(cx).to_string();
        let nickname_text = get_text(&state.form.nickname_editor, cx);

        let mut args = Vec::new();
        if !identity_file.is_empty() {
            args.push("-i".to_string());
            args.push(identity_file);
        }
        if !jump_host.is_empty() {
            args.push("-J".to_string());
            args.push(jump_host);
        }

        let (port_forwards, port_forward_errors) = parse_port_forwards(&port_forwards);
        if !port_forward_errors.is_empty() {
            errors.port_forwards = port_forward_errors;
        }

        let default_nickname = Self::default_ssh_nickname(&host, username.as_deref(), port);
        let mut connection_options = SshConnectionOptions {
            host: host.into(),
            username,
            port,
            ..SshConnectionOptions::default()
        };
        connection_options.nickname = if nickname_text.trim().is_empty() {
            default_nickname
        } else {
            Some(nickname_text)
        };
        if !password.is_empty() {
            connection_options.password = Some(password);
        }
        if !args.is_empty() {
            connection_options.args = Some(args);
        }
        if !port_forwards.is_empty() {
            connection_options.port_forwards = Some(port_forwards);
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(connection_options)
    }

    fn show_save_toast(&self, message: impl Into<SharedString>, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let message: SharedString = message.into();
        let message_text = message.to_string();
        workspace.update(cx, |workspace, cx| {
            struct SshSaveToast;
            workspace.show_toast(
                Toast::new(
                    NotificationId::composite::<SshSaveToast>(message_text.clone()),
                    message_text.clone(),
                )
                .autohide(),
                cx,
            );
        });
    }

    fn save_ssh_connection_options(
        &mut self,
        connection_options: SshConnectionOptions,
        cx: &mut Context<Self>,
    ) {
        let host = connection_options.host.to_string();
        let username = connection_options.username.clone();
        let port = connection_options.port;
        let nickname = connection_options.nickname.clone();
        let args = connection_options.args.unwrap_or_default();
        let port_forwards = connection_options.port_forwards.clone();
        let connection_timeout = connection_options.connection_timeout;

        self.update_settings_file(cx, move |setting, _| {
            let connections = setting.ssh_connections.get_or_insert(Default::default());
            let mut entry = connections
                .iter()
                .find(|connection| {
                    connection.host == host
                        && connection.username == username
                        && connection.port == port
                })
                .cloned()
                .unwrap_or_else(|| SshConnection {
                    host: host.clone(),
                    username: username.clone(),
                    port,
                    projects: BTreeSet::new(),
                    nickname: None,
                    args: Vec::new(),
                    upload_binary_over_ssh: None,
                    port_forwards: None,
                    connection_timeout: None,
                });
            entry.args = args.clone();
            entry.port_forwards = port_forwards.clone();
            entry.connection_timeout = connection_timeout;
            entry.nickname = nickname.clone();
            connections.retain(|connection| {
                connection.host != host
                    || connection.username != username
                    || connection.port != port
            });
            connections.insert(0, entry);
        });
    }

    fn remove_saved_connection(&mut self, connection: &SshConnection, cx: &mut Context<Self>) {
        let connection = connection.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.ssh_connections.as_mut() {
                connections.retain(|existing| existing != &connection);
            }
        });
    }

    fn confirm_remove_saved_connection(
        &mut self,
        connection: SshConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection_string = SshConnectionOptions::from(connection.clone()).connection_string();
        let prompt_message = format!("Delete saved connection `{}`?", connection_string);
        let confirmation = window.prompt(
            PromptLevel::Warning,
            &prompt_message,
            None,
            &["Delete", "Cancel"],
            cx,
        );
        let remote_servers = cx.entity();
        cx.spawn(async move |_, cx| {
            if confirmation.await.ok() == Some(0) {
                remote_servers.update(cx, |this, cx| {
                    this.remove_saved_connection(&connection, cx);
                    this.show_save_toast("Saved connection deleted.", cx);
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn confirm_remove_all_saved_connections(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmation = window.prompt(
            PromptLevel::Warning,
            "Delete all saved connections?",
            Some("This will clear the saved connection history."),
            &["Delete all", "Cancel"],
            cx,
        );
        let remote_servers = cx.entity();
        cx.spawn(async move |_, cx| {
            if confirmation.await.ok() == Some(0) {
                remote_servers.update(cx, |this, cx| {
                    this.update_settings_file(cx, |setting, _| {
                        if let Some(connections) = setting.ssh_connections.as_mut() {
                            connections.clear();
                        }
                    });
                    this.show_save_toast("All saved connections deleted.", cx);
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn set_editor_text(
        editor: &Entity<Editor>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
        });
    }

    fn apply_saved_connection(
        &mut self,
        connection: SshConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = if let Mode::CreateRemoteServer(state) = &mut self.mode {
            state
        } else {
            return;
        };

        state.input_mode = CreateRemoteServerInputMode::Form;
        if let Some(nickname) = &connection.nickname {
            Self::set_editor_text(&state.form.nickname_editor, nickname, window, cx);
        } else {
            Self::set_editor_text(&state.form.nickname_editor, "", window, cx);
        }
        Self::set_editor_text(&state.form.host_editor, &connection.host, window, cx);
        if let Some(username) = &connection.username {
            Self::set_editor_text(&state.form.username_editor, username, window, cx);
        } else {
            Self::set_editor_text(&state.form.username_editor, "", window, cx);
        }
        if let Some(port) = connection.port {
            Self::set_editor_text(&state.form.port_editor, &port.to_string(), window, cx);
        } else {
            Self::set_editor_text(&state.form.port_editor, "", window, cx);
        }

        let mut identity_file = None;
        let mut jump_host = None;
        let mut args_iter = connection.args.iter();
        while let Some(arg) = args_iter.next() {
            if arg == "-i" {
                identity_file = args_iter.next().cloned();
            } else if let Some(rest) = arg.strip_prefix("-i") {
                if !rest.is_empty() {
                    identity_file = Some(rest.to_string());
                }
            } else if arg == "-J" {
                jump_host = args_iter.next().cloned();
            } else if let Some(rest) = arg.strip_prefix("-J") {
                if !rest.is_empty() {
                    jump_host = Some(rest.to_string());
                }
            }
        }

        let has_identity_file = identity_file.is_some();
        let has_jump_host = jump_host.is_some();
        if let Some(identity_file) = identity_file {
            Self::set_editor_text(&state.form.identity_file_editor, &identity_file, window, cx);
        } else {
            Self::set_editor_text(&state.form.identity_file_editor, "", window, cx);
        }
        if let Some(jump_host) = jump_host {
            Self::set_editor_text(&state.form.jump_host_editor, &jump_host, window, cx);
        } else {
            Self::set_editor_text(&state.form.jump_host_editor, "", window, cx);
        }

        let has_port_forwards = connection
            .port_forwards
            .as_ref()
            .is_some_and(|pf| !pf.is_empty());
        if let Some(port_forwards) = connection.port_forwards.as_ref() {
            let formatted = port_forwards
                .iter()
                .map(|pf| {
                    let local_host = pf.local_host.as_deref().unwrap_or("localhost");
                    let remote_host = pf.remote_host.as_deref().unwrap_or("localhost");
                    format!(
                        "{local_host}:{}:{remote_host}:{}",
                        pf.local_port, pf.remote_port
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Self::set_editor_text(&state.form.port_forwards_editor, &formatted, window, cx);
        } else {
            Self::set_editor_text(&state.form.port_forwards_editor, "", window, cx);
        }

        state.show_advanced = has_identity_file || has_jump_host || has_port_forwards;
        state.form_errors = CreateRemoteServerFormErrors::default();
        state.address_error = None;
        cx.notify();
    }

    fn set_command_text(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Mode::CreateRemoteServer(state) = &mut self.mode {
            state.input_mode = CreateRemoteServerInputMode::Command;
            state.address_editor.update(cx, |editor, cx| {
                editor.set_text(command, window, cx);
            });
            state.address_error = None;
            cx.notify();
        }
    }

    fn default_ssh_nickname(
        host: &str,
        username: Option<&str>,
        port: Option<u16>,
    ) -> Option<String> {
        if host.is_empty() {
            return None;
        }
        let mut nickname = match username.filter(|value| !value.is_empty()) {
            Some(user) => format!("{user}@{host}"),
            None => host.to_string(),
        };
        if let Some(port) = port {
            if port != 22 {
                nickname.push(':');
                nickname.push_str(&port.to_string());
            }
        }
        Some(nickname)
    }

    fn ssh_command_string(connection: &SshConnection) -> String {
        let options: SshConnectionOptions = connection.clone().into();
        let mut parts = vec!["ssh".to_string()];
        parts.extend(options.additional_args());
        parts.push(options.ssh_destination());
        parts.join(" ")
    }

    fn start_ssh_connection(
        &mut self,
        state: &CreateRemoteServer,
        connection_options: SshConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ssh_prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.connection_string(),
                connection_options.nickname.clone(),
                false,
                false,
                window,
                cx,
            )
        });

        let build_remote_server = std::env::var("ZED_BUILD_REMOTE_SERVER")
            .map(|value| !matches!(value.as_str(), "false" | "no" | "off" | "0"))
            .unwrap_or(true);
        let timeout = if cfg!(debug_assertions) || build_remote_server {
            START_PROXY_TIMEOUT_WITH_BUILD
        } else {
            START_PROXY_TIMEOUT
        };

        let connection = connect(
            ConnectionIdentifier::setup(),
            RemoteConnectionOptions::Ssh(connection_options.clone()),
            ssh_prompt.clone(),
            window,
            cx,
        )
        .prompt_err("Failed to connect", window, cx, |_, _, _| None)
        .with_timeout(timeout, cx.background_executor());

        let restore_state = state.rebuild_with(None, None, None, None);
        let creating = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Ok(Some(Some(client))) => this
                    .update_in(cx, |this, window, cx| {
                        let client: Entity<RemoteClient> = client;
                        info!("ssh server created");
                        telemetry::event!("SSH Server Created");
                        let connection_options = connection_options.clone();
                        this.save_ssh_connection_options(connection_options.clone(), cx);
                        let server_index =
                            this.add_ssh_server_with_index(connection_options.clone(), cx);
                        this.retained_connections.push(client.clone());
                        if !this.show_project_picker_with_session(
                            server_index.into(),
                            RemoteConnectionOptions::Ssh(connection_options),
                            client,
                            window,
                            cx,
                        ) {
                            this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                            this.focus_handle(cx).focus(window, cx);
                            cx.notify();
                        }
                    })
                    .log_err(),
                Ok(_) => this
                    .update(cx, |this, cx| {
                        restore_state.set_read_only(false, cx);
                        this.mode = Mode::CreateRemoteServer(restore_state);
                        cx.notify()
                    })
                    .log_err(),
                Err(_) => this
                    .update_in(cx, |this, window, cx| {
                        restore_state.set_read_only(false, cx);
                        this.mode = Mode::CreateRemoteServer(restore_state);
                        cx.notify();
                        drop(window.prompt(
                            PromptLevel::Critical,
                            "Failed to connect",
                            Some("Timed out while starting proxy. Please try again."),
                            &["Ok"],
                            cx,
                        ));
                    })
                    .log_err(),
            };
            None
        });

        state.set_read_only(true, cx);
        self.mode = Mode::CreateRemoteServer(state.rebuild_with(
            None,
            Some(ssh_prompt),
            Some(creating),
            None,
        ));
    }

    fn ssh_keychain_url_from_form(
        &self,
        state: &CreateRemoteServer,
        cx: &mut Context<Self>,
    ) -> Option<(String, String)> {
        let mut host = get_text(&state.form.host_editor, cx);
        if host.is_empty() {
            return None;
        }
        let mut username =
            Some(get_text(&state.form.username_editor, cx)).filter(|t| !t.is_empty());
        if username.is_none() {
            if let Some((user, host_part)) = split_user_host(&host) {
                username = Some(user);
                host = host_part;
            }
        }
        let port_text = get_text(&state.form.port_editor, cx);
        if port_text.is_empty() {
            return None;
        }
        let port = port_text.parse::<u16>().ok().filter(|port| *port > 0);
        let username = username?;

        let connection = SshConnectionOptions {
            host: host.into(),
            username: Some(username.clone()),
            port,
            ..SshConnectionOptions::default()
        };
        let url = format!("ssh://{}", connection.connection_string());
        Some((url, username))
    }

    fn maybe_refresh_password_state(
        &mut self,
        state: &CreateRemoteServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((url, _username)) = self.ssh_keychain_url_from_form(state, cx) else {
            if state.password_keychain_url.is_some()
                || state.password_keychain_status != PasswordKeychainStatus::Unknown
            {
                let mut new_state = state.rebuild_with(None, None, None, None);
                new_state.password_keychain_url = None;
                new_state.password_keychain_status = PasswordKeychainStatus::Unknown;
                new_state.password_autoload_url = None;
                self.mode = Mode::CreateRemoteServer(new_state);
                cx.notify();
            }
            return;
        };

        if state.password_keychain_url.as_deref() == Some(url.as_str())
            && state.password_keychain_status != PasswordKeychainStatus::Unknown
        {
            return;
        }

        let auto_fill = state.form.password_editor.read(cx).text(cx).is_empty()
            && state.password_autoload_url.as_deref() != Some(url.as_str());
        let mut new_state = state.rebuild_with(None, None, None, None);
        new_state.password_keychain_url = Some(url.clone());
        new_state.password_keychain_status = PasswordKeychainStatus::Loading;
        if auto_fill {
            new_state.password_autoload_url = Some(url.clone());
        }
        self.mode = Mode::CreateRemoteServer(new_state);
        cx.notify();

        let read_task = cx.read_credentials(&url);
        cx.spawn_in(window, async move |this, cx| {
            let result = read_task.await;
            this.update_in(cx, |this, window, cx| {
                let Mode::CreateRemoteServer(state) = &this.mode else {
                    return;
                };
                if state.password_keychain_url.as_deref() != Some(url.as_str()) {
                    return;
                }

                let mut new_state = state.rebuild_with(None, None, None, None);
                match result {
                    Ok(Some((_user, bytes))) => match String::from_utf8(bytes) {
                        Ok(password) => {
                            new_state.password_keychain_status = PasswordKeychainStatus::Saved;
                            if auto_fill && state.form.password_editor.read(cx).text(cx).is_empty()
                            {
                                state.form.password_editor.update(cx, |editor, cx| {
                                    editor.set_text(password, window, cx);
                                });
                            }
                        }
                        Err(_) => {
                            new_state.password_keychain_status = PasswordKeychainStatus::Error;
                        }
                    },
                    Ok(None) => {
                        new_state.password_keychain_status = PasswordKeychainStatus::NotSaved;
                    }
                    Err(error) => {
                        log::error!("Failed to read ssh password: {error:#}");
                        new_state.password_keychain_status = PasswordKeychainStatus::Error;
                    }
                }
                this.mode = Mode::CreateRemoteServer(new_state);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn toggle_password_visibility(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        let new_visible = !state.password_visible;
        state.form.password_editor.update(cx, |editor, cx| {
            editor.set_masked(!new_visible, cx);
        });
        let mut new_state = state.rebuild_with(None, None, None, None);
        new_state.password_visible = new_visible;
        self.mode = Mode::CreateRemoteServer(new_state);
        cx.notify();
    }

    fn show_password_toast(&self, message: impl Into<SharedString>, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let message = message.into();
        let message_text = message.to_string();
        workspace.update(cx, |workspace, cx| {
            struct SshPasswordToast;
            workspace.show_toast(
                Toast::new(
                    NotificationId::composite::<SshPasswordToast>(message.clone()),
                    message_text.clone(),
                )
                .autohide(),
                cx,
            );
        });
    }

    fn use_saved_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        let Some((url, _username)) = self.ssh_keychain_url_from_form(&state, cx) else {
            self.show_password_toast("Enter host, username, and port first.", cx);
            return;
        };
        if !state.form.password_editor.read(cx).text(cx).is_empty() {
            self.show_password_toast("Password field is not empty.", cx);
            return;
        }

        let read_task = cx.read_credentials(&url);
        cx.spawn_in(window, async move |this, cx| {
            let result = read_task.await;
            this.update_in(cx, |this, window, cx| {
                let Mode::CreateRemoteServer(state) = &this.mode else {
                    return;
                };
                let mut updated = state.rebuild_with(None, None, None, None);
                updated.password_keychain_url = Some(url.clone());
                match result {
                    Ok(Some((_user, bytes))) => match String::from_utf8(bytes) {
                        Ok(password) => {
                            state.form.password_editor.update(cx, |editor, cx| {
                                editor.set_text(password, window, cx);
                            });
                            updated.password_keychain_status = PasswordKeychainStatus::Saved;
                            updated.password_autoload_url = Some(url.clone());
                            this.show_password_toast("Password loaded from keychain.", cx);
                        }
                        Err(_) => {
                            updated.password_keychain_status = PasswordKeychainStatus::Error;
                            this.show_password_toast("Saved password is not valid UTF-8.", cx);
                        }
                    },
                    Ok(None) => {
                        updated.password_keychain_status = PasswordKeychainStatus::NotSaved;
                        this.show_password_toast("No saved password for this host.", cx);
                    }
                    Err(error) => {
                        log::error!("Failed to read ssh password: {error:#}");
                        updated.password_keychain_status = PasswordKeychainStatus::Error;
                        this.show_password_toast("Failed to read saved password.", cx);
                    }
                }
                this.mode = Mode::CreateRemoteServer(updated);
            })
            .log_err();
        })
        .detach();
    }

    fn save_password_to_keychain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        let Some((url, username)) = self.ssh_keychain_url_from_form(&state, cx) else {
            self.show_password_toast("Enter host, username, and port first.", cx);
            return;
        };
        let password = state.form.password_editor.read(cx).text(cx).to_string();
        if password.is_empty() {
            self.show_password_toast("Enter a password to save.", cx);
            return;
        }

        let write_task = cx.write_credentials(&url, &username, password.as_bytes());
        cx.spawn_in(window, async move |this, cx| {
            let result = write_task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    if let Mode::CreateRemoteServer(state) = &this.mode {
                        let mut updated = state.rebuild_with(None, None, None, None);
                        updated.password_keychain_status = PasswordKeychainStatus::Saved;
                        updated.password_keychain_url = Some(url.clone());
                        this.mode = Mode::CreateRemoteServer(updated);
                    }
                    this.show_password_toast("Password saved to keychain.", cx);
                }
                Err(error) => {
                    log::error!("Failed to save ssh password: {error:#}");
                    if let Mode::CreateRemoteServer(state) = &this.mode {
                        let mut updated = state.rebuild_with(None, None, None, None);
                        updated.password_keychain_status = PasswordKeychainStatus::Error;
                        updated.password_keychain_url = Some(url.clone());
                        this.mode = Mode::CreateRemoteServer(updated);
                    }
                    this.show_password_toast("Failed to save password.", cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn delete_saved_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        let Some((url, _username)) = self.ssh_keychain_url_from_form(&state, cx) else {
            self.show_password_toast("Enter host, username, and port first.", cx);
            return;
        };

        let delete_task = cx.delete_credentials(&url);
        cx.spawn_in(window, async move |this, cx| {
            let result = delete_task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    if let Mode::CreateRemoteServer(state) = &this.mode {
                        let mut updated = state.rebuild_with(None, None, None, None);
                        updated.password_keychain_status = PasswordKeychainStatus::NotSaved;
                        updated.password_keychain_url = Some(url.clone());
                        this.mode = Mode::CreateRemoteServer(updated);
                    }
                    this.show_password_toast("Saved password deleted.", cx);
                }
                Err(error) => {
                    log::error!("Failed to delete ssh password: {error:#}");
                    if let Mode::CreateRemoteServer(state) = &this.mode {
                        let mut updated = state.rebuild_with(None, None, None, None);
                        updated.password_keychain_status = PasswordKeychainStatus::Error;
                        updated.password_keychain_url = Some(url.clone());
                        this.mode = Mode::CreateRemoteServer(updated);
                    }
                    this.show_password_toast("Failed to delete saved password.", cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn pick_identity_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = match &self.mode {
            Mode::CreateRemoteServer(state) => state.snapshot(),
            _ => return,
        };
        if state.ssh_prompt.is_some() || state._creating.is_some() {
            return;
        }

        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select SSH identity file".into()),
        });

        cx.spawn_in(window, async move |this, cx| {
            let paths = match prompt.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    log::error!("Failed to prompt for identity file: {error}");
                    return;
                }
                Err(_) => return,
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let path_text = path.to_string_lossy().to_string();
            this.update_in(cx, move |this, window, cx| {
                let Mode::CreateRemoteServer(state) = &this.mode else {
                    return;
                };
                state.form.identity_file_editor.update(cx, |editor, cx| {
                    editor.set_text(path_text, window, cx);
                });
            })
            .log_err();
        })
        .detach();
    }

    #[cfg(target_os = "windows")]
    fn connect_wsl_distro(
        &mut self,
        picker: Entity<Picker<crate::wsl_picker::WslPickerDelegate>>,
        distro: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection_options = WslConnectionOptions {
            distro_name: distro,
            user: None,
        };

        let prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.distro_name.clone(),
                None,
                true,
                false,
                window,
                cx,
            )
        });
        let connection = connect(
            ConnectionIdentifier::setup(),
            connection_options.clone().into(),
            prompt.clone(),
            window,
            cx,
        )
        .prompt_err("Failed to connect", window, cx, |_, _, _| None);

        let wsl_picker = picker.clone();
        let creating = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Some(Some(client)) => this.update_in(cx, |this, window, cx| {
                    telemetry::event!("WSL Distro Added");
                    this.retained_connections.push(client);
                    let Some(fs) = this
                        .workspace
                        .read_with(cx, |workspace, cx| {
                            workspace.project().read(cx).fs().clone()
                        })
                        .log_err()
                    else {
                        return;
                    };

                    crate::add_wsl_distro(fs, &connection_options, cx);
                    this.mode = Mode::default_mode(&BTreeSet::new(), cx);
                    this.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }),
                _ => this.update(cx, |this, cx| {
                    this.mode = Mode::AddWslDistro(AddWslDistro {
                        picker: wsl_picker,
                        connection_prompt: None,
                        _creating: None,
                    });
                    cx.notify();
                }),
            }
            .log_err();
        });

        self.mode = Mode::AddWslDistro(AddWslDistro {
            picker,
            connection_prompt: Some(prompt),
            _creating: Some(creating),
        });
    }

    fn view_server_options(
        &mut self,
        (server_index, connection): (ServerIndex, RemoteConnectionOptions),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::ViewServerOptions(match (server_index, connection) {
            (ServerIndex::Ssh(server_index), RemoteConnectionOptions::Ssh(connection)) => {
                ViewServerOptionsState::Ssh {
                    connection,
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            (ServerIndex::Wsl(server_index), RemoteConnectionOptions::Wsl(connection)) => {
                ViewServerOptionsState::Wsl {
                    connection,
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            (
                ServerIndex::DevContainer(server_index),
                RemoteConnectionOptions::Docker(connection),
            ) => {
                let host = match connection.host {
                    DockerHost::Local => None,
                    DockerHost::Wsl(options) => Some(DevContainerHost::Wsl {
                        distro_name: options.distro_name,
                        user: options.user,
                    }),
                    DockerHost::Ssh(options) => Some(DevContainerHost::Ssh {
                        host: options.host.to_string(),
                        username: options.username,
                        port: options.port,
                        args: options.args.unwrap_or_default(),
                    }),
                };
                ViewServerOptionsState::DevContainer {
                    connection: DevContainerConnection {
                        name: connection.name,
                        container_id: connection.container_id,
                        use_podman: connection.use_podman,
                        projects: Default::default(),
                        host_projects: Default::default(),
                        host,
                    },
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            _ => {
                log::error!("server index and connection options mismatch");
                self.mode = Mode::default_mode(&BTreeSet::default(), cx);
                return;
            }
        });
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn view_in_progress_dev_container(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::CreateRemoteDevContainer(
            CreateRemoteDevContainer::new(window, cx).with_progress(
                DevContainerCreationProgress::Creating,
                window,
                cx,
            ),
        );
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn handle_devcontainer_progress_event(
        &mut self,
        event: DevContainerProgressEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Mode::CreateRemoteDevContainer(state) = &mut self.mode else {
            return;
        };

        if state.build_state.is_none() {
            state.build_state = Some(DevContainerBuildState::new(window, cx));
        }

        let Some(build_state) = state.build_state.as_mut() else {
            return;
        };

        match event {
            DevContainerProgressEvent::StepStarted(step) => {
                build_state.start_step(step);
                if state.progress == DevContainerCreationProgress::Initial {
                    state.progress = DevContainerCreationProgress::Creating;
                }
            }
            DevContainerProgressEvent::StepCompleted(step) => {
                build_state.complete_step(step);
                if build_state.status == DevContainerBuildStatus::Success {
                    state.progress = DevContainerCreationProgress::Success;
                }
            }
            DevContainerProgressEvent::StepFailed(step, message) => {
                build_state.fail_step(step);
                build_state.append_log_line(
                    DevContainerLogLine {
                        stream: DevContainerLogStream::Stderr,
                        line: message.clone(),
                    },
                    window,
                    cx,
                );
                state.progress = DevContainerCreationProgress::Error(message);
            }
            DevContainerProgressEvent::LogLine(line) => {
                build_state.append_log_line(line, window, cx);
            }
        }

        cx.notify();
    }

    fn retained_connection_for(
        &self,
        connection_options: &RemoteConnectionOptions,
        cx: &App,
    ) -> Option<Entity<RemoteClient>> {
        self.retained_connections.iter().find_map(|client| {
            let client_state = client.read(cx);
            if client_state.connection_options() == *connection_options
                && !client_state.is_disconnected()
            {
                Some(client.clone())
            } else {
                None
            }
        })
    }

    fn show_project_picker_with_session(
        &mut self,
        index: ServerIndex,
        connection_options: RemoteConnectionOptions,
        session: Entity<RemoteClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let app_state = workspace.read_with(cx, |workspace, _| workspace.app_state().clone());

        let create_new_window = self.create_new_window;
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (path_style, project) = cx
                .update(|_, cx| {
                    (
                        session.read(cx).path_style(),
                        project::Project::remote(
                            session.clone(),
                            app_state.client.clone(),
                            app_state.node_runtime.clone(),
                            app_state.user_store.clone(),
                            app_state.languages.clone(),
                            app_state.fs.clone(),
                            true,
                            cx,
                        ),
                    )
                })
                .log_err()?;

            let home_dir = project
                .read_with(cx, |project, cx| project.resolve_abs_path("~", cx))
                .await
                .and_then(|path| path.into_abs_path())
                .map(|path| RemotePathBuf::new(path, path_style))
                .unwrap_or_else(|| match path_style {
                    PathStyle::Posix => RemotePathBuf::from_str("/", PathStyle::Posix),
                    PathStyle::Windows => RemotePathBuf::from_str("C:\\", PathStyle::Windows),
                });

            this.update_in(cx, |this, window, cx| {
                this.mode = Mode::ProjectPicker(ProjectPicker::new(
                    create_new_window,
                    index,
                    connection_options,
                    project,
                    home_dir,
                    workspace,
                    window,
                    cx,
                ));
                this.focus_handle(cx).focus(window, cx);
                cx.notify();
            })
            .log_err();
            None::<()>
        })
        .detach();

        true
    }

    fn create_remote_project(
        &mut self,
        index: ServerIndex,
        connection_options: RemoteConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.retained_connection_for(&connection_options, cx) {
            if self.show_project_picker_with_session(
                index,
                connection_options.clone(),
                session,
                window,
                cx,
            ) {
                return;
            }
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let create_new_window = self.create_new_window;
        workspace.update(cx, |_, cx| {
            cx.defer_in(window, move |workspace, window, cx| {
                let app_state = workspace.app_state().clone();
                workspace.toggle_modal(window, cx, |window, cx| {
                    RemoteConnectionModal::new(&connection_options, Vec::new(), window, cx)
                });
                let prompt = workspace
                    .active_modal::<RemoteConnectionModal>(cx)
                    .unwrap()
                    .read(cx)
                    .prompt
                    .clone();

                let connect = connect(
                    ConnectionIdentifier::setup(),
                    connection_options.clone(),
                    prompt,
                    window,
                    cx,
                )
                .prompt_err("Failed to connect", window, cx, |_, _, _| None);

                cx.spawn_in(window, async move |workspace, cx| {
                    let session = connect.await;

                    workspace.update(cx, |workspace, cx| {
                        if let Some(prompt) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                            prompt.update(cx, |prompt, cx| prompt.finished(cx))
                        }
                    })?;

                    let Some(Some(session)) = session else {
                        return workspace.update_in(cx, |workspace, window, cx| {
                            let weak = cx.entity().downgrade();
                            let fs = workspace.project().read(cx).fs().clone();
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::new(create_new_window, fs, window, weak, cx)
                            });
                        });
                    };

                    let (path_style, project) = cx.update(|_, cx| {
                        (
                            session.read(cx).path_style(),
                            project::Project::remote(
                                session,
                                app_state.client.clone(),
                                app_state.node_runtime.clone(),
                                app_state.user_store.clone(),
                                app_state.languages.clone(),
                                app_state.fs.clone(),
                                true,
                                cx,
                            ),
                        )
                    })?;

                    let home_dir = project
                        .read_with(cx, |project, cx| project.resolve_abs_path("~", cx))
                        .await
                        .and_then(|path| path.into_abs_path())
                        .map(|path| RemotePathBuf::new(path, path_style))
                        .unwrap_or_else(|| match path_style {
                            PathStyle::Posix => RemotePathBuf::from_str("/", PathStyle::Posix),
                            PathStyle::Windows => {
                                RemotePathBuf::from_str("C:\\", PathStyle::Windows)
                            }
                        });

                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let weak = cx.entity().downgrade();
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::project_picker(
                                    create_new_window,
                                    index,
                                    connection_options,
                                    project,
                                    home_dir,
                                    window,
                                    cx,
                                    weak,
                                )
                            });
                        })
                        .ok();
                    Ok(())
                })
                .detach();
            })
        })
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let create_state = match &self.mode {
            Mode::CreateRemoteServer(state) => Some(state.snapshot()),
            _ => None,
        };
        if let Some(state) = create_state {
            if let Some(prompt) = state.ssh_prompt.as_ref() {
                prompt.update(cx, |prompt, cx| {
                    prompt.confirm(window, cx);
                });
                return;
            }

            match state.input_mode {
                CreateRemoteServerInputMode::Command => {
                    self.create_ssh_server(&state, window, cx);
                }
                CreateRemoteServerInputMode::Form => {
                    self.create_ssh_server_from_form(&state, window, cx);
                }
            }
            return;
        }

        match &self.mode {
            Mode::Default(_) | Mode::ViewServerOptions(_) => {}
            Mode::ProjectPicker(_) => {}
            Mode::CreateRemoteServer(_) => {}
            Mode::CreateRemoteDevContainer(_) => {}
            Mode::EditNickname(state) => {
                let text = Some(state.editor.read(cx).text(cx)).filter(|text| !text.is_empty());
                let index = state.index;
                self.update_settings_file(cx, move |setting, _| {
                    if let Some(connections) = setting.ssh_connections.as_mut()
                        && let Some(connection) = connections.get_mut(index.0)
                    {
                        connection.nickname = text;
                    }
                });
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle.focus(window, cx);
            }
            Mode::EditDevContainerName(state) => {
                let new_name = state.editor.read(cx).text(cx);
                let new_name = new_name.trim().to_string();
                let index = state.index;
                if !new_name.is_empty() {
                    self.update_settings_file(cx, move |setting, _| {
                        if let Some(connections) = setting.dev_container_connections.as_mut()
                            && let Some(connection) = connections.get_mut(index.0)
                        {
                            connection.name = new_name.clone();
                        }
                    });
                }
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle.focus(window, cx);
            }
            #[cfg(target_os = "windows")]
            Mode::AddWslDistro(state) => {
                let delegate = &state.picker.read(cx).delegate;
                let distro = delegate.selected_distro().unwrap();
                self.connect_wsl_distro(state.picker.clone(), distro, window, cx);
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Default(_) => cx.emit(DismissEvent),
            Mode::CreateRemoteServer(state) if state.ssh_prompt.is_some() => {
                let new_state = state.rebuild_with(None, None, None, None);
                new_state.set_read_only(false, cx);
                self.mode = Mode::CreateRemoteServer(new_state);
                cx.notify();
            }
            _ => {
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        }
    }

    fn server_row_labels(
        &self,
        remote_server: &RemoteEntry,
    ) -> (IconName, SharedString, Option<SharedString>) {
        match remote_server {
            RemoteEntry::Project { connection, .. } => match connection {
                Connection::Ssh(connection) => {
                    if let Some(nickname) = connection.nickname.clone() {
                        let aux_label = SharedString::from(format!("({})", connection.host));
                        (IconName::Server, nickname.into(), Some(aux_label))
                    } else {
                        (IconName::Server, connection.host.clone().into(), None)
                    }
                }
                Connection::Wsl(connection) => {
                    (IconName::Linux, connection.distro_name.clone().into(), None)
                }
                Connection::DevContainer(connection) => {
                    (IconName::Box, connection.name.clone().into(), None)
                }
            },
            RemoteEntry::SshConfig { host, .. } => (
                IconName::Server,
                host.clone(),
                Some(SharedString::from("SSH config")),
            ),
        }
    }

    fn header_info(
        &self,
        remote_server: &RemoteEntry,
    ) -> (SharedString, Option<SharedString>, bool, bool) {
        match remote_server {
            RemoteEntry::Project { connection, .. } => match connection {
                Connection::Ssh(connection) => (
                    connection.host.clone().into(),
                    connection.nickname.clone().map(Into::into),
                    false,
                    false,
                ),
                Connection::Wsl(connection) => {
                    (connection.distro_name.clone().into(), None, true, false)
                }
                Connection::DevContainer(connection) => {
                    (connection.name.clone().into(), None, false, true)
                }
            },
            RemoteEntry::SshConfig { host, .. } => (host.clone(), None, false, false),
        }
    }

    fn search_editor(&self, tab: RemoteProjectsTab) -> &Entity<Editor> {
        match tab {
            RemoteProjectsTab::Ssh => &self.ssh_search_editor,
            RemoteProjectsTab::DevContainers => &self.dev_container_search_editor,
            RemoteProjectsTab::Wsl => &self.wsl_search_editor,
        }
    }

    fn normalized_search_query(&self, tab: RemoteProjectsTab, cx: &App) -> String {
        self.search_editor(tab)
            .read(cx)
            .text(cx)
            .trim()
            .to_lowercase()
    }

    fn clear_search(
        &mut self,
        tab: RemoteProjectsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tab {
            RemoteProjectsTab::Ssh => {
                self.ssh_search_editor.update(cx, |editor, cx| {
                    editor.set_text("", window, cx);
                });
                self.ssh_page = 0;
            }
            RemoteProjectsTab::DevContainers => {
                self.dev_container_search_editor.update(cx, |editor, cx| {
                    editor.set_text("", window, cx);
                });
                self.dev_container_page = 0;
            }
            RemoteProjectsTab::Wsl => {
                self.wsl_search_editor.update(cx, |editor, cx| {
                    editor.set_text("", window, cx);
                });
                self.wsl_page = 0;
            }
        }
        self.selected_entry = None;
        cx.notify();
    }

    fn matches_search_query(&self, remote_server: &RemoteEntry, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }

        let mut haystack = String::new();
        match remote_server {
            RemoteEntry::Project {
                connection,
                projects,
                ..
            } => {
                match connection {
                    Connection::Ssh(connection) => {
                        haystack.push_str(&connection.host);
                        if let Some(nickname) = &connection.nickname {
                            haystack.push(' ');
                            haystack.push_str(nickname);
                        }
                        if let Some(username) = &connection.username {
                            haystack.push(' ');
                            haystack.push_str(username);
                        }
                        if let Some(port) = connection.port {
                            haystack.push(' ');
                            haystack.push_str(&port.to_string());
                        }
                    }
                    Connection::Wsl(connection) => {
                        haystack.push_str(&connection.distro_name);
                        if let Some(user) = &connection.user {
                            haystack.push(' ');
                            haystack.push_str(user);
                        }
                    }
                    Connection::DevContainer(connection) => {
                        haystack.push_str(&connection.name);
                        haystack.push(' ');
                        haystack.push_str(&connection.container_id);
                    }
                }

                for (_, project) in projects {
                    for path in &project.paths {
                        haystack.push(' ');
                        haystack.push_str(path);
                    }
                }
            }
            RemoteEntry::SshConfig { host, .. } => {
                haystack.push_str(host.as_ref());
                if let Some(entry) = self.ssh_config_entries.get(host.as_ref()) {
                    if let Some(hostname) = entry.hostname.as_ref() {
                        haystack.push(' ');
                        haystack.push_str(hostname);
                    }
                    if let Some(user) = entry.user.as_ref() {
                        haystack.push(' ');
                        haystack.push_str(user);
                    }
                    if let Some(port) = entry.port {
                        haystack.push(' ');
                        haystack.push_str(&port.to_string());
                    }
                }
            }
        }

        haystack.to_lowercase().contains(query)
    }

    fn render_search_bar(
        &mut self,
        tab: RemoteProjectsTab,
        has_query: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (editor, clear_id) = match tab {
            RemoteProjectsTab::Ssh => (self.ssh_search_editor.clone(), "ssh-search-clear"),
            RemoteProjectsTab::DevContainers => (
                self.dev_container_search_editor.clone(),
                "dev-container-search-clear",
            ),
            RemoteProjectsTab::Wsl => (self.wsl_search_editor.clone(), "wsl-search-clear"),
        };
        let tab_for_clear = tab;

        h_flex()
            .w_full()
            .items_center()
            .gap_1()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .px_2()
            .py_1()
            .child(Icon::new(IconName::MagnifyingGlass).color(Color::Muted))
            .child(editor)
            .when(has_query, |this| {
                this.child(
                    IconButton::new(clear_id, IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .shape(IconButtonShape::Square)
                        .size(ButtonSize::Compact)
                        .tooltip(Tooltip::text("Clear search"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.clear_search(tab_for_clear, window, cx);
                        })),
                )
            })
    }

    fn render_remote_server_row(
        &mut self,
        ix: usize,
        remote_server: RemoteEntry,
        selected_key: Option<&RemoteEntryKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entry_key = RemoteEntryKey::from_entry(&remote_server);
        let is_selected = selected_key.is_some_and(|key| key.matches(&remote_server));
        let select = remote_server.select_entry().clone();
        let (icon, main_label, aux_label) = self.server_row_labels(&remote_server);
        let build_row = {
            let entry_key = entry_key.clone();
            let select = select.clone();
            let main_label = main_label.clone();
            let aux_label = aux_label.clone();
            move |window: &mut Window, cx: &mut Context<Self>| {
                let entry_key = entry_key.clone();
                let select = select.clone();
                let aux_label = aux_label.clone();

                h_flex()
                    .id(("remote-server-row-container", ix))
                    .track_focus(&select.focus_handle)
                    .anchor_scroll(select.scroll_anchor.clone())
                    .on_action(cx.listener({
                        let entry_key = entry_key.clone();
                        move |this, _: &menu::Confirm, _window, cx| {
                            this.selected_entry = Some(entry_key.clone());
                            cx.notify();
                        }
                    }))
                    .child(
                        ListItem::new(("remote-server-row", ix))
                            .toggle_state(
                                is_selected || select.focus_handle.contains_focused(window, cx),
                            )
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(icon).color(Color::Muted))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .overflow_hidden()
                                    .child(Label::new(main_label.clone()).size(LabelSize::Small))
                                    .children(aux_label.map(|label| {
                                        Label::new(label).size(LabelSize::Small).color(Color::Muted)
                                    })),
                            )
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.selected_entry = Some(entry_key.clone());
                                cx.notify();
                            })),
                    )
            }
        };
        build_row(window, cx).into_any_element()
    }

    fn render_remote_details(
        &mut self,
        ix: usize,
        remote_server: RemoteEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match &remote_server {
            RemoteEntry::Project {
                open_folder,
                projects,
                configure,
                connection,
                index,
                ..
            } => {
                let index = *index;
                let (connection_string, nickname, is_wsl, is_devcontainer) =
                    self.header_info(&remote_server);
                let can_configure = remote_server.can_configure();

                let mut list = List::new()
                    .empty_message("No projects.")
                    .children(projects.iter().enumerate().map(|(pix, p)| {
                        v_flex().gap_0p5().child(self.render_remote_project(
                            index,
                            remote_server.clone(),
                            pix,
                            p,
                            window,
                            cx,
                        ))
                    }))
                    .child(
                        h_flex()
                            .id(("new-remote-project-container", ix))
                            .track_focus(&open_folder.focus_handle)
                            .anchor_scroll(open_folder.scroll_anchor.clone())
                            .on_action(cx.listener({
                                let connection = connection.clone();
                                move |this, _: &menu::Confirm, window, cx| {
                                    this.create_remote_project(
                                        index,
                                        connection.clone().into(),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .child(
                                ListItem::new(("new-remote-project", ix))
                                    .toggle_state(
                                        open_folder.focus_handle.contains_focused(window, cx),
                                    )
                                    .inset(true)
                                    .spacing(ui::ListItemSpacing::Sparse)
                                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                                    .child(Label::new("Open Folder"))
                                    .on_click(cx.listener({
                                        let connection = connection.clone();
                                        move |this, _, window, cx| {
                                            this.create_remote_project(
                                                index,
                                                connection.clone().into(),
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            ),
                    );

                if can_configure {
                    list = list.child(
                        h_flex()
                            .id(("server-options-container", ix))
                            .track_focus(&configure.focus_handle)
                            .anchor_scroll(configure.scroll_anchor.clone())
                            .on_action(cx.listener({
                                let connection = connection.clone();
                                move |this, _: &menu::Confirm, window, cx| {
                                    this.view_server_options(
                                        (index, connection.clone().into()),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .child(
                                ListItem::new(("server-options", ix))
                                    .toggle_state(
                                        configure.focus_handle.contains_focused(window, cx),
                                    )
                                    .inset(true)
                                    .spacing(ui::ListItemSpacing::Sparse)
                                    .start_slot(Icon::new(IconName::Settings).color(Color::Muted))
                                    .child(Label::new("View Server Options"))
                                    .on_click(cx.listener({
                                        let connection = connection.clone();
                                        move |this, _, window, cx| {
                                            this.view_server_options(
                                                (index, connection.clone().into()),
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            ),
                    );
                }

                let header = SshConnectionHeader {
                    connection_string,
                    paths: Default::default(),
                    nickname,
                    is_wsl,
                    is_devcontainer,
                }
                .render(window, cx);

                v_flex().child(header).child(ListSeparator).child(list)
            }
            RemoteEntry::SshConfig {
                open_folder, host, ..
            } => {
                let connection_string = host.clone();
                let connection = remote_server.connection().into_owned();
                let list = List::new().child(
                    h_flex()
                        .id(("new-remote-project-container", ix))
                        .track_focus(&open_folder.focus_handle)
                        .anchor_scroll(open_folder.scroll_anchor.clone())
                        .on_action(cx.listener({
                            let connection = connection.clone();
                            let host = host.clone();
                            move |this, _: &menu::Confirm, window, cx| {
                                let new_ix = this.create_host_from_ssh_config(&host, cx);
                                this.create_remote_project(
                                    new_ix.into(),
                                    connection.clone().into(),
                                    window,
                                    cx,
                                );
                            }
                        }))
                        .child(
                            ListItem::new(("new-remote-project", ix))
                                .toggle_state(open_folder.focus_handle.contains_focused(window, cx))
                                .inset(true)
                                .spacing(ui::ListItemSpacing::Sparse)
                                .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                                .child(Label::new("Open Folder"))
                                .on_click(cx.listener({
                                    let host = host.clone();
                                    move |this, _, window, cx| {
                                        let new_ix = this.create_host_from_ssh_config(&host, cx);
                                        this.create_remote_project(
                                            new_ix.into(),
                                            connection.clone().into(),
                                            window,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                );
                let header = SshConnectionHeader {
                    connection_string,
                    paths: Default::default(),
                    nickname: None,
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx);

                v_flex().child(header).child(ListSeparator).child(list)
            }
        }
    }

    fn render_remote_project(
        &mut self,
        server_ix: ServerIndex,
        server: RemoteEntry,
        ix: usize,
        (navigation, project): &(NavigableEntry, RemoteProject),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let create_new_window = self.create_new_window;
        let is_from_zed = server.is_from_zed();
        let element_id_base = SharedString::from(format!(
            "remote-project-{}",
            match server_ix {
                ServerIndex::Ssh(index) => format!("ssh-{index}"),
                ServerIndex::Wsl(index) => format!("wsl-{index}"),
                ServerIndex::DevContainer(index) => format!("devcontainer-{index}"),
            }
        ));
        let container_element_id_base =
            SharedString::from(format!("remote-project-container-{element_id_base}"));

        let callback = Rc::new({
            let project = project.clone();
            move |remote_server_projects: &mut Self,
                  secondary_confirm: bool,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                let Some(app_state) = remote_server_projects
                    .workspace
                    .read_with(cx, |workspace, _| workspace.app_state().clone())
                    .log_err()
                else {
                    return;
                };
                let project = project.clone();
                let server = server.connection().into_owned();
                cx.emit(DismissEvent);

                let replace_window = match (create_new_window, secondary_confirm) {
                    (true, false) | (false, true) => None,
                    (true, true) | (false, false) => window.window_handle().downcast::<Workspace>(),
                };

                cx.spawn_in(window, async move |_, cx| {
                    let result = open_remote_project(
                        server.into(),
                        project.paths.into_iter().map(PathBuf::from).collect(),
                        app_state,
                        OpenOptions {
                            replace_window,
                            ..OpenOptions::default()
                        },
                        cx,
                    )
                    .await;
                    if let Err(e) = result {
                        log::error!("Failed to connect: {e:#}");
                        cx.prompt(
                            gpui::PromptLevel::Critical,
                            "Failed to connect",
                            Some(&e.to_string()),
                            &["Ok"],
                        )
                        .await
                        .ok();
                    }
                })
                .detach();
            }
        });

        div()
            .id((container_element_id_base, ix))
            .track_focus(&navigation.focus_handle)
            .anchor_scroll(navigation.scroll_anchor.clone())
            .on_action(cx.listener({
                let callback = callback.clone();
                move |this, _: &menu::Confirm, window, cx| {
                    callback(this, false, window, cx);
                }
            }))
            .on_action(cx.listener({
                let callback = callback.clone();
                move |this, _: &menu::SecondaryConfirm, window, cx| {
                    callback(this, true, window, cx);
                }
            }))
            .child(
                ListItem::new((element_id_base, ix))
                    .toggle_state(navigation.focus_handle.contains_focused(window, cx))
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(
                        Icon::new(IconName::Folder)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .child(Label::new(project.paths.join(", ")).truncate_start())
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                        let secondary_confirm = e.modifiers().platform;
                        callback(this, secondary_confirm, window, cx)
                    }))
                    .tooltip(Tooltip::text(project.paths.join("\n")))
                    .when(is_from_zed, |server_list_item| {
                        server_list_item.end_hover_slot::<AnyElement>(Some(
                            div()
                                .mr_2()
                                .child({
                                    let project = project.clone();
                                    // Right-margin to offset it from the Scrollbar
                                    IconButton::new("remove-remote-project", IconName::Trash)
                                        .icon_size(IconSize::Small)
                                        .shape(IconButtonShape::Square)
                                        .size(ButtonSize::Large)
                                        .tooltip(Tooltip::text("Delete Remote Project"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.delete_remote_project(server_ix, &project, cx)
                                        }))
                                })
                                .into_any_element(),
                        ))
                    }),
            )
    }

    fn open_remote_project_from_paths(
        &mut self,
        connection: Connection,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let create_new_window = self.create_new_window;
        let Some(app_state) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().clone())
            .log_err()
        else {
            return;
        };
        cx.emit(DismissEvent);

        let replace_window = if create_new_window {
            None
        } else {
            window.window_handle().downcast::<Workspace>()
        };
        let paths = paths.into_iter().map(PathBuf::from).collect::<Vec<_>>();

        cx.spawn_in(window, async move |_, cx| {
            let result = open_remote_project(
                connection.into(),
                paths,
                app_state,
                OpenOptions {
                    replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            if let Err(e) = result {
                log::error!("Failed to connect: {e:#}");
                cx.prompt(
                    gpui::PromptLevel::Critical,
                    "Failed to connect",
                    Some(&e.to_string()),
                    &["Ok"],
                )
                .await
                .ok();
            }
        })
        .detach();
    }

    fn update_settings_file(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut RemoteSettingsContent, &App) + Send + Sync + 'static,
    ) {
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .log_err()
        else {
            return;
        };
        update_settings_file(fs, cx, move |setting, cx| f(&mut setting.remote, cx));
    }

    fn save_dev_container_connection(
        &mut self,
        connection: DevContainerConnection,
        starting_dir: String,
        host_starting_dir: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.update_settings_file(cx, move |setting, _| {
            let connections = setting
                .dev_container_connections
                .get_or_insert(Default::default());
            upsert_dev_container_connection(
                connections,
                connection,
                starting_dir,
                host_starting_dir,
            );
        });
    }

    fn delete_ssh_server(&mut self, server: SshServerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.ssh_connections.as_mut() {
                connections.remove(server.0);
            }
        });
    }

    fn show_devcontainer_toast(&self, message: impl Into<SharedString>, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let message: SharedString = message.into();
        let message_text = message.to_string();
        workspace.update(cx, |workspace, cx| {
            struct DevContainerToast;
            workspace.show_toast(
                Toast::new(
                    NotificationId::composite::<DevContainerToast>(message_text.clone()),
                    message_text.clone(),
                )
                .autohide(),
                cx,
            );
        });
    }

    fn active_dev_container_client(
        &self,
        connection: &DevContainerConnection,
        cx: &App,
    ) -> Option<Entity<RemoteClient>> {
        let key = DevContainerKey::from_connection(connection);
        if let Some(workspace) = self.workspace.upgrade() {
            let project = workspace.read(cx).project().clone();
            if let Some(remote_options) = project.read(cx).remote_connection_options(cx) {
                if dev_container_key_from_remote_options(&remote_options) == Some(key) {
                    if let Some(client) = project.read(cx).remote_client() {
                        if !client.read(cx).is_disconnected() {
                            return Some(client);
                        }
                    }
                }
            }
        }

        let connection_options: RemoteConnectionOptions =
            Connection::DevContainer(connection.clone()).into();
        self.retained_connection_for(&connection_options, cx)
    }

    fn disconnect_active_dev_container(
        &self,
        connection: &DevContainerConnection,
        server_not_running: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(client) = self.active_dev_container_client(connection, cx) else {
            return false;
        };
        client.update(cx, |client, cx| {
            client
                .disconnect(server_not_running, cx)
                .detach_and_log_err(cx);
        });
        true
    }

    fn prompt_disconnect_dev_container(
        remote_servers: Entity<RemoteServerProjects>,
        name: SharedString,
        connection: DevContainerConnection,
        window: &mut Window,
        cx: &mut App,
    ) {
        let prompt_message = format!("Disconnect from `{}`?", name);
        let confirmation = window.prompt(
            PromptLevel::Warning,
            &prompt_message,
            Some("The container will keep running, and the host folder will reopen."),
            &["Disconnect", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |_, cx| {
            if confirmation.await.ok() == Some(0) {
                let has_host_paths =
                    RemoteServerProjects::host_project_paths(&connection).is_some();
                remote_servers
                    .update_in(cx, |this, window, cx| {
                        if has_host_paths || connection.host.is_none() {
                            this.return_to_host_folder(&connection, window, cx);
                        } else {
                            let disconnected =
                                this.disconnect_active_dev_container(&connection, false, cx);
                            if disconnected {
                                this.show_devcontainer_toast(
                                    format!("Disconnected from `{}`.", name),
                                    cx,
                                );
                            } else {
                                this.show_devcontainer_toast(
                                    format!("No active connection to `{}`.", name),
                                    cx,
                                );
                            }
                        }
                    })
                    .ok();
            }
            anyhow::Ok(())
        })
        .detach();
    }

    fn prompt_return_to_host_folder(
        remote_servers: Entity<RemoteServerProjects>,
        name: SharedString,
        connection: DevContainerConnection,
        window: &mut Window,
        cx: &mut App,
    ) {
        let prompt_message = format!("Return to the host folder from `{}`?", name);
        let confirmation = window.prompt(
            PromptLevel::Warning,
            &prompt_message,
            Some("This will close the dev container workspace in this window."),
            &["Return", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |_, cx| {
            if confirmation.await.ok() == Some(0) {
                remote_servers
                    .update_in(cx, |this, window, cx| {
                        this.return_to_host_folder(&connection, window, cx);
                    })
                    .ok();
            }
            anyhow::Ok(())
        })
        .detach();
    }

    fn prompt_stop_dev_container(
        remote_servers: Entity<RemoteServerProjects>,
        name: SharedString,
        connection: DevContainerConnection,
        window: &mut Window,
        cx: &mut App,
    ) {
        let prompt_message = format!("Stop container for `{}`?", name);
        let confirmation = window.prompt(
            PromptLevel::Warning,
            &prompt_message,
            Some("This will stop the container and disconnect if it is active."),
            &["Stop and Disconnect", "Cancel"],
            cx,
        );

        cx.spawn(async move |cx| {
            if confirmation.await.ok() == Some(0) {
                let result = stop_dev_container_container(&connection).await;
                remote_servers.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            let disconnected =
                                this.disconnect_active_dev_container(&connection, true, cx);
                            let message = if disconnected {
                                format!("Stopped container and disconnected `{}`.", name)
                            } else {
                                format!("Stopped container `{}`.", name)
                            };
                            this.show_devcontainer_toast(message, cx);
                        }
                        Err(message) => {
                            this.show_devcontainer_toast(
                                format!("Failed to stop `{}`: {}", name, message),
                                cx,
                            );
                        }
                    }
                    cx.notify();
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn prompt_remove_dev_container(
        remote_servers: Entity<RemoteServerProjects>,
        index: DevContainerIndex,
        name: SharedString,
        connection: DevContainerConnection,
        window: &mut Window,
        cx: &mut App,
    ) {
        let prompt_message = format!("Remove dev container `{}`?", name);

        let confirmation = window.prompt(
            PromptLevel::Warning,
            &prompt_message,
            Some(
                "This will stop and delete the container, even if it is running. If Zed is connected, it will disconnect.",
            ),
            &["Remove and delete container", "Cancel"],
            cx,
        );

        cx.spawn(async move |cx| {
            if confirmation.await.ok() == Some(0) {
                let result = remove_dev_container_container(&connection).await;
                remote_servers.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            let disconnected =
                                this.disconnect_active_dev_container(&connection, true, cx);
                            this.delete_dev_container_server(index, cx);
                            let message = if disconnected {
                                format!("Removed dev container `{}` and disconnected.", name)
                            } else {
                                format!("Removed dev container `{}`.", name)
                            };
                            this.show_devcontainer_toast(message, cx);
                            this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                        }
                        Err(message) => {
                            this.show_devcontainer_toast(
                                format!("Failed to remove `{}`: {}", name, message),
                                cx,
                            );
                        }
                    }
                    cx.notify();
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn host_project_paths(connection: &DevContainerConnection) -> Option<Vec<PathBuf>> {
        connection
            .host_projects
            .iter()
            .next()
            .map(|project| project.paths.iter().map(PathBuf::from).collect())
    }

    fn host_connection_options(
        connection: &DevContainerConnection,
    ) -> Option<RemoteConnectionOptions> {
        let host = connection.host.as_ref()?;
        match host {
            DevContainerHost::Ssh {
                host,
                username,
                port,
                args,
            } => Some(RemoteConnectionOptions::Ssh(SshConnectionOptions {
                host: host.clone().into(),
                username: username.clone(),
                port: *port,
                args: Some(args.clone()),
                ..SshConnectionOptions::default()
            })),
            DevContainerHost::Wsl { distro_name, user } => {
                Some(RemoteConnectionOptions::Wsl(WslConnectionOptions {
                    distro_name: distro_name.clone(),
                    user: user.clone(),
                }))
            }
        }
    }

    fn return_to_host_folder(
        &self,
        connection: &DevContainerConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.emit(DismissEvent);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(paths) = Self::host_project_paths(connection) else {
            if connection.host.is_none() {
                self.return_to_local_folder(window, cx);
            } else {
                self.show_devcontainer_toast("No host folder recorded for this dev container.", cx);
            }
            return;
        };

        if let Some(connection_options) = Self::host_connection_options(connection) {
            let Some(app_state) = workspace
                .read_with(cx, |workspace, _| workspace.app_state().clone())
                .log_err()
            else {
                return;
            };
            let replace_window = window.window_handle().downcast::<Workspace>();
            let remote_servers = cx.entity();
            cx.spawn_in(window, async move |_, cx| {
                let result = open_remote_project(
                    connection_options,
                    paths,
                    app_state,
                    OpenOptions {
                        replace_window,
                        ..Default::default()
                    },
                    cx,
                )
                .await;
                if let Err(err) = result {
                    log::error!("Failed to open host folder: {err:#}");
                    remote_servers.update(cx, |this, cx| {
                        this.show_devcontainer_toast(
                            format!("Failed to open host folder: {err:#}"),
                            cx,
                        );
                    });
                }
                anyhow::Ok(())
            })
            .detach();
        } else {
            let workspace_handle = workspace.clone();
            cx.spawn_in(window, async move |_, cx| {
                if let Some(task) = workspace_handle
                    .update_in(cx, |workspace, window, cx| {
                        workspace.open_workspace_for_paths(true, paths, window, cx)
                    })
                    .log_err()
                {
                    task.await.log_err();
                }
                anyhow::Ok(())
            })
            .detach();
        }
    }

    fn return_to_local_folder(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: None,
        });

        let workspace_handle = workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(result) = prompt.await else {
                return Ok(());
            };
            let Some(paths) = result.log_err().flatten() else {
                return Ok(());
            };

            if let Some(task) = workspace_handle
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_workspace_for_paths(true, paths, window, cx)
                })
                .log_err()
            {
                task.await.log_err();
            }
            anyhow::Ok(())
        })
        .detach();
    }

    fn delete_dev_container_server(&mut self, server: DevContainerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.dev_container_connections.as_mut() {
                connections.remove(server.0);
            }
        });
    }

    fn delete_remote_project(
        &mut self,
        server: ServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        match server {
            ServerIndex::Ssh(server) => {
                self.delete_ssh_project(server, project, cx);
            }
            ServerIndex::Wsl(server) => {
                self.delete_wsl_project(server, project, cx);
            }
            ServerIndex::DevContainer(server) => {
                self.delete_dev_container_project(server, project, cx);
            }
        }
    }

    fn delete_ssh_project(
        &mut self,
        server: SshServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .ssh_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.remove(&project);
            }
        });
    }

    fn delete_wsl_project(
        &mut self,
        server: WslServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .wsl_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.remove(&project);
            }
        });
    }

    fn delete_dev_container_project(
        &mut self,
        server: DevContainerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .dev_container_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.remove(&project);
            }
        });
    }

    fn delete_wsl_distro(&mut self, server: WslServerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.wsl_connections.as_mut() {
                connections.remove(server.0);
            }
        });
    }

    fn prune_dev_container_connections(
        &mut self,
        remove_keys: Vec<DevContainerKey>,
        cx: &mut Context<Self>,
    ) {
        if remove_keys.is_empty() {
            return;
        }
        let remove_keys: HashSet<DevContainerKey> = remove_keys.into_iter().collect();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.dev_container_connections.as_mut() {
                connections.retain(|connection| {
                    !remove_keys.contains(&DevContainerKey::from_connection(connection))
                });
            }
        });
    }

    fn refresh_dev_container_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_dev_container_connections_inner(window, cx, true);
    }

    fn refresh_dev_container_connections_silent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_dev_container_connections_inner(window, cx, false);
    }

    fn refresh_dev_container_connections_inner(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify: bool,
    ) {
        let connections: Vec<DevContainerConnection> = RemoteSettings::get_global(cx)
            .dev_container_connections()
            .collect();
        if connections.is_empty() {
            if notify {
                self.show_devcontainer_toast("No dev containers to refresh.", cx);
            }
            return;
        }

        let workspace = self.workspace.clone();
        let remote_servers = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let mut missing = Vec::new();
            let mut unknown = 0usize;
            let executor = cx.background_executor().clone();
            let mut probes = futures::stream::iter(connections.into_iter().map(|connection| {
                let executor = executor.clone();
                async move {
                    let result = probe_dev_container(&connection)
                        .with_timeout(DEV_CONTAINER_PROBE_TIMEOUT, &executor)
                        .await
                        .unwrap_or(DevContainerProbe::Unknown);
                    (connection, result)
                }
            }))
            .buffer_unordered(DEV_CONTAINER_PROBE_CONCURRENCY);

            while let Some((connection, result)) = probes.next().await {
                match result {
                    DevContainerProbe::Exists => {}
                    DevContainerProbe::Missing => {
                        missing.push(DevContainerKey::from_connection(&connection));
                    }
                    DevContainerProbe::Unknown => {
                        unknown += 1;
                    }
                }
            }

            let missing_count = missing.len();
            if missing_count > 0 {
                let remove_keys = missing.clone();
                remote_servers.update(cx, |this, cx| {
                    this.prune_dev_container_connections(remove_keys, cx);
                });
            }

            if notify {
                let message = if unknown > 0 && missing_count == 0 {
                    format!("Could not verify {unknown} dev container(s).")
                } else if missing_count > 0 {
                    if unknown > 0 {
                        format!(
                            "Removed {} stale dev container(s). {unknown} could not be verified.",
                            missing_count
                        )
                    } else {
                        format!("Removed {} stale dev container(s).", missing_count)
                    }
                } else {
                    "Dev containers are up to date.".to_string()
                };
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        struct DevContainerRefreshToast;
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::composite::<DevContainerRefreshToast>(
                                    message.clone(),
                                ),
                                message.clone(),
                            )
                            .autohide(),
                            cx,
                        );
                    });
                }
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn add_ssh_server(
        &mut self,
        connection_options: remote::SshConnectionOptions,
        cx: &mut Context<Self>,
    ) {
        self.update_settings_file(cx, move |setting, _| {
            setting
                .ssh_connections
                .get_or_insert(Default::default())
                .push(SshConnection {
                    host: connection_options.host.to_string(),
                    username: connection_options.username,
                    port: connection_options.port,
                    projects: BTreeSet::new(),
                    nickname: None,
                    args: connection_options.args.unwrap_or_default(),
                    upload_binary_over_ssh: None,
                    port_forwards: connection_options.port_forwards,
                    connection_timeout: connection_options.connection_timeout,
                })
        });
    }

    fn edit_in_dev_container_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            cx.emit(DismissEvent);
            cx.notify();
            return;
        };

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();

            let worktree = project
                .read(cx)
                .visible_worktrees(cx)
                .find_map(|tree| tree.read(cx).root_entry()?.is_dir().then_some(tree));

            if let Some(worktree) = worktree {
                let tree_id = worktree.read(cx).id();
                let devcontainer_path = RelPath::unix(".devcontainer/devcontainer.json").unwrap();
                cx.spawn_in(window, async move |workspace, cx| {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(
                                (tree_id, devcontainer_path),
                                None,
                                true,
                                window,
                                cx,
                            )
                        })?
                        .await
                })
                .detach();
            } else {
                return;
            }
        });
        cx.emit(DismissEvent);
        cx.notify();
    }

    fn open_dev_container(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(app_state) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().clone())
            .log_err()
        else {
            return;
        };
        let host_starting_dir = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .project()
                    .read(cx)
                    .active_project_directory(cx)
                    .map(|dir| dir.display().to_string())
            })
            .ok()
            .flatten();

        let replace_window = window.window_handle().downcast::<Workspace>();

        cx.spawn_in(window, async move |entity, cx| {
            let (progress_tx, mut progress_rx) = mpsc::unbounded::<DevContainerProgressEvent>();
            let progress_entity = entity.clone();
            cx.spawn(async move |cx| {
                while let Some(event) = progress_rx.next().await {
                    progress_entity
                        .update_in(cx, |this, window, cx| {
                            this.handle_devcontainer_progress_event(event, window, cx);
                        })
                        .ok();
                }
                anyhow::Ok(())
            })
            .detach();

            let (dev_connection, starting_dir) = match start_dev_container_with_progress(
                cx,
                app_state.node_runtime.clone(),
                Some(progress_tx),
            )
            .await
            {
                Ok((c, s)) => (c, s),
                Err(e) => {
                    log::error!("Failed to start dev container: {:?}", e);
                    entity
                        .update_in(cx, |remote_server_projects, window, cx| {
                            let message = e.to_string();
                            match &mut remote_server_projects.mode {
                                Mode::CreateRemoteDevContainer(state) => {
                                    if state.build_state.is_none() {
                                        state.build_state =
                                            Some(DevContainerBuildState::new(window, cx));
                                    }
                                    if let Some(build_state) = state.build_state.as_mut() {
                                        build_state.append_log_line(
                                            DevContainerLogLine {
                                                stream: DevContainerLogStream::Stderr,
                                                line: message.clone(),
                                            },
                                            window,
                                            cx,
                                        );
                                    }
                                    state.progress =
                                        DevContainerCreationProgress::Error(message.clone());
                                    cx.notify();
                                }
                                _ => {
                                    remote_server_projects.mode = Mode::CreateRemoteDevContainer(
                                        CreateRemoteDevContainer::new(window, cx).with_progress(
                                            DevContainerCreationProgress::Error(message.clone()),
                                            window,
                                            cx,
                                        ),
                                    );
                                    cx.notify();
                                }
                            }
                        })
                        .log_err();
                    return;
                }
            };
            let connection = Connection::DevContainer(dev_connection.clone());
            let host_starting_dir =
                Self::normalize_host_starting_dir(&dev_connection, host_starting_dir).await;

            entity
                .update(cx, |remote_server_projects, cx| {
                    remote_server_projects.save_dev_container_connection(
                        dev_connection,
                        starting_dir.clone(),
                        host_starting_dir.clone(),
                        cx,
                    );
                })
                .log_err();
            let result = open_remote_project(
                connection.into(),
                vec![starting_dir].into_iter().map(PathBuf::from).collect(),
                app_state,
                OpenOptions {
                    replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            match result {
                Ok(_) => {
                    entity
                        .update_in(cx, |remote_server_projects, _, cx| {
                            if let Mode::CreateRemoteDevContainer(state) =
                                &mut remote_server_projects.mode
                            {
                                state.progress = DevContainerCreationProgress::Success;
                                cx.notify();
                            }
                        })
                        .log_err();
                }
                Err(e) => {
                    log::error!("Failed to connect: {e:#}");
                    entity
                        .update_in(cx, |remote_server_projects, window, cx| {
                            if let Mode::CreateRemoteDevContainer(state) =
                                &mut remote_server_projects.mode
                            {
                                if state.build_state.is_none() {
                                    state.build_state =
                                        Some(DevContainerBuildState::new(window, cx));
                                }
                                if let Some(build_state) = state.build_state.as_mut() {
                                    build_state.fail_step(DevContainerBuildStep::ReadConfiguration);
                                    build_state.append_log_line(
                                        DevContainerLogLine {
                                            stream: DevContainerLogStream::Stderr,
                                            line: e.to_string(),
                                        },
                                        window,
                                        cx,
                                    );
                                }
                                state.progress = DevContainerCreationProgress::Error(e.to_string());
                                cx.notify();
                            }
                        })
                        .log_err();
                    cx.prompt(
                        gpui::PromptLevel::Critical,
                        "Failed to connect",
                        Some(&e.to_string()),
                        &["Ok"],
                    )
                    .await
                    .ok();
                }
            }
        })
        .detach();
    }

    async fn normalize_host_starting_dir(
        connection: &DevContainerConnection,
        host_starting_dir: Option<String>,
    ) -> Option<String> {
        let Some(path) = host_starting_dir else {
            return None;
        };
        let Some(DevContainerHost::Wsl { distro_name, user }) = connection.host.as_ref() else {
            return Some(path);
        };

        #[cfg(target_os = "windows")]
        {
            if path.starts_with('/') {
                return Some(path);
            }
            if let Some(wsl_path) = wsl_unc_path_to_posix(&path, distro_name) {
                return Some(wsl_path);
            }
            let options = WslConnectionOptions {
                distro_name: distro_name.clone(),
                user: user.clone(),
            };
            match options.abs_windows_path_to_wsl_path(Path::new(&path)).await {
                Ok(wsl_path) => Some(wsl_path),
                Err(err) => {
                    log::warn!("Failed to convert host path to WSL path: {err:#}");
                    Some(path)
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            Some(path)
        }
    }

    #[cfg(target_os = "windows")]
    fn wsl_unc_path_to_posix(path: &str, distro_name: &str) -> Option<String> {
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);
        let prefixes = [
            format!("\\\\wsl$\\\\{distro_name}"),
            format!("\\\\wsl.localhost\\\\{distro_name}"),
            format!("//wsl$/{distro_name}"),
            format!("//wsl.localhost/{distro_name}"),
        ];
        for prefix in prefixes {
            if path.starts_with(&prefix) {
                let mut rest = &path[prefix.len()..];
                rest = rest.strip_prefix('\\').unwrap_or(rest);
                rest = rest.strip_prefix('/').unwrap_or(rest);
                let mut converted = rest.replace('\\', "/");
                if !converted.starts_with('/') {
                    converted.insert(0, '/');
                }
                return Some(converted);
            }
        }
        None
    }

    fn render_create_dev_container(
        &self,
        state: &CreateRemoteDevContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if state.progress != DevContainerCreationProgress::Initial {
            self.focus_handle(cx).focus(window, cx);
            let Some(build_state) = &state.build_state else {
                return div()
                    .track_focus(&self.focus_handle(cx))
                    .size_full()
                    .child(
                        v_flex().py_1().gap_1().child(
                            Callout::new()
                                .severity(Severity::Warning)
                                .title("Preparing dev container build")
                                .description("Waiting for progress data..."),
                        ),
                    )
                    .into_any_element();
            };

            let theme = cx.theme();
            let (status_text, status_color, status_icon, status_animate) = match build_state.status
            {
                DevContainerBuildStatus::Running => {
                    ("Running", Color::Accent, IconName::ArrowCircle, true)
                }
                DevContainerBuildStatus::Success => {
                    ("Success", Color::Success, IconName::Check, false)
                }
                DevContainerBuildStatus::Failed => {
                    ("Failed", Color::Error, IconName::XCircle, false)
                }
            };
            let status_icon = if status_animate {
                Icon::new(status_icon)
                    .color(status_color)
                    .with_rotate_animation(2)
                    .into_any_element()
            } else {
                Icon::new(status_icon)
                    .color(status_color)
                    .into_any_element()
            };

            let status_row = h_flex()
                .gap_0p5()
                .items_center()
                .child(
                    Label::new("Status")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    h_flex().gap_0p5().items_center().child(status_icon).child(
                        Label::new(status_text)
                            .size(LabelSize::Small)
                            .color(status_color),
                    ),
                );

            let steps_view = v_flex()
                .gap_0p5()
                .children(build_state.steps.iter().enumerate().map(|(index, step)| {
                    let (icon_name, icon_color, icon_animate, step_status_text, step_status_color) =
                        match step.status {
                            DevContainerStepStatus::Pending => (
                                IconName::Circle,
                                Color::Muted,
                                false,
                                "Pending",
                                Color::Muted,
                            ),
                            DevContainerStepStatus::Running => (
                                IconName::ArrowCircle,
                                Color::Accent,
                                true,
                                "Running",
                                Color::Accent,
                            ),
                            DevContainerStepStatus::Completed => (
                                IconName::Check,
                                Color::Success,
                                false,
                                "Done",
                                Color::Success,
                            ),
                            DevContainerStepStatus::Failed => (
                                IconName::XCircle,
                                Color::Error,
                                false,
                                "Failed",
                                Color::Error,
                            ),
                        };
                    let icon = if icon_animate {
                        Icon::new(icon_name)
                            .color(icon_color)
                            .with_rotate_animation(2)
                            .into_any_element()
                    } else {
                        Icon::new(icon_name).color(icon_color).into_any_element()
                    };

                    let mut label = Label::new(step.label.clone()).size(LabelSize::Small);
                    if index == build_state.selected_step {
                        label = label.color(Color::Accent);
                    }

                    h_flex()
                        .gap_1()
                        .items_center()
                        .justify_between()
                        .child(h_flex().gap_1().items_center().child(icon).child(label))
                        .child(
                            Label::new(step_status_text)
                                .size(LabelSize::XSmall)
                                .color(step_status_color),
                        )
                        .into_any_element()
                }));

            let error_callout =
                if let DevContainerCreationProgress::Error(message) = &state.progress {
                    let hints = devcontainer_error_hints(message);
                    let mut description_children: Vec<AnyElement> = Vec::new();
                    description_children.push(
                        Label::new(message)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .into_any_element(),
                    );
                    if !hints.is_empty() {
                        description_children.push(
                            Label::new("Suggested fixes:")
                                .size(LabelSize::Small)
                                .into_any_element(),
                        );
                        for hint in hints {
                            description_children.push(
                                Label::new(format!("• {}", hint))
                                    .size(LabelSize::Small)
                                    .into_any_element(),
                            );
                        }
                    }
                    Some(
                        Callout::new()
                            .severity(Severity::Error)
                            .title("Build failed")
                            .description_slot(v_flex().gap_0p5().children(description_children))
                            .into_any_element(),
                    )
                } else {
                    None
                };

            let log_header = h_flex().items_center().justify_between().child(
                h_flex()
                    .gap_0p5()
                    .items_center()
                    .child(Label::new("Build Logs").size(LabelSize::Small))
                    .child(
                        Label::new("Live")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );

            let log_container = div()
                .border_1()
                .border_color(theme.colors().border_variant)
                .rounded_sm()
                .bg(theme.colors().editor_background)
                .p_1()
                .min_h(rems(12.))
                .max_h(rems(20.))
                .child(build_state.log_editor.clone());

            let finish_enabled = build_state.status != DevContainerBuildStatus::Running;
            let finish_style = match build_state.status {
                DevContainerBuildStatus::Success => ButtonStyle::Tinted(TintColor::Success),
                DevContainerBuildStatus::Failed => ButtonStyle::Tinted(TintColor::Error),
                DevContainerBuildStatus::Running => ButtonStyle::Tinted(TintColor::Accent),
            };

            let actions = h_flex()
                .justify_between()
                .items_center()
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .id("devcontainer-progress-back")
                                .track_focus(&build_state.back_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    if let Mode::CreateRemoteDevContainer(state) = &mut this.mode {
                                        if let Some(build_state) = state.build_state.as_mut() {
                                            build_state.select_previous_step();
                                            build_state.log_editor.update(cx, |editor, cx| {
                                                editor.move_to_beginning(
                                                    &editor::actions::MoveToBeginning,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }
                                        cx.notify();
                                    }
                                    cx.focus_self(window);
                                }))
                                .child(
                                    Button::new("devcontainer-progress-back-button", "Back")
                                        .icon(IconName::ArrowLeft)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            if let Mode::CreateRemoteDevContainer(state) =
                                                &mut this.mode
                                            {
                                                if let Some(build_state) =
                                                    state.build_state.as_mut()
                                                {
                                                    build_state.select_previous_step();
                                                    build_state.log_editor.update(
                                                        cx,
                                                        |editor, cx| {
                                                            editor.move_to_beginning(
                                                                &editor::actions::MoveToBeginning,
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    );
                                                }
                                                cx.notify();
                                            }
                                            cx.focus_self(window);
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("devcontainer-progress-close")
                                .track_focus(&build_state.close_entry.focus_handle)
                                .on_action(cx.listener(|_, _: &menu::Confirm, _, cx| {
                                    cx.emit(DismissEvent);
                                }))
                                .child(
                                    Button::new("devcontainer-progress-close-button", "Close")
                                        .icon(IconName::Close)
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(DismissEvent);
                                        })),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .id("devcontainer-progress-copy-logs")
                                .track_focus(&build_state.copy_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                                    if let Mode::CreateRemoteDevContainer(state) = &this.mode {
                                        if let Some(build_state) = &state.build_state {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                build_state.log_contents.clone(),
                                            ));
                                        }
                                    }
                                }))
                                .child(
                                    CopyButton::new(build_state.log_contents.clone())
                                        .tooltip_label("Copy logs"),
                                ),
                        )
                        .child(
                            div()
                                .id("devcontainer-progress-finish")
                                .track_focus(&build_state.finish_entry.focus_handle)
                                .on_action(cx.listener(move |_, _: &menu::Confirm, _, cx| {
                                    if finish_enabled {
                                        cx.emit(DismissEvent);
                                    }
                                }))
                                .child(
                                    Button::new("devcontainer-progress-finish-button", "Finish")
                                        .style(finish_style)
                                        .disabled(!finish_enabled)
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            if finish_enabled {
                                                cx.emit(DismissEvent);
                                            }
                                        })),
                                ),
                        ),
                );

            let mut view = Navigable::new(
                div()
                    .track_focus(&self.focus_handle(cx))
                    .size_full()
                    .child(
                        v_flex()
                            .pb_1()
                            .gap_1()
                            .child(ModalHeader::new().child(
                                Headline::new("Dev Container Setup").size(HeadlineSize::XSmall),
                            ))
                            .child(ListSeparator)
                            .child(
                                v_flex()
                                    .gap_1()
                                    .px_1()
                                    .child(status_row)
                                    .child(steps_view)
                                    .when_some(error_callout, |this, callout| this.child(callout))
                                    .child(log_header)
                                    .child(log_container)
                                    .child(actions),
                            ),
                    )
                    .into_any_element(),
            );

            view = view.entry(build_state.back_entry.clone());
            view = view.entry(build_state.close_entry.clone());
            view = view.entry(build_state.copy_entry.clone());
            view = view.entry(build_state.finish_entry.clone());

            return view.render(window, cx).into_any_element();
        }

        let mut view = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(
                    v_flex()
                        .pb_1()
                        .child(
                            ModalHeader::new()
                                .child(Headline::new("Dev Containers").size(HeadlineSize::XSmall)),
                        )
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("confirm-create-from-devcontainer-json")
                                .track_focus(&state.entries[0].focus_handle)
                                .on_action(cx.listener({
                                    move |this, _: &menu::Confirm, window, cx| {
                                        this.open_dev_container(window, cx);
                                        this.view_in_progress_dev_container(window, cx);
                                    }
                                }))
                                .map(|this| {
                                    if state.progress == DevContainerCreationProgress::Creating {
                                        this.child(
                                            ListItem::new("creating")
                                                .inset(true)
                                                .spacing(ui::ListItemSpacing::Sparse)
                                                .disabled(true)
                                                .start_slot(
                                                    Icon::new(IconName::ArrowCircle)
                                                        .color(Color::Muted)
                                                        .with_rotate_animation(2),
                                                )
                                                .child(
                                                    h_flex()
                                                        .opacity(0.6)
                                                        .gap_1()
                                                        .child(Label::new("Creating From"))
                                                        .child(
                                                            Label::new("devcontainer.json")
                                                                .buffer_font(cx),
                                                        )
                                                        .child(LoadingLabel::new("")),
                                                ),
                                        )
                                    } else {
                                        this.child(
                                            ListItem::new(
                                                "li-confirm-create-from-devcontainer-json",
                                            )
                                            .toggle_state(
                                                state.entries[0]
                                                    .focus_handle
                                                    .contains_focused(window, cx),
                                            )
                                            .inset(true)
                                            .spacing(ui::ListItemSpacing::Sparse)
                                            .start_slot(
                                                Icon::new(IconName::Plus).color(Color::Muted),
                                            )
                                            .child(
                                                h_flex()
                                                    .gap_1()
                                                    .child(Label::new("Open or Create New From"))
                                                    .child(
                                                        Label::new("devcontainer.json")
                                                            .buffer_font(cx),
                                                    ),
                                            )
                                            .on_click(
                                                cx.listener({
                                                    move |this, _, window, cx| {
                                                        this.open_dev_container(window, cx);
                                                        this.view_in_progress_dev_container(
                                                            window, cx,
                                                        );
                                                        cx.notify();
                                                    }
                                                }),
                                            ),
                                        )
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id("edit-devcontainer-json")
                                .track_focus(&state.entries[1].focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.edit_in_dev_container_json(window, cx);
                                }))
                                .child(
                                    ListItem::new("li-edit-devcontainer-json")
                                        .toggle_state(
                                            state.entries[1]
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(Icon::new(IconName::Pencil).color(Color::Muted))
                                        .child(
                                            h_flex().gap_1().child(Label::new("Edit")).child(
                                                Label::new("devcontainer.json").buffer_font(cx),
                                            ),
                                        )
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.edit_in_dev_container_json(window, cx);
                                        })),
                                ),
                        )
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("devcontainer-go-back")
                                .track_focus(&state.entries[2].focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                    cx.focus_self(window);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("li-devcontainer-go-back")
                                        .toggle_state(
                                            state.entries[2]
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ArrowLeft).color(Color::Muted),
                                        )
                                        .child(Label::new("Go Back"))
                                        .end_slot(
                                            KeyBinding::for_action_in(
                                                &menu::Cancel,
                                                &self.focus_handle,
                                                cx,
                                            )
                                            .size(rems_from_px(12.)),
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.mode =
                                                Mode::default_mode(&this.ssh_config_servers, cx);
                                            cx.focus_self(window);
                                            cx.notify()
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        );

        view = view.entry(state.entries[0].clone());
        view = view.entry(state.entries[1].clone());
        view = view.entry(state.entries[2].clone());

        view.render(window, cx).into_any_element()
    }

    fn render_create_remote_server(
        &mut self,
        state: CreateRemoteServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if state.form.port_editor.read(cx).text(cx).is_empty() {
            state.form.port_editor.update(cx, |editor, cx| {
                editor.set_text("22", window, cx);
            });
        }
        self.maybe_refresh_password_state(&state, window, cx);
        let ssh_prompt = state.ssh_prompt.clone();

        state.address_editor.update(cx, |editor, cx| {
            if editor.text(cx).is_empty() {
                editor.set_placeholder_text("user@host, host alias, or ssh command", window, cx);
            }
        });

        let selected_host = get_text(&state.form.host_editor, cx);
        let password_text = state.form.password_editor.read(cx).text(cx);
        let keychain_ready = self.ssh_keychain_url_from_form(&state, cx).is_some();
        let theme = cx.theme();
        let input_mode = state.input_mode;
        let use_command_input = input_mode == CreateRemoteServerInputMode::Command;
        let saved_connections_icon = if use_command_input {
            IconName::Terminal
        } else {
            IconName::HistoryRerun
        };
        let ssh_settings = RemoteSettings::get_global(cx);
        let saved_connections_raw: Vec<SshConnection> = ssh_settings.ssh_connections().collect();
        let mut saved_connections = Vec::new();
        let mut seen_connections = HashSet::new();
        for connection in saved_connections_raw {
            let key = (
                connection.host.clone(),
                connection.username.clone(),
                connection.port,
            );
            if seen_connections.insert(key) {
                saved_connections.push(connection);
            }
        }
        let read_ssh_config = ssh_settings.read_ssh_config;
        let show_quick_pick = ssh_prompt.is_none() && state.address_error.is_none();

        let mode_toggle = ToggleButtonGroup::single_row(
            "ssh-input-mode",
            [
                ToggleButtonSimple::new(
                    "Form",
                    cx.listener(|this, _, window, cx| {
                        if let Mode::CreateRemoteServer(state) = &mut this.mode {
                            state.input_mode = CreateRemoteServerInputMode::Form;
                            state.form.host_editor.focus_handle(cx).focus(window, cx);
                            cx.notify();
                        }
                    }),
                ),
                ToggleButtonSimple::new(
                    "SSH Command",
                    cx.listener(|this, _, window, cx| {
                        if let Mode::CreateRemoteServer(state) = &mut this.mode {
                            state.input_mode = CreateRemoteServerInputMode::Command;
                            state.address_editor.focus_handle(cx).focus(window, cx);
                            cx.notify();
                        }
                    }),
                ),
            ],
        )
        .style(ToggleButtonGroupStyle::Outlined)
        .label_size(LabelSize::Small)
        .auto_width()
        .selected_index(match input_mode {
            CreateRemoteServerInputMode::Form => 0,
            CreateRemoteServerInputMode::Command => 1,
        });

        const HISTORY_MENU_LIMIT: usize = 10;
        let mut host_history = Vec::new();
        let mut username_history = Vec::new();
        let mut port_history = Vec::new();
        let mut host_seen = HashSet::new();
        let mut username_seen = HashSet::new();
        let mut port_seen = HashSet::new();
        for connection in &saved_connections {
            if host_history.len() < HISTORY_MENU_LIMIT && host_seen.insert(&connection.host) {
                host_history.push(SharedString::from(connection.host.clone()));
            }
            if let Some(username) = &connection.username {
                if username_history.len() < HISTORY_MENU_LIMIT && username_seen.insert(username) {
                    username_history.push(SharedString::from(username.clone()));
                }
            }
            if let Some(port) = connection.port {
                if port_history.len() < HISTORY_MENU_LIMIT && port_seen.insert(port) {
                    port_history.push(SharedString::from(port.to_string()));
                }
            }
            if host_history.len() >= HISTORY_MENU_LIMIT
                && username_history.len() >= HISTORY_MENU_LIMIT
                && port_history.len() >= HISTORY_MENU_LIMIT
            {
                break;
            }
        }

        let weak_self = cx.weak_entity();
        let history_menu =
            |id: &'static str,
             entries: Vec<SharedString>,
             tooltip: &'static str,
             on_select: Rc<dyn Fn(SharedString, &mut Window, &mut App)>| {
                let has_entries = !entries.is_empty();
                PopoverMenu::new(id)
                    .trigger(
                        IconButton::new(format!("{id}-history"), IconName::HistoryRerun)
                            .icon_size(IconSize::Small)
                            .shape(IconButtonShape::Square)
                            .size(ButtonSize::Large)
                            .tooltip(Tooltip::text(tooltip))
                            .disabled(!has_entries),
                    )
                    .menu({
                        let entries = entries.clone();
                        let on_select = on_select.clone();
                        move |window, cx| {
                            let entries = entries.clone();
                            let on_select = on_select.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                if entries.is_empty() {
                                    menu = menu.header("No saved entries");
                                } else {
                                    for entry in entries.iter() {
                                        let entry = entry.clone();
                                        let on_select = on_select.clone();
                                        menu =
                                            menu.entry(entry.clone(), None, move |window, cx| {
                                                on_select(entry.clone(), window, cx);
                                            });
                                    }
                                }
                                menu
                            }))
                        }
                    })
            };

        let field = |label: SharedString, editor: Entity<Editor>, error: Option<SharedString>| {
            let mut element = v_flex()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(
                    div()
                        .border_1()
                        .border_color(theme.colors().border_variant)
                        .rounded_sm()
                        .px_2()
                        .py_1()
                        .child(editor),
                );
            if let Some(error) = error {
                element =
                    element.child(Label::new(error).size(LabelSize::Small).color(Color::Error));
            }
            element
        };

        let field_with_trailing = |label: SharedString,
                                   editor: Entity<Editor>,
                                   error: Option<SharedString>,
                                   trailing: Option<AnyElement>| {
            let mut field = v_flex()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(
                    div()
                        .border_1()
                        .border_color(theme.colors().border_variant)
                        .rounded_sm()
                        .px_2()
                        .py_1()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(div().flex_1().child(editor))
                                .when_some(trailing, |this, trailing| this.child(trailing)),
                        ),
                );
            if let Some(error) = error {
                field = field.child(Label::new(error).size(LabelSize::Small).color(Color::Error));
            }
            field
        };

        let advanced_toggle = ListItem::new("ssh-advanced-toggle")
            .inset(true)
            .spacing(ui::ListItemSpacing::Sparse)
            .start_slot(
                Icon::new(if state.show_advanced {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .color(Color::Muted),
            )
            .child(
                Label::new("Advanced options")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if let Mode::CreateRemoteServer(state) = &mut this.mode {
                    state.show_advanced = !state.show_advanced;
                    cx.notify();
                }
            }));

        let ssh_config_hint = if read_ssh_config {
            self.ssh_config_entries
                .get(selected_host.as_str())
                .map(|entry| {
                    let mut parts = Vec::new();
                    if let Some(user) = entry.user.as_deref() {
                        parts.push(format!("user {user}"));
                    }
                    if let Some(port) = entry.port {
                        parts.push(format!("port {port}"));
                    }
                    if let Some(hostname) = entry.hostname.as_deref() {
                        parts.push(format!("host {hostname}"));
                    }
                    let details = if parts.is_empty() {
                        "From SSH config".to_string()
                    } else {
                        format!("From SSH config: {}", parts.join(", "))
                    };
                    let defaults_disabled = state.ssh_prompt.is_some() || state._creating.is_some();
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Label::new(details)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("ssh-use-config-defaults", "Use config defaults")
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("Only fills empty fields from SSH config."))
                                .disabled(defaults_disabled)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.use_ssh_config_defaults(window, cx);
                                })),
                        )
                })
        } else {
            None
        };

        let host_history_button = history_menu(
            "ssh-host-history",
            host_history.clone(),
            "Host history",
            Rc::new({
                let weak_self = weak_self.clone();
                move |value, window, cx| {
                    let value = value.to_string();
                    weak_self
                        .update(cx, |this, cx| {
                            if let Mode::CreateRemoteServer(state) = &mut this.mode {
                                Self::set_editor_text(&state.form.host_editor, &value, window, cx);
                                state.form_errors.host = None;
                                state.address_error = None;
                            }
                        })
                        .ok();
                }
            }),
        )
        .into_any_element();

        let username_history_button = history_menu(
            "ssh-username-history",
            username_history.clone(),
            "Username history",
            Rc::new({
                let weak_self = weak_self.clone();
                move |value, window, cx| {
                    let value = value.to_string();
                    weak_self
                        .update(cx, |this, cx| {
                            if let Mode::CreateRemoteServer(state) = &mut this.mode {
                                Self::set_editor_text(
                                    &state.form.username_editor,
                                    &value,
                                    window,
                                    cx,
                                );
                                state.address_error = None;
                            }
                        })
                        .ok();
                }
            }),
        )
        .into_any_element();

        let port_history_button = history_menu(
            "ssh-port-history",
            port_history.clone(),
            "Port history",
            Rc::new({
                let weak_self = weak_self.clone();
                move |value, window, cx| {
                    let value = value.to_string();
                    weak_self
                        .update(cx, |this, cx| {
                            if let Mode::CreateRemoteServer(state) = &mut this.mode {
                                Self::set_editor_text(&state.form.port_editor, &value, window, cx);
                                state.form_errors.port = None;
                                state.address_error = None;
                            }
                        })
                        .ok();
                }
            }),
        )
        .into_any_element();

        let form_fields = v_flex()
            .gap_2()
            .child(field_with_trailing(
                "Host".into(),
                state.form.host_editor.clone(),
                state.form_errors.host.clone(),
                Some(host_history_button),
            ))
            .when_some(ssh_config_hint, |this, hint| this.child(hint))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        field_with_trailing(
                            "Username".into(),
                            state.form.username_editor.clone(),
                            None,
                            Some(username_history_button),
                        )
                        .w(rems(12.)),
                    )
                    .child(
                        field_with_trailing(
                            "Port".into(),
                            state.form.port_editor.clone(),
                            state.form_errors.port.clone(),
                            Some(port_history_button),
                        )
                        .w(rems(8.)),
                    )
                    .child(
                        field(
                            "Nickname (optional)".into(),
                            state.form.nickname_editor.clone(),
                            None,
                        )
                        .flex_1(),
                    ),
            )
            .child({
                let busy = state.ssh_prompt.is_some() || state._creating.is_some();
                let password_empty = password_text.is_empty();
                let can_use_saved = keychain_ready && password_empty && !busy;
                let can_save = keychain_ready && !password_empty && !busy;
                let can_delete = keychain_ready && !busy;

                let visibility_button = IconButton::new("ssh-password-visibility", IconName::Eye)
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .size(ButtonSize::Large)
                    .toggle_state(state.password_visible)
                    .tooltip(Tooltip::text(if state.password_visible {
                        "Hide password"
                    } else {
                        "Show password"
                    }))
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_password_visibility(window, cx);
                    }));

                let load_button = IconButton::new("ssh-password-use", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::text("Use saved password"))
                    .disabled(!can_use_saved)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.use_saved_password(window, cx);
                    }));

                let save_button = IconButton::new("ssh-password-save", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::text("Save password to keychain"))
                    .disabled(!can_save)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save_password_to_keychain(window, cx);
                    }));

                let delete_button = IconButton::new("ssh-password-delete", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .size(ButtonSize::Large)
                    .tooltip(Tooltip::text("Delete saved password"))
                    .disabled(!can_delete)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.delete_saved_password(window, cx);
                    }));

                let (status_label, status_color) = if !keychain_ready {
                    ("Enter host, username, and port to check keychain.", Color::Muted)
                } else {
                    match state.password_keychain_status {
                        PasswordKeychainStatus::Unknown => {
                            ("Stored in the system keychain.", Color::Muted)
                        }
                        PasswordKeychainStatus::Loading => ("Checking keychain...", Color::Muted),
                        PasswordKeychainStatus::Saved => ("Saved in keychain.", Color::Success),
                        PasswordKeychainStatus::NotSaved => ("Not saved.", Color::Muted),
                        PasswordKeychainStatus::Error => ("Keychain error.", Color::Error),
                    }
                };

                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(
                                field(
                                    "Password".into(),
                                    state.form.password_editor.clone(),
                                    None,
                                )
                                .flex_1(),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(visibility_button)
                                    .child(load_button)
                                    .child(save_button)
                                    .child(delete_button),
                            ),
                    )
                    .child(
                        Label::new(status_label)
                            .size(LabelSize::Small)
                            .color(status_color),
                    )
            })
            .child(advanced_toggle)
            .when(state.show_advanced, |this| {
                let identity_button = IconButton::new(
                    "ssh-identity-file-browse",
                    IconName::FolderOpen,
                )
                .icon_size(IconSize::Small)
                .shape(IconButtonShape::Square)
                .size(ButtonSize::Large)
                .tooltip(Tooltip::text("Choose identity file"))
                .disabled(state.ssh_prompt.is_some() || state._creating.is_some())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.pick_identity_file(window, cx);
                }));

                let mut advanced = this
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(
                                field(
                                    "Identity file".into(),
                                    state.form.identity_file_editor.clone(),
                                    None,
                                )
                                .flex_1(),
                            )
                            .child(identity_button),
                    )
                    .child(
                        Label::new("Select a private key file.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(field(
                        "Jump host".into(),
                        state.form.jump_host_editor.clone(),
                        None,
                    ))
                    .child(field(
                        "Port forwards".into(),
                        state.form.port_forwards_editor.clone(),
                        None,
                    ));
                if !state.form_errors.port_forwards.is_empty() {
                    for error in &state.form_errors.port_forwards {
                        advanced = advanced.child(
                            Label::new(error.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        );
                    }
                }
                advanced
                    .child(
                        Label::new(
                            "Port forward format: local_port:remote_host:remote_port",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(
                            "or local_host:local_port:remote_host:remote_port (comma or newline separated).",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
            });

        let input_section = match input_mode {
            CreateRemoteServerInputMode::Form => form_fields.into_any_element(),
            CreateRemoteServerInputMode::Command => v_flex()
                .gap_0p5()
                .child(
                    Label::new("SSH command")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(state.address_editor.clone())
                .into_any_element(),
        };

        let connect_button = Button::new("ssh-connect-button", "Connect")
            .on_click(cx.listener(|this, _, window, cx| {
                let state = match &this.mode {
                    Mode::CreateRemoteServer(state) => state.snapshot(),
                    _ => return,
                };
                if state.ssh_prompt.is_some() {
                    return;
                }
                match state.input_mode {
                    CreateRemoteServerInputMode::Form => {
                        this.create_ssh_server_from_form(&state, window, cx);
                    }
                    CreateRemoteServerInputMode::Command => {
                        this.create_ssh_server(&state, window, cx);
                    }
                }
            }))
            .disabled(state.ssh_prompt.is_some() || state._creating.is_some());

        let save_button = Button::new("ssh-save-button", "Save")
            .on_click(cx.listener(|this, _, _window, cx| {
                let state = match &this.mode {
                    Mode::CreateRemoteServer(state) => state.snapshot(),
                    _ => return,
                };
                if state.ssh_prompt.is_some() {
                    return;
                }

                match state.input_mode {
                    CreateRemoteServerInputMode::Form => {
                        let connection_options =
                            match this.build_ssh_connection_from_form(&state, cx) {
                                Ok(options) => options,
                                Err(errors) => {
                                    let mut new_state =
                                        state.rebuild_with(None, None, None, Some(errors));
                                    if !new_state.form_errors.port_forwards.is_empty() {
                                        new_state.show_advanced = true;
                                    }
                                    this.mode = Mode::CreateRemoteServer(new_state);
                                    return;
                                }
                            };
                        this.save_ssh_connection_options(connection_options, cx);
                        this.show_save_toast("Saved SSH connection.", cx);
                    }
                    CreateRemoteServerInputMode::Command => {
                        let input = get_text(&state.address_editor, cx);
                        if input.is_empty() {
                            let new_state = state.rebuild_with(
                                Some("Enter an SSH command to save.".into()),
                                None,
                                None,
                                Some(CreateRemoteServerFormErrors::default()),
                            );
                            this.mode = Mode::CreateRemoteServer(new_state);
                            return;
                        }
                        let connection_options =
                            match SshConnectionOptions::parse_command_line(&input) {
                                Ok(options) => options,
                                Err(error) => {
                                    let new_state = state.rebuild_with(
                                        Some(format!("could not parse: {:?}", error).into()),
                                        None,
                                        None,
                                        Some(CreateRemoteServerFormErrors::default()),
                                    );
                                    this.mode = Mode::CreateRemoteServer(new_state);
                                    return;
                                }
                            };
                        this.save_ssh_connection_options(connection_options, cx);
                        this.show_save_toast("Saved SSH connection.", cx);
                    }
                }
            }))
            .disabled(state.ssh_prompt.is_some() || state._creating.is_some());

        v_flex()
            .track_focus(&self.focus_handle(cx))
            .id("create-remote-server")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(mode_toggle)
                    .child(div().h(rems_from_px(4.)))
                    .child(input_section)
                    .child(
                        h_flex()
                            .pt_2()
                            .justify_end()
                            .gap_1()
                            .child(save_button)
                            .child(connect_button),
                    ),
            )
            .child(
                v_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .map(|this| {
                        if let Some(ssh_prompt) = ssh_prompt {
                            this.child(h_flex().w_full().child(ssh_prompt))
                        } else if let Some(address_error) = &state.address_error {
                            this.child(
                                h_flex().p_2().w_full().gap_2().child(
                                    Label::new(address_error.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                ),
                            )
                        } else {
                            this.when(show_quick_pick, |this| {
                                let mut section = v_flex().p_2().gap_1();
                                let quick_pick_tab = if read_ssh_config {
                                    state.quick_pick_tab
                                } else {
                                    QuickPickTab::SavedConnections
                                };
                                if read_ssh_config {
                                    section = section.child(
                                        h_flex().items_center().child(
                                            ToggleButtonGroup::single_row(
                                                "ssh-quick-pick-tabs",
                                                [
                                                    ToggleButtonSimple::new(
                                                        "SSH config hosts",
                                                        cx.listener(|this, _, _, cx| {
                                                            if let Mode::CreateRemoteServer(
                                                                state,
                                                            ) = &mut this.mode
                                                            {
                                                                state.quick_pick_tab =
                                                                    QuickPickTab::SshConfig;
                                                                cx.notify();
                                                            }
                                                        }),
                                                    ),
                                                    ToggleButtonSimple::new(
                                                        "Saved connections",
                                                        cx.listener(|this, _, _, cx| {
                                                            if let Mode::CreateRemoteServer(
                                                                state,
                                                            ) = &mut this.mode
                                                            {
                                                                state.quick_pick_tab =
                                                                    QuickPickTab::SavedConnections;
                                                                cx.notify();
                                                            }
                                                        }),
                                                    ),
                                                ],
                                            )
                                            .label_size(LabelSize::Small)
                                            .style(ToggleButtonGroupStyle::Outlined)
                                            .auto_width()
                                            .selected_index(match quick_pick_tab {
                                                QuickPickTab::SshConfig => 0,
                                                QuickPickTab::SavedConnections => 1,
                                            }),
                                        ),
                                    );
                                }
                                let mut ssh_config_section = v_flex().gap_1();
                                let mut saved_connections_section = v_flex().gap_1();
                                if quick_pick_tab == QuickPickTab::SshConfig {
                                    if read_ssh_config {
                                        let mut ssh_config_hosts =
                                            self.ssh_config_servers.clone();
                                        for connection in &saved_connections {
                                            ssh_config_hosts
                                                .remove(connection.host.as_str());
                                        }
                                        let ssh_config_query = state
                                            .ssh_config_search_editor
                                            .read(cx)
                                            .text(cx);
                                        let normalized_ssh_query =
                                            ssh_config_query.trim().to_lowercase();
                                        let has_ssh_query = !normalized_ssh_query.is_empty();
                                        let ssh_config_hosts: Vec<SharedString> =
                                            ssh_config_hosts
                                                .into_iter()
                                                .map(SharedString::from)
                                                .collect();
                                        let ssh_config_hosts: Vec<SharedString> =
                                            if has_ssh_query {
                                                ssh_config_hosts
                                                    .into_iter()
                                                    .filter(|host| {
                                                        let mut haystack = host.to_string();
                                                        if let Some(entry) = self
                                                            .ssh_config_entries
                                                            .get(host.as_ref())
                                                        {
                                                            if let Some(hostname) =
                                                                entry.hostname.as_ref()
                                                            {
                                                                haystack.push(' ');
                                                                haystack.push_str(hostname);
                                                            }
                                                            if let Some(user) = entry.user.as_ref()
                                                            {
                                                                haystack.push(' ');
                                                                haystack.push_str(user);
                                                            }
                                                            if let Some(port) = entry.port {
                                                                haystack.push(' ');
                                                                haystack
                                                                    .push_str(&port.to_string());
                                                            }
                                                        }
                                                        haystack
                                                            .to_lowercase()
                                                            .contains(&normalized_ssh_query)
                                                    })
                                                    .collect()
                                            } else {
                                                ssh_config_hosts
                                            };
                                        let ssh_config_total = ssh_config_hosts.len();
                                        let ssh_config_total_pages =
                                            if ssh_config_total == 0 {
                                                0
                                            } else {
                                                (ssh_config_total + SAVED_CONNECTIONS_LIMIT - 1)
                                                    / SAVED_CONNECTIONS_LIMIT
                                            };
                                        let ssh_config_page = if ssh_config_total_pages == 0 {
                                            0
                                        } else {
                                            state
                                                .ssh_config_hosts_page
                                                .min(
                                                    ssh_config_total_pages
                                                        .saturating_sub(1),
                                                )
                                        };
                                        let visible_ssh_config_hosts: Vec<SharedString> =
                                            ssh_config_hosts
                                                .iter()
                                                .skip(
                                                    ssh_config_page
                                                        * SAVED_CONNECTIONS_LIMIT,
                                                )
                                                .take(SAVED_CONNECTIONS_LIMIT)
                                                .cloned()
                                                .collect();
                                        ssh_config_section = ssh_config_section.child(
                                            h_flex().items_center().child(
                                                Label::new("SSH config hosts")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                        );
                                        ssh_config_section = ssh_config_section.child(
                                                h_flex()
                                                    .w_full()
                                                    .items_center()
                                                    .gap_1()
                                                    .border_1()
                                                .border_color(theme.colors().border_variant)
                                                .rounded_sm()
                                                .px_2()
                                                .py_1()
                                                .child(
                                                    Icon::new(IconName::MagnifyingGlass)
                                                        .color(Color::Muted),
                                                )
                                                .child(state.ssh_config_search_editor.clone())
                                                .when(has_ssh_query, |this| {
                                                    this.child(
                                                        IconButton::new(
                                                            "ssh-config-search-clear",
                                                            IconName::Close,
                                                        )
                                                        .icon_size(IconSize::XSmall)
                                                        .icon_color(Color::Muted)
                                                        .shape(IconButtonShape::Square)
                                                        .size(ButtonSize::Compact)
                                                        .tooltip(Tooltip::text("Clear search"))
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                if let Mode::CreateRemoteServer(
                                                                    state,
                                                                ) = &mut this.mode
                                                                {
                                                                    state
                                                                        .ssh_config_search_editor
                                                                        .update(
                                                                            cx,
                                                                            |editor, cx| {
                                                                                editor.set_text(
                                                                                    "",
                                                                                    window,
                                                                                    cx,
                                                                                );
                                                                            },
                                                                        );
                                                                    state.ssh_config_hosts_page = 0;
                                                                    cx.notify();
                                                                }
                                                            },
                                                        )),
                                                    )
                                                }),
                                        );
                                        if visible_ssh_config_hosts.is_empty() {
                                            ssh_config_section = ssh_config_section.child(
                                                Label::new(if has_ssh_query {
                                                    "No SSH config hosts match your search."
                                                } else {
                                                    "No SSH config hosts found."
                                                })
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                            );
                                        } else {
                                            ssh_config_section = ssh_config_section.child(
                                                List::new().children(
                                                    visible_ssh_config_hosts
                                                        .iter()
                                                        .enumerate()
                                                        .map(|(ix, host)| {
                                                            let detail = self
                                                                .ssh_config_entries
                                                                .get(host.as_ref())
                                                                .map(|entry| {
                                                                    let hostname = entry
                                                                        .hostname
                                                                        .as_deref()
                                                                        .unwrap_or(&entry.host);
                                                                    let user = entry
                                                                        .user
                                                                        .as_deref()
                                                                        .unwrap_or("user");
                                                                    let port =
                                                                        entry.port.unwrap_or(22);
                                                                    format!(
                                                                        "{user}@{hostname}:{port}"
                                                                    )
                                                                })
                                                                .unwrap_or_else(|| {
                                                                    host.to_string()
                                                                });
                                                            let title = host.clone();
                                                            ListItem::new(("ssh-config-host", ix))
                                                                .inset(true)
                                                                .spacing(ui::ListItemSpacing::Sparse)
                                                                .start_slot(
                                                                    Icon::new(IconName::Server)
                                                                        .color(Color::Muted),
                                                                )
                                                                .child(
                                                                    v_flex()
                                                                        .gap_0p5()
                                                                        .child(Label::new(title))
                                                                        .child(
                                                                            Label::new(detail)
                                                                                .size(LabelSize::Small)
                                                                                .color(Color::Muted),
                                                                        ),
                                                                )
                                                                .end_slot(
                                                                    Label::new("Use")
                                                                        .size(LabelSize::Small)
                                                                        .color(Color::Muted),
                                                                )
                                                                .on_click(cx.listener({
                                                                    let host = host.clone();
                                                                    let use_command_input =
                                                                        use_command_input;
                                                                    move |this, _, window, cx| {
                                                                        if use_command_input {
                                                                            let command =
                                                                                format!(
                                                                                    "ssh {}",
                                                                                    host
                                                                                );
                                                                            this.set_command_text(
                                                                                &command,
                                                                                window,
                                                                                cx,
                                                                            );
                                                                        } else {
                                                                            this.use_ssh_config_host(
                                                                                host.clone(),
                                                                                window,
                                                                                cx,
                                                                            );
                                                                        }
                                                                    }
                                                                }))
                                                                .into_any_element()
                                                        }),
                                                ),
                                            );
                                            if ssh_config_total_pages > 1 {
                                                ssh_config_section = ssh_config_section.child(
                                                    h_flex()
                                                        .gap_1()
                                                        .items_center()
                                                        .child(
                                                            IconButton::new(
                                                                "ssh-config-page-prev",
                                                                IconName::ChevronLeft,
                                                            )
                                                            .icon_size(IconSize::XSmall)
                                                            .icon_color(Color::Muted)
                                                            .shape(IconButtonShape::Square)
                                                            .size(ButtonSize::Compact)
                                                            .disabled(ssh_config_page == 0)
                                                            .tooltip(Tooltip::text(
                                                                "Previous page",
                                                            ))
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    if let Mode::CreateRemoteServer(
                                                                        state,
                                                                    ) = &mut this.mode
                                                                    {
                                                                        state
                                                                            .ssh_config_hosts_page =
                                                                            ssh_config_page
                                                                                .saturating_sub(1);
                                                                        cx.notify();
                                                                    }
                                                                },
                                                            )),
                                                        )
                                                        .child(
                                                            Label::new(format!(
                                                                "Page {} of {}",
                                                                ssh_config_page + 1,
                                                                ssh_config_total_pages
                                                            ))
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                        )
                                                        .child(
                                                            IconButton::new(
                                                                "ssh-config-page-next",
                                                                IconName::ChevronRight,
                                                            )
                                                            .icon_size(IconSize::XSmall)
                                                            .icon_color(Color::Muted)
                                                            .shape(IconButtonShape::Square)
                                                            .size(ButtonSize::Compact)
                                                            .disabled(
                                                                ssh_config_page + 1
                                                                    >= ssh_config_total_pages,
                                                            )
                                                            .tooltip(Tooltip::text("Next page"))
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    if let Mode::CreateRemoteServer(
                                                                        state,
                                                                    ) = &mut this.mode
                                                                    {
                                                                        state
                                                                            .ssh_config_hosts_page =
                                                                            ssh_config_page
                                                                                .saturating_add(1);
                                                                        cx.notify();
                                                                    }
                                                                },
                                                            )),
                                                        ),
                                                );
                                            }
                                        }
                                    }
                                }
                                if quick_pick_tab == QuickPickTab::SavedConnections {
                                    let saved_connections_total = saved_connections.len();
                                    let search_query = state
                                        .saved_connections_search_editor
                                        .read(cx)
                                        .text(cx);
                                    let normalized_query = search_query.trim().to_lowercase();
                                    let has_search_query = !normalized_query.is_empty();
                                    let filtered_connections: Vec<SshConnection> =
                                        if has_search_query {
                                            saved_connections
                                                .iter()
                                                .filter(|connection| {
                                                    let mut haystack = String::new();
                                                    haystack
                                                        .push_str(&connection.host);
                                                    if let Some(username) =
                                                        &connection.username
                                                    {
                                                        haystack.push(' ');
                                                        haystack.push_str(username);
                                                    }
                                                    if let Some(port) = connection.port {
                                                        haystack.push(' ');
                                                        haystack
                                                            .push_str(&port.to_string());
                                                    }
                                                    if let Some(nickname) =
                                                        &connection.nickname
                                                    {
                                                        haystack.push(' ');
                                                        haystack.push_str(nickname);
                                                    }
                                                    for arg in &connection.args {
                                                        haystack.push(' ');
                                                        haystack.push_str(arg);
                                                    }
                                                    if let Some(port_forwards) =
                                                        &connection.port_forwards
                                                    {
                                                        for forward in port_forwards {
                                                            let local_host = forward
                                                                .local_host
                                                                .as_deref()
                                                                .unwrap_or("localhost");
                                                            let remote_host = forward
                                                                .remote_host
                                                                .as_deref()
                                                                .unwrap_or("localhost");
                                                            haystack.push(' ');
                                                            haystack.push_str(&format!(
                                                                "{local_host}:{}:{remote_host}:{}",
                                                                forward.local_port,
                                                                forward.remote_port
                                                            ));
                                                        }
                                                    }
                                                    let connection_string =
                                                        SshConnectionOptions::from(
                                                            (*connection).clone(),
                                                        )
                                                        .connection_string();
                                                    haystack.push(' ');
                                                    haystack.push_str(&connection_string);
                                                    haystack
                                                        .to_lowercase()
                                                        .contains(&normalized_query)
                                                })
                                                .cloned()
                                                .collect()
                                        } else {
                                            saved_connections.clone()
                                        };
                                    let filtered_total = filtered_connections.len();
                                    let total_pages = if filtered_total == 0 {
                                        0
                                    } else {
                                        (filtered_total + SAVED_CONNECTIONS_LIMIT - 1)
                                            / SAVED_CONNECTIONS_LIMIT
                                    };
                                    let current_page = if total_pages == 0 {
                                        0
                                    } else {
                                        state
                                            .saved_connections_page
                                            .min(total_pages.saturating_sub(1))
                                    };
                                    let visible_saved_connections: Vec<SshConnection> =
                                        filtered_connections
                                            .iter()
                                            .skip(
                                                current_page
                                                    * SAVED_CONNECTIONS_LIMIT,
                                            )
                                            .take(SAVED_CONNECTIONS_LIMIT)
                                            .cloned()
                                            .collect();
                                    let saved_connections_truncated =
                                        filtered_total
                                            > visible_saved_connections.len();
                                    saved_connections_section = saved_connections_section.child(
                                        h_flex()
                                            .items_center()
                                            .justify_between()
                                            .child(
                                            Label::new("Saved connections")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            h_flex().gap_1().items_center().child(
                                                IconButton::new(
                                                    "ssh-saved-delete-all",
                                                    IconName::Trash,
                                                )
                                                .icon_size(IconSize::XSmall)
                                                .icon_color(Color::Error)
                                                .shape(IconButtonShape::Square)
                                                .size(ButtonSize::Compact)
                                                .tooltip(Tooltip::text(
                                                    "Delete all saved connections",
                                                ))
                                                .disabled(saved_connections_total == 0)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.confirm_remove_all_saved_connections(
                                                        window, cx,
                                                    );
                                                })),
                                            ),
                                        ),
                                );
                                    saved_connections_section = saved_connections_section.child(
                                        h_flex()
                                            .w_full()
                                            .items_center()
                                            .gap_1()
                                            .border_1()
                                            .border_color(theme.colors().border_variant)
                                            .rounded_sm()
                                            .px_2()
                                            .py_1()
                                            .child(
                                                Icon::new(IconName::MagnifyingGlass)
                                                    .color(Color::Muted),
                                            )
                                            .child(state.saved_connections_search_editor.clone())
                                            .when(has_search_query, |this| {
                                                this.child(
                                                    IconButton::new(
                                                        "ssh-saved-search-clear",
                                                        IconName::Close,
                                                    )
                                                    .icon_size(IconSize::XSmall)
                                                    .icon_color(Color::Muted)
                                                    .shape(IconButtonShape::Square)
                                                    .size(ButtonSize::Compact)
                                                    .tooltip(Tooltip::text("Clear search"))
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            if let Mode::CreateRemoteServer(
                                                                state,
                                                            ) = &mut this.mode
                                                            {
                                                                state
                                                                    .saved_connections_search_editor
                                                                    .update(
                                                                        cx,
                                                                        |editor, cx| {
                                                                            editor.set_text(
                                                                                "",
                                                                                window,
                                                                                cx,
                                                                            );
                                                                        },
                                                                    );
                                                                state.saved_connections_page = 0;
                                                                cx.notify();
                                                            }
                                                        },
                                                    )),
                                                )
                                            }),
                                    );
                                    if visible_saved_connections.is_empty() {
                                        saved_connections_section = saved_connections_section.child(
                                            Label::new(if has_search_query {
                                                "No saved connections match your search."
                                            } else {
                                                "No saved connections yet."
                                            })
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        );
                                    } else {
                                        saved_connections_section = saved_connections_section.child(
                                            List::new().children(
                                                visible_saved_connections
                                                    .iter()
                                                    .enumerate()
                                                    .map(|(ix, connection)| {
                                                        let connection_string =
                                                            SshConnectionOptions::from(
                                                                connection.clone(),
                                                            )
                                                            .connection_string();
                                                        let title = connection
                                                            .nickname
                                                            .clone()
                                                            .unwrap_or_else(|| {
                                                                connection.host.clone()
                                                            });
                                                        let mut detail = connection_string.clone();
                                                        let mut extras = Vec::new();
                                                        let mut identity_file = None;
                                                        let mut jump_host = None;
                                                        let mut args_iter = connection.args.iter();
                                                        while let Some(arg) = args_iter.next() {
                                                            if arg == "-i" {
                                                                identity_file =
                                                                    args_iter.next().cloned();
                                                            } else if let Some(rest) =
                                                                arg.strip_prefix("-i")
                                                            {
                                                                if !rest.is_empty() {
                                                                    identity_file =
                                                                        Some(rest.to_string());
                                                                }
                                                            } else if arg == "-J" {
                                                                jump_host =
                                                                    args_iter.next().cloned();
                                                            } else if let Some(rest) =
                                                                arg.strip_prefix("-J")
                                                            {
                                                                if !rest.is_empty() {
                                                                    jump_host =
                                                                        Some(rest.to_string());
                                                                }
                                                            }
                                                        }
                                                        if let Some(identity_file) = identity_file {
                                                            extras.push(format!(
                                                                "identity {identity_file}"
                                                            ));
                                                        }
                                                        if let Some(jump_host) = jump_host {
                                                            extras.push(format!("jump {jump_host}"));
                                                        }
                                                        if let Some(port_forwards) =
                                                            connection.port_forwards.as_ref()
                                                        {
                                                            if !port_forwards.is_empty() {
                                                                extras.push(format!(
                                                                    "{} forwards",
                                                                    port_forwards.len()
                                                                ));
                                                            }
                                                        }
                                                        if !extras.is_empty() {
                                                            detail = format!(
                                                                "{} | {}",
                                                                detail,
                                                                extras.join(" | ")
                                                            );
                                                        }
                                                        ListItem::new(("ssh-saved-connection", ix))
                                                            .inset(true)
                                                            .spacing(ui::ListItemSpacing::Sparse)
                                                            .start_slot(
                                                                Icon::new(saved_connections_icon)
                                                                    .color(Color::Muted),
                                                            )
                                                            .child(
                                                                v_flex()
                                                                    .gap_0p5()
                                                                    .child(Label::new(title))
                                                                    .child(
                                                                        Label::new(detail)
                                                                            .size(LabelSize::Small)
                                                                            .color(Color::Muted),
                                                                    ),
                                                            )
                                                            .end_slot(
                                                                h_flex()
                                                                    .gap_1()
                                                                    .items_center()
                                                                    .child(
                                                                        Label::new("Use")
                                                                            .size(LabelSize::Small)
                                                                            .color(Color::Muted),
                                                                    )
                                                                .child(
                                                                    IconButton::new(
                                                                        ("ssh-saved-delete", ix),
                                                                        IconName::Trash,
                                                                    )
                                                                    .icon_size(IconSize::XSmall)
                                                                    .icon_color(Color::Muted)
                                                                    .shape(IconButtonShape::Square)
                                                                    .size(ButtonSize::Compact)
                                                                    .tooltip(Tooltip::text(
                                                                        "Delete saved connection",
                                                                    ))
                                                                    .on_click(cx.listener({
                                                                        let connection =
                                                                            connection.clone();
                                                                        move |this, _, window, cx| {
                                                                            cx.stop_propagation();
                                                                            this.confirm_remove_saved_connection(
                                                                                connection.clone(),
                                                                                window,
                                                                                cx,
                                                                            );
                                                                        }
                                                                    })),
                                                                ),
                                                            )
                                                            .on_click(cx.listener({
                                                                let connection = connection.clone();
                                                                let use_command_input =
                                                                    use_command_input;
                                                                move |this, _, window, cx| {
                                                                    if use_command_input {
                                                                        let command =
                                                                            Self::ssh_command_string(
                                                                                &connection,
                                                                            );
                                                                        this.set_command_text(
                                                                            &command,
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    } else {
                                                                        this.apply_saved_connection(
                                                                            connection.clone(),
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    }
                                                                }
                                                            }))
                                                            .into_any_element()
                                                    }),
                                            ),
                                        );
                                        if total_pages > 1 {
                                            saved_connections_section = saved_connections_section.child(
                                                h_flex()
                                                    .gap_1()
                                                    .items_center()
                                                    .child(
                                                        IconButton::new(
                                                            "ssh-saved-page-prev",
                                                            IconName::ChevronLeft,
                                                        )
                                                        .icon_size(IconSize::XSmall)
                                                        .icon_color(Color::Muted)
                                                        .shape(IconButtonShape::Square)
                                                        .size(ButtonSize::Compact)
                                                        .disabled(current_page == 0)
                                                        .tooltip(Tooltip::text("Previous page"))
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if let Mode::CreateRemoteServer(
                                                                    state,
                                                                ) = &mut this.mode
                                                                {
                                                                    state.saved_connections_page =
                                                                        current_page
                                                                            .saturating_sub(1);
                                                                    cx.notify();
                                                                }
                                                            },
                                                        )),
                                                    )
                                                    .child(
                                                        Label::new(format!(
                                                            "Page {} of {}",
                                                            current_page + 1,
                                                            total_pages
                                                        ))
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted),
                                                    )
                                                    .child(
                                                        IconButton::new(
                                                            "ssh-saved-page-next",
                                                            IconName::ChevronRight,
                                                        )
                                                        .icon_size(IconSize::XSmall)
                                                        .icon_color(Color::Muted)
                                                        .shape(IconButtonShape::Square)
                                                        .size(ButtonSize::Compact)
                                                        .disabled(current_page + 1 >= total_pages)
                                                        .tooltip(Tooltip::text("Next page"))
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if let Mode::CreateRemoteServer(
                                                                    state,
                                                                ) = &mut this.mode
                                                                {
                                                                    state.saved_connections_page =
                                                                        current_page
                                                                            .saturating_add(1);
                                                                    cx.notify();
                                                                }
                                                            },
                                                        )),
                                                    ),
                                            );
                                        }
                                        if filtered_total > 0 && saved_connections_truncated {
                                            let suffix = if has_search_query {
                                                "matching connections"
                                            } else {
                                                "saved connections"
                                            };
                                            let message = format!(
                                                "Showing {} of {} {}.",
                                                visible_saved_connections.len(),
                                                filtered_total,
                                                suffix
                                            );
                                            saved_connections_section = saved_connections_section.child(
                                                Label::new(message)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            );
                                        }
                                    }
                                }
                                let content = match quick_pick_tab {
                                    QuickPickTab::SshConfig => ssh_config_section.into_any_element(),
                                    QuickPickTab::SavedConnections => {
                                        saved_connections_section.into_any_element()
                                    }
                                };
                                section = section.child(content);
                                this.child(section).child(ListSeparator)
                            })
                            .child(
                                h_flex()
                                    .p_2()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Label::new(
                                            "Pick a host from your SSH config, use the form, or paste an SSH command.",
                                        )
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        Button::new("learn-more", "Learn More")
                                            .label_size(LabelSize::Small)
                                            .icon(IconName::ArrowUpRight)
                                            .icon_size(IconSize::XSmall)
                                            .on_click(|_, _, cx| {
                                                cx.open_url(
                                                    "https://zed.dev/docs/remote-development",
                                                );
                                            }),
                                    ),
                            )
                        }
                    }),
            )
    }

    fn add_ssh_server_with_index(
        &mut self,
        connection_options: remote::SshConnectionOptions,
        cx: &mut Context<Self>,
    ) -> SshServerIndex {
        let new_ix = Arc::new(AtomicUsize::new(0));
        let update_new_ix = new_ix.clone();
        self.update_settings_file(cx, move |settings, _| {
            update_new_ix.store(
                settings
                    .ssh_connections
                    .as_ref()
                    .map_or(0, |connections| connections.len()),
                atomic::Ordering::Release,
            );
        });
        self.add_ssh_server(connection_options, cx);
        SshServerIndex(new_ix.load(atomic::Ordering::Acquire))
    }

    #[cfg(target_os = "windows")]
    fn render_add_wsl_distro(
        &self,
        state: &AddWslDistro,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_prompt = state.connection_prompt.clone();

        state.picker.update(cx, |picker, cx| {
            picker.focus_handle(cx).focus(window, cx);
        });

        v_flex()
            .id("add-wsl-distro")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .map(|this| {
                if let Some(connection_prompt) = connection_prompt {
                    this.child(connection_prompt)
                } else {
                    this.child(state.picker.clone())
                }
            })
    }

    fn render_view_options(
        &mut self,
        options: ViewServerOptionsState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let last_entry = options.entries().last().unwrap();

        let mut view = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(match &options {
                    ViewServerOptionsState::Ssh { connection, .. } => SshConnectionHeader {
                        connection_string: connection.host.to_string().into(),
                        paths: Default::default(),
                        nickname: connection.nickname.clone().map(|s| s.into()),
                        is_wsl: false,
                        is_devcontainer: false,
                    }
                    .render(window, cx)
                    .into_any_element(),
                    ViewServerOptionsState::Wsl { connection, .. } => SshConnectionHeader {
                        connection_string: connection.distro_name.clone().into(),
                        paths: Default::default(),
                        nickname: None,
                        is_wsl: true,
                        is_devcontainer: false,
                    }
                    .render(window, cx)
                    .into_any_element(),
                    ViewServerOptionsState::DevContainer { connection, .. } => {
                        SshConnectionHeader {
                            connection_string: connection.name.clone().into(),
                            paths: Default::default(),
                            nickname: None,
                            is_wsl: false,
                            is_devcontainer: true,
                        }
                        .render(window, cx)
                        .into_any_element()
                    }
                })
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .map(|this| match &options {
                            ViewServerOptionsState::Ssh {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_ssh(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                            ViewServerOptionsState::Wsl {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_wsl(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                            ViewServerOptionsState::DevContainer {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_dev_container(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                        })
                        .child(ListSeparator)
                        .child({
                            div()
                                .id("ssh-options-copy-server-address")
                                .track_focus(&last_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                    cx.focus_self(window);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("go-back")
                                        .toggle_state(
                                            last_entry.focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ArrowLeft).color(Color::Muted),
                                        )
                                        .child(Label::new("Go Back"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.mode =
                                                Mode::default_mode(&this.ssh_config_servers, cx);
                                            cx.focus_self(window);
                                            cx.notify()
                                        })),
                                )
                        }),
                )
                .into_any_element(),
        );

        for entry in options.entries() {
            view = view.entry(entry.clone());
        }

        view.render(window, cx).into_any_element()
    }

    fn render_edit_wsl(
        &self,
        connection: &WslConnectionOptions,
        index: WslServerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let distro_name = SharedString::new(connection.distro_name.clone());

        v_flex().child({
            fn remove_wsl_distro(
                remote_servers: Entity<RemoteServerProjects>,
                index: WslServerIndex,
                distro_name: SharedString,
                window: &mut Window,
                cx: &mut App,
            ) {
                let prompt_message = format!("Remove WSL distro `{}`?", distro_name);

                let confirmation = window.prompt(
                    PromptLevel::Warning,
                    &prompt_message,
                    None,
                    &["Yes, remove it", "No, keep it"],
                    cx,
                );

                cx.spawn(async move |cx| {
                    if confirmation.await.ok() == Some(0) {
                        remote_servers.update(cx, |this, cx| {
                            this.delete_wsl_distro(index, cx);
                        });
                        remote_servers.update(cx, |this, cx| {
                            this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                            cx.notify();
                        });
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            div()
                .id("wsl-options-remove-distro")
                .track_focus(&entries[0].focus_handle)
                .on_action(cx.listener({
                    let distro_name = distro_name.clone();
                    move |_, _: &menu::Confirm, window, cx| {
                        remove_wsl_distro(cx.entity(), index, distro_name.clone(), window, cx);
                        cx.focus_self(window);
                    }
                }))
                .child(
                    ListItem::new("remove-distro")
                        .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                        .inset(true)
                        .spacing(ui::ListItemSpacing::Sparse)
                        .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                        .child(Label::new("Remove Distro").color(Color::Error))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            remove_wsl_distro(cx.entity(), index, distro_name.clone(), window, cx);
                            cx.focus_self(window);
                        })),
                )
        })
    }

    fn render_edit_dev_container(
        &self,
        connection: &DevContainerConnection,
        index: DevContainerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_name = SharedString::new(connection.name.clone());
        let connection_for_disconnect = connection.clone();
        let connection_for_stop = connection.clone();
        let connection_for_remove = connection.clone();
        let connection_for_reconnect = connection.clone();
        let recent_project_paths = RemoteSettings::get_global(cx)
            .dev_container_connections()
            .nth(index.0)
            .and_then(|connection| {
                connection
                    .projects
                    .iter()
                    .next()
                    .map(|project| project.paths.clone())
            });

        v_flex()
            .child({
                let label = "Rename Dev Container";
                div()
                    .id("devcontainer-options-rename")
                    .track_focus(&entries[0].focus_handle)
                    .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                        this.mode = Mode::EditDevContainerName(EditDevContainerNameState::new(
                            index, window, cx,
                        ));
                        cx.notify();
                    }))
                    .child(
                        ListItem::new("rename-devcontainer")
                            .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Pencil).color(Color::Muted))
                            .child(Label::new(label))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.mode = Mode::EditDevContainerName(
                                    EditDevContainerNameState::new(index, window, cx),
                                );
                                cx.notify();
                            })),
                    )
            })
            .child({
                div()
                    .id("devcontainer-options-refresh")
                    .track_focus(&entries[1].focus_handle)
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.refresh_dev_container_connections(window, cx);
                    }))
                    .child(
                        ListItem::new("refresh-devcontainer")
                            .toggle_state(entries[1].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::RotateCw).color(Color::Muted))
                            .child(Label::new("Refresh Dev Containers"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh_dev_container_connections(window, cx);
                            })),
                    )
            })
            .child({
                div()
                    .id("devcontainer-options-reconnect")
                    .track_focus(&entries[2].focus_handle)
                    .on_action(cx.listener({
                        let connection = connection_for_reconnect.clone();
                        let recent_project_paths = recent_project_paths.clone();
                        move |this, _: &menu::Confirm, window, cx| {
                            if let Some(paths) = recent_project_paths.clone() {
                                this.open_remote_project_from_paths(
                                    Connection::DevContainer(connection.clone()),
                                    paths,
                                    window,
                                    cx,
                                );
                            } else {
                                this.create_remote_project(
                                    ServerIndex::DevContainer(index),
                                    Connection::DevContainer(connection.clone()).into(),
                                    window,
                                    cx,
                                );
                            }
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("reconnect-devcontainer")
                            .toggle_state(entries[2].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::PlayFilled).color(Color::Muted))
                            .child(Label::new("Reconnect Dev Container"))
                            .on_click(cx.listener({
                                let connection = connection_for_reconnect.clone();
                                let recent_project_paths = recent_project_paths.clone();
                                move |this, _, window, cx| {
                                    if let Some(paths) = recent_project_paths.clone() {
                                        this.open_remote_project_from_paths(
                                            Connection::DevContainer(connection.clone()),
                                            paths,
                                            window,
                                            cx,
                                        );
                                    } else {
                                        this.create_remote_project(
                                            ServerIndex::DevContainer(index),
                                            Connection::DevContainer(connection.clone()).into(),
                                            window,
                                            cx,
                                        );
                                    }
                                    cx.focus_self(window);
                                }
                            })),
                    )
            })
            .child({
                div()
                    .id("devcontainer-options-disconnect")
                    .track_focus(&entries[3].focus_handle)
                    .on_action(cx.listener({
                        let connection_name = connection_name.clone();
                        let connection = connection_for_disconnect.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            Self::prompt_disconnect_dev_container(
                                cx.entity(),
                                connection_name.clone(),
                                connection.clone(),
                                window,
                                cx,
                            );
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("disconnect-devcontainer")
                            .toggle_state(entries[3].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Disconnected).color(Color::Muted))
                            .child(Label::new("Disconnect Dev Container"))
                            .on_click(cx.listener({
                                let connection_name = connection_name.clone();
                                let connection = connection_for_disconnect.clone();
                                move |_, _, window, cx| {
                                    Self::prompt_disconnect_dev_container(
                                        cx.entity(),
                                        connection_name.clone(),
                                        connection.clone(),
                                        window,
                                        cx,
                                    );
                                    cx.focus_self(window);
                                }
                            })),
                    )
            })
            .child({
                div()
                    .id("devcontainer-options-return-host")
                    .track_focus(&entries[4].focus_handle)
                    .on_action(cx.listener({
                        let connection_name = connection_name.clone();
                        let connection = connection.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            Self::prompt_return_to_host_folder(
                                cx.entity(),
                                connection_name.clone(),
                                connection.clone(),
                                window,
                                cx,
                            );
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("return-host-devcontainer")
                            .toggle_state(entries[4].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Exit).color(Color::Muted))
                            .child(Label::new("Return to Host Folder"))
                            .on_click(cx.listener({
                                let connection_name = connection_name.clone();
                                let connection = connection.clone();
                                move |_, _, window, cx| {
                                    Self::prompt_return_to_host_folder(
                                        cx.entity(),
                                        connection_name.clone(),
                                        connection.clone(),
                                        window,
                                        cx,
                                    );
                                    cx.focus_self(window);
                                }
                            })),
                    )
            })
            .child({
                div()
                    .id("devcontainer-options-stop")
                    .track_focus(&entries[5].focus_handle)
                    .on_action(cx.listener({
                        let connection_name = connection_name.clone();
                        let connection = connection_for_stop.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            Self::prompt_stop_dev_container(
                                cx.entity(),
                                connection_name.clone(),
                                connection.clone(),
                                window,
                                cx,
                            );
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("stop-devcontainer")
                            .toggle_state(entries[5].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Stop).color(Color::Warning))
                            .child(Label::new("Stop Container and Disconnect"))
                            .on_click(cx.listener({
                                let connection_name = connection_name.clone();
                                let connection = connection_for_stop.clone();
                                move |_, _, window, cx| {
                                    Self::prompt_stop_dev_container(
                                        cx.entity(),
                                        connection_name.clone(),
                                        connection.clone(),
                                        window,
                                        cx,
                                    );
                                    cx.focus_self(window);
                                }
                            })),
                    )
            })
            .child({
                div()
                    .id("devcontainer-options-remove")
                    .track_focus(&entries[6].focus_handle)
                    .on_action(cx.listener({
                        let connection_name = connection_name.clone();
                        let connection = connection_for_remove.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            Self::prompt_remove_dev_container(
                                cx.entity(),
                                index,
                                connection_name.clone(),
                                connection.clone(),
                                window,
                                cx,
                            );
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("remove-devcontainer")
                            .toggle_state(entries[6].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                            .child(Label::new("Remove Dev Container").color(Color::Error))
                            .on_click(cx.listener({
                                let connection_name = connection_name.clone();
                                let connection = connection_for_remove.clone();
                                move |_, _, window, cx| {
                                    Self::prompt_remove_dev_container(
                                        cx.entity(),
                                        index,
                                        connection_name.clone(),
                                        connection.clone(),
                                        window,
                                        cx,
                                    );
                                    cx.focus_self(window);
                                }
                            })),
                    )
            })
    }

    fn render_edit_ssh(
        &self,
        connection: &SshConnectionOptions,
        index: SshServerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_string = SharedString::new(connection.host.to_string());

        v_flex()
            .child({
                let label = if connection.nickname.is_some() {
                    "Edit Nickname"
                } else {
                    "Add Nickname to Server"
                };
                div()
                    .id("ssh-options-add-nickname")
                    .track_focus(&entries[0].focus_handle)
                    .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                        this.mode = Mode::EditNickname(EditNicknameState::new(index, window, cx));
                        cx.notify();
                    }))
                    .child(
                        ListItem::new("add-nickname")
                            .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Pencil).color(Color::Muted))
                            .child(Label::new(label))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.mode =
                                    Mode::EditNickname(EditNicknameState::new(index, window, cx));
                                cx.notify();
                            })),
                    )
            })
            .child({
                let workspace = self.workspace.clone();
                fn callback(
                    workspace: WeakEntity<Workspace>,
                    connection_string: SharedString,
                    cx: &mut App,
                ) {
                    cx.write_to_clipboard(ClipboardItem::new_string(connection_string.to_string()));
                    workspace
                        .update(cx, |this, cx| {
                            struct SshServerAddressCopiedToClipboard;
                            let notification = format!(
                                "Copied server address ({}) to clipboard",
                                connection_string
                            );

                            this.show_toast(
                                Toast::new(
                                    NotificationId::composite::<SshServerAddressCopiedToClipboard>(
                                        connection_string.clone(),
                                    ),
                                    notification,
                                )
                                .autohide(),
                                cx,
                            );
                        })
                        .ok();
                }
                div()
                    .id("ssh-options-copy-server-address")
                    .track_focus(&entries[1].focus_handle)
                    .on_action({
                        let connection_string = connection_string.clone();
                        let workspace = self.workspace.clone();
                        move |_: &menu::Confirm, _, cx| {
                            callback(workspace.clone(), connection_string.clone(), cx);
                        }
                    })
                    .child(
                        ListItem::new("copy-server-address")
                            .toggle_state(entries[1].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Copy).color(Color::Muted))
                            .child(Label::new("Copy Server Address"))
                            .end_hover_slot(
                                Label::new(connection_string.clone()).color(Color::Muted),
                            )
                            .on_click({
                                let connection_string = connection_string.clone();
                                move |_, _, cx| {
                                    callback(workspace.clone(), connection_string.clone(), cx);
                                }
                            }),
                    )
            })
            .child({
                fn remove_ssh_server(
                    remote_servers: Entity<RemoteServerProjects>,
                    index: SshServerIndex,
                    connection_string: SharedString,
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    let prompt_message = format!("Remove server `{}`?", connection_string);

                    let confirmation = window.prompt(
                        PromptLevel::Warning,
                        &prompt_message,
                        None,
                        &["Yes, remove it", "No, keep it"],
                        cx,
                    );

                    cx.spawn(async move |cx| {
                        if confirmation.await.ok() == Some(0) {
                            remote_servers.update(cx, |this, cx| {
                                this.delete_ssh_server(index, cx);
                            });
                            remote_servers.update(cx, |this, cx| {
                                this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                cx.notify();
                            });
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }
                div()
                    .id("ssh-options-copy-server-address")
                    .track_focus(&entries[2].focus_handle)
                    .on_action(cx.listener({
                        let connection_string = connection_string.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            remove_ssh_server(
                                cx.entity(),
                                index,
                                connection_string.clone(),
                                window,
                                cx,
                            );
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("remove-server")
                            .toggle_state(entries[2].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                            .child(Label::new("Remove Server").color(Color::Error))
                            .on_click(cx.listener(move |_, _, window, cx| {
                                remove_ssh_server(
                                    cx.entity(),
                                    index,
                                    connection_string.clone(),
                                    window,
                                    cx,
                                );
                                cx.focus_self(window);
                            })),
                    )
            })
    }

    fn render_edit_nickname(
        &self,
        state: &EditNicknameState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(connection) = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(state.index.0)
        else {
            return v_flex()
                .id("ssh-edit-nickname")
                .track_focus(&self.focus_handle(cx));
        };

        let connection_string = connection.host.clone();
        let nickname = connection.nickname.map(|s| s.into());

        v_flex()
            .id("ssh-edit-nickname")
            .track_focus(&self.focus_handle(cx))
            .child(
                SshConnectionHeader {
                    connection_string: connection_string.into(),
                    paths: Default::default(),
                    nickname,
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx),
            )
            .child(
                h_flex()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(state.editor.clone()),
            )
    }

    fn render_edit_dev_container_name(
        &self,
        state: &EditDevContainerNameState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(connection) = RemoteSettings::get_global(cx)
            .dev_container_connections()
            .nth(state.index.0)
        else {
            return v_flex()
                .id("devcontainer-edit-name")
                .track_focus(&self.focus_handle(cx));
        };

        let connection_string = connection.name.clone();

        v_flex()
            .id("devcontainer-edit-name")
            .track_focus(&self.focus_handle(cx))
            .child(
                SshConnectionHeader {
                    connection_string: connection_string.into(),
                    paths: Default::default(),
                    nickname: None,
                    is_wsl: false,
                    is_devcontainer: true,
                }
                .render(window, cx),
            )
            .child(
                h_flex()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(state.editor.clone()),
            )
    }

    fn render_default(
        &mut self,
        mut state: DefaultState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ssh_settings = RemoteSettings::get_global(cx);
        let mut should_rebuild = false;

        let ssh_connections_changed = ssh_settings.ssh_connections.0.iter().ne(state
            .servers
            .iter()
            .filter_map(|server| match server {
                RemoteEntry::Project {
                    connection: Connection::Ssh(connection),
                    ..
                } => Some(connection),
                _ => None,
            }));

        let wsl_connections_changed = ssh_settings.wsl_connections.0.iter().ne(state
            .servers
            .iter()
            .filter_map(|server| match server {
                RemoteEntry::Project {
                    connection: Connection::Wsl(connection),
                    ..
                } => Some(connection),
                _ => None,
            }));

        let dev_container_connections_changed =
            ssh_settings
                .dev_container_connections
                .0
                .iter()
                .ne(state.servers.iter().filter_map(|server| match server {
                    RemoteEntry::Project {
                        connection: Connection::DevContainer(connection),
                        ..
                    } => Some(connection),
                    _ => None,
                }));

        if ssh_connections_changed || wsl_connections_changed || dev_container_connections_changed {
            should_rebuild = true;
        };

        if !should_rebuild && ssh_settings.read_ssh_config {
            let current_ssh_hosts: BTreeSet<SharedString> = state
                .servers
                .iter()
                .filter_map(|server| match server {
                    RemoteEntry::SshConfig { host, .. } => Some(host.clone()),
                    _ => None,
                })
                .collect();
            let mut expected_ssh_hosts = self.ssh_config_servers.clone();
            for server in &state.servers {
                if let RemoteEntry::Project {
                    connection: Connection::Ssh(connection),
                    ..
                } = server
                {
                    expected_ssh_hosts.remove(connection.host.as_str());
                }
            }
            should_rebuild = current_ssh_hosts != expected_ssh_hosts;
        }

        if should_rebuild {
            self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
            if let Mode::Default(new_state) = &self.mode {
                state = new_state.clone();
            }
        }
        if self.selected_entry.is_some()
            && !state.servers.iter().any(|entry| {
                self.selected_entry
                    .as_ref()
                    .is_some_and(|key| key.matches(entry))
            })
        {
            self.selected_entry = None;
        }

        let connect_button = div()
            .id("ssh-connect-new-server-container")
            .track_focus(&state.add_new_server.focus_handle)
            .anchor_scroll(state.add_new_server.scroll_anchor.clone())
            .child(
                ListItem::new("register-remote-server-button")
                    .toggle_state(
                        state
                            .add_new_server
                            .focus_handle
                            .contains_focused(window, cx),
                    )
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                    .child(Label::new("Connect SSH Server"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let state = this.new_create_remote_server_state(window, cx);
                        this.mode = Mode::CreateRemoteServer(state);

                        cx.notify();
                    })),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                let state = this.new_create_remote_server_state(window, cx);
                this.mode = Mode::CreateRemoteServer(state);

                cx.notify();
            }));

        let has_open_project = self
            .workspace
            .upgrade()
            .map(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .is_some()
            })
            .unwrap_or(false);
        let is_local = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().read(cx).is_local())
            .unwrap_or(true);
        let dev_container_disabled = !has_open_project || !is_local;

        let connect_dev_container_button = div()
            .id("connect-new-dev-container")
            .track_focus(&state.add_new_devcontainer.focus_handle)
            .anchor_scroll(state.add_new_devcontainer.scroll_anchor.clone())
            .child(
                ListItem::new("register-dev-container-button")
                    .toggle_state(
                        state
                            .add_new_devcontainer
                            .focus_handle
                            .contains_focused(window, cx),
                    )
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                    .child(Label::new("Connect Dev Container"))
                    .when(dev_container_disabled, |this| {
                        this.tooltip(Tooltip::text(
                            "Open a project to create a dev container.",
                        ))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if dev_container_disabled {
                            let confirmation = window.prompt(
                                PromptLevel::Info,
                                "Open a project to create a dev container",
                                Some("Dev containers are created from the active project's .devcontainer/devcontainer.json."),
                                &["Open Folder...", "Cancel"],
                                cx,
                            );
                            let remote_servers = cx.entity();
                            cx.spawn_in(window, async move |_, cx| {
                                if confirmation.await.ok() == Some(0) {
                                    remote_servers
                                        .update_in(cx, |_, window, cx| {
                                            window.dispatch_action(
                                                Box::new(workspace::Open),
                                                cx,
                                            );
                                        })
                                        .ok();
                                }
                                anyhow::Ok(())
                            })
                            .detach_and_log_err(cx);
                            return;
                        }
                        let state = CreateRemoteDevContainer::new(window, cx);
                        this.mode = Mode::CreateRemoteDevContainer(state);

                        cx.notify();
                    })),
            )
            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                if dev_container_disabled {
                    let confirmation = window.prompt(
                        PromptLevel::Info,
                        "Open a project to create a dev container",
                        Some("Dev containers are created from the active project's .devcontainer/devcontainer.json."),
                        &["Open Folder...", "Cancel"],
                        cx,
                    );
                    let remote_servers = cx.entity();
                    cx.spawn_in(window, async move |_, cx| {
                        if confirmation.await.ok() == Some(0) {
                            remote_servers
                                .update_in(cx, |_, window, cx| {
                                    window.dispatch_action(Box::new(workspace::Open), cx);
                                })
                                .ok();
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                    return;
                }
                let state = CreateRemoteDevContainer::new(window, cx);
                this.mode = Mode::CreateRemoteDevContainer(state);

                cx.notify();
            }));

        let refresh_dev_container_button = div()
            .id("refresh-dev-containers")
            .track_focus(&state.refresh_devcontainer.focus_handle)
            .anchor_scroll(state.refresh_devcontainer.scroll_anchor.clone())
            .child(
                ListItem::new("refresh-dev-containers-button")
                    .toggle_state(
                        state
                            .refresh_devcontainer
                            .focus_handle
                            .contains_focused(window, cx),
                    )
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(Icon::new(IconName::RotateCw).color(Color::Muted))
                    .child(Label::new("Refresh Dev Containers"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.refresh_dev_container_connections(window, cx);
                    })),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.refresh_dev_container_connections(window, cx);
            }));

        #[cfg(target_os = "windows")]
        let wsl_connect_button = div()
            .id("wsl-connect-new-server")
            .track_focus(&state.add_new_wsl.focus_handle)
            .anchor_scroll(state.add_new_wsl.scroll_anchor.clone())
            .child(
                ListItem::new("wsl-add-new-server")
                    .toggle_state(state.add_new_wsl.focus_handle.contains_focused(window, cx))
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                    .child(Label::new("Add WSL Distro"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let state = AddWslDistro::new(window, cx);
                        this.mode = Mode::AddWslDistro(state);

                        cx.notify();
                    })),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                let state = AddWslDistro::new(window, cx);
                this.mode = Mode::AddWslDistro(state);

                cx.notify();
            }));

        let selected_tab = self.selected_tab;
        let scroll_handle = state.scroll_handle.clone();
        let ssh_query = self.normalized_search_query(RemoteProjectsTab::Ssh, cx);
        let dev_container_query =
            self.normalized_search_query(RemoteProjectsTab::DevContainers, cx);
        #[cfg(target_os = "windows")]
        let wsl_query = self.normalized_search_query(RemoteProjectsTab::Wsl, cx);
        let ssh_count = state
            .servers
            .iter()
            .filter(|server| server.matches_tab(RemoteProjectsTab::Ssh))
            .count();
        let dev_container_count = state
            .servers
            .iter()
            .filter(|server| server.matches_tab(RemoteProjectsTab::DevContainers))
            .count();
        #[cfg(target_os = "windows")]
        let wsl_count = state
            .servers
            .iter()
            .filter(|server| server.matches_tab(RemoteProjectsTab::Wsl))
            .count();
        let first_ssh_key = state
            .servers
            .iter()
            .find(|server| {
                server.matches_tab(RemoteProjectsTab::Ssh)
                    && self.matches_search_query(server, &ssh_query)
            })
            .map(RemoteEntryKey::from_entry);
        let first_dev_container_key = state
            .servers
            .iter()
            .find(|server| {
                server.matches_tab(RemoteProjectsTab::DevContainers)
                    && self.matches_search_query(server, &dev_container_query)
            })
            .map(RemoteEntryKey::from_entry);
        #[cfg(target_os = "windows")]
        let first_wsl_key = state
            .servers
            .iter()
            .find(|server| {
                server.matches_tab(RemoteProjectsTab::Wsl)
                    && self.matches_search_query(server, &wsl_query)
            })
            .map(RemoteEntryKey::from_entry);

        let tabs = {
            #[cfg(target_os = "windows")]
            let tabs = ToggleButtonGroup::single_row(
                "remote-project-tabs",
                [
                    ToggleButtonSimple::new(format!("SSH ({ssh_count})"), {
                        let scroll_handle = scroll_handle.clone();
                        let first_ssh_key = first_ssh_key.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            this.selected_tab = RemoteProjectsTab::Ssh;
                            this.ssh_page = 0;
                            this.selected_entry = first_ssh_key.clone();
                            scroll_handle.scroll_to_top_of_item(0);
                            cx.notify();
                        })
                    }),
                    ToggleButtonSimple::new(format!("WSL ({wsl_count})"), {
                        let scroll_handle = scroll_handle.clone();
                        let first_wsl_key = first_wsl_key.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            this.selected_tab = RemoteProjectsTab::Wsl;
                            this.wsl_page = 0;
                            this.selected_entry = first_wsl_key.clone();
                            scroll_handle.scroll_to_top_of_item(0);
                            cx.notify();
                        })
                    }),
                    ToggleButtonSimple::new(format!("Dev Containers ({dev_container_count})"), {
                        let scroll_handle = scroll_handle.clone();
                        let first_dev_container_key = first_dev_container_key.clone();
                        cx.listener(move |this, _event, window, cx| {
                            this.selected_tab = RemoteProjectsTab::DevContainers;
                            this.dev_container_page = 0;
                            this.selected_entry = first_dev_container_key.clone();
                            scroll_handle.scroll_to_top_of_item(0);
                            this.refresh_dev_container_connections_silent(window, cx);
                            cx.notify();
                        })
                    }),
                ],
            )
            .style(ToggleButtonGroupStyle::Outlined)
            .label_size(LabelSize::Small)
            .auto_width()
            .selected_index(match selected_tab {
                RemoteProjectsTab::Ssh => 0,
                RemoteProjectsTab::Wsl => 1,
                RemoteProjectsTab::DevContainers => 2,
            });

            #[cfg(not(target_os = "windows"))]
            let tabs = ToggleButtonGroup::single_row(
                "remote-project-tabs",
                [
                    ToggleButtonSimple::new(format!("SSH ({ssh_count})"), {
                        let scroll_handle = scroll_handle.clone();
                        let first_ssh_key = first_ssh_key.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            this.selected_tab = RemoteProjectsTab::Ssh;
                            this.ssh_page = 0;
                            this.selected_entry = first_ssh_key.clone();
                            scroll_handle.scroll_to_top_of_item(0);
                            cx.notify();
                        })
                    }),
                    ToggleButtonSimple::new(format!("Dev Containers ({dev_container_count})"), {
                        let scroll_handle = scroll_handle.clone();
                        let first_dev_container_key = first_dev_container_key.clone();
                        cx.listener(move |this, _event, window, cx| {
                            this.selected_tab = RemoteProjectsTab::DevContainers;
                            this.dev_container_page = 0;
                            this.selected_entry = first_dev_container_key.clone();
                            scroll_handle.scroll_to_top_of_item(0);
                            this.refresh_dev_container_connections_silent(window, cx);
                            cx.notify();
                        })
                    }),
                ],
            )
            .style(ToggleButtonGroupStyle::Outlined)
            .label_size(LabelSize::Small)
            .auto_width()
            .selected_index(match selected_tab {
                RemoteProjectsTab::Ssh => 0,
                RemoteProjectsTab::DevContainers => 1,
                RemoteProjectsTab::Wsl => 0,
            });

            tabs
        };

        let selected_query = match selected_tab {
            RemoteProjectsTab::Ssh => ssh_query.as_str(),
            RemoteProjectsTab::DevContainers => dev_container_query.as_str(),
            RemoteProjectsTab::Wsl => {
                #[cfg(target_os = "windows")]
                {
                    wsl_query.as_str()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    ""
                }
            }
        };
        let has_search_query = !selected_query.is_empty();

        let visible_servers: Vec<(usize, RemoteEntry)> = state
            .servers
            .iter()
            .enumerate()
            .filter(|(_, server)| {
                server.matches_tab(selected_tab)
                    && self.matches_search_query(server, selected_query)
            })
            .map(|(ix, server)| (ix, server.clone()))
            .collect();

        let total_pages = if visible_servers.is_empty() {
            0
        } else {
            (visible_servers.len() + REMOTE_SERVERS_PAGE_SIZE - 1) / REMOTE_SERVERS_PAGE_SIZE
        };
        let stored_page = match selected_tab {
            RemoteProjectsTab::Ssh => self.ssh_page,
            RemoteProjectsTab::DevContainers => self.dev_container_page,
            RemoteProjectsTab::Wsl => self.wsl_page,
        };
        let mut current_page = stored_page.min(total_pages.saturating_sub(1));
        if let Some(selected_key) = self.selected_entry.as_ref() {
            if let Some(selected_pos) = visible_servers
                .iter()
                .position(|(_, entry)| selected_key.matches(entry))
            {
                current_page = selected_pos / REMOTE_SERVERS_PAGE_SIZE;
            }
        }
        if current_page != stored_page {
            match selected_tab {
                RemoteProjectsTab::Ssh => self.ssh_page = current_page,
                RemoteProjectsTab::DevContainers => self.dev_container_page = current_page,
                RemoteProjectsTab::Wsl => self.wsl_page = current_page,
            }
        }

        let paged_servers: Vec<(usize, RemoteEntry)> = visible_servers
            .iter()
            .skip(current_page * REMOTE_SERVERS_PAGE_SIZE)
            .take(REMOTE_SERVERS_PAGE_SIZE)
            .cloned()
            .collect();

        let selected_entry = self
            .selected_entry
            .as_ref()
            .and_then(|key| {
                paged_servers
                    .iter()
                    .find(|(_, entry)| key.matches(entry))
                    .cloned()
            })
            .or_else(|| paged_servers.first().cloned());

        let selected_key = selected_entry
            .as_ref()
            .map(|(_, entry)| RemoteEntryKey::from_entry(entry));
        if let Some(new_key) = selected_key.clone() {
            if self.selected_entry.as_ref() != Some(&new_key) {
                self.selected_entry = Some(new_key);
            }
        } else if self.selected_entry.is_some() {
            self.selected_entry = None;
        }

        let page_first_key = |page: usize| {
            visible_servers
                .iter()
                .skip(page * REMOTE_SERVERS_PAGE_SIZE)
                .next()
                .map(|(_, entry)| RemoteEntryKey::from_entry(entry))
        };
        let prev_page_key = (current_page > 0)
            .then(|| page_first_key(current_page - 1))
            .flatten();
        let next_page_key = (current_page + 1 < total_pages)
            .then(|| page_first_key(current_page + 1))
            .flatten();

        let empty_message = if has_search_query {
            "No servers match your search."
        } else {
            selected_tab.empty_message()
        };
        let left_list = List::new()
            .empty_message(
                h_flex()
                    .size_full()
                    .p_2()
                    .justify_center()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(empty_message).color(Color::Muted))
                    .into_any_element(),
            )
            .children(paged_servers.iter().map(|(ix, connection)| {
                self.render_remote_server_row(
                    *ix,
                    connection.clone(),
                    selected_key.as_ref(),
                    window,
                    cx,
                )
                .into_any_element()
            }));

        let mut left_column = v_flex().gap_1();
        left_column = match selected_tab {
            RemoteProjectsTab::Ssh => left_column.child(connect_button),
            RemoteProjectsTab::DevContainers => left_column
                .child(connect_dev_container_button)
                .child(refresh_dev_container_button),
            RemoteProjectsTab::Wsl => {
                #[cfg(target_os = "windows")]
                {
                    left_column.child(wsl_connect_button)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    left_column
                }
            }
        };
        left_column = left_column.child(div().px_2().child(self.render_search_bar(
            selected_tab,
            has_search_query,
            window,
            cx,
        )));

        if total_pages > 1 {
            let scroll_handle_prev = scroll_handle.clone();
            let scroll_handle_next = scroll_handle.clone();
            let prev_page_key = prev_page_key.clone();
            let next_page_key = next_page_key.clone();
            left_column = left_column.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .child(
                        IconButton::new("remote-servers-page-prev", IconName::ChevronLeft)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .shape(IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .disabled(current_page == 0)
                            .tooltip(Tooltip::text("Previous page"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let next_page = current_page.saturating_sub(1);
                                match selected_tab {
                                    RemoteProjectsTab::Ssh => this.ssh_page = next_page,
                                    RemoteProjectsTab::DevContainers => {
                                        this.dev_container_page = next_page
                                    }
                                    RemoteProjectsTab::Wsl => this.wsl_page = next_page,
                                }
                                this.selected_entry = prev_page_key.clone();
                                scroll_handle_prev.scroll_to_top_of_item(0);
                                cx.notify();
                            })),
                    )
                    .child(
                        Label::new(format!("Page {} of {}", current_page + 1, total_pages))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("remote-servers-page-next", IconName::ChevronRight)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .shape(IconButtonShape::Square)
                            .size(ButtonSize::Compact)
                            .disabled(current_page + 1 >= total_pages)
                            .tooltip(Tooltip::text("Next page"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let next_page = current_page.saturating_add(1);
                                match selected_tab {
                                    RemoteProjectsTab::Ssh => this.ssh_page = next_page,
                                    RemoteProjectsTab::DevContainers => {
                                        this.dev_container_page = next_page
                                    }
                                    RemoteProjectsTab::Wsl => this.wsl_page = next_page,
                                }
                                this.selected_entry = next_page_key.clone();
                                scroll_handle_next.scroll_to_top_of_item(0);
                                cx.notify();
                            })),
                    ),
            );
        }

        left_column = left_column.child(left_list);

        let right_column = if let Some((selected_ix, selected_entry)) = selected_entry.as_ref() {
            self.render_remote_details(*selected_ix, selected_entry.clone(), window, cx)
                .into_any_element()
        } else {
            v_flex()
                .p_3()
                .child(Label::new("Select a server to view projects.").color(Color::Muted))
                .into_any_element()
        };

        let columns = h_flex()
            .items_start()
            .child(
                v_flex()
                    .w(rems(14.))
                    .flex_none()
                    .border_r_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(left_column),
            )
            .child(v_flex().flex_1().child(right_column));

        let modal_section = v_flex()
            .track_focus(&self.focus_handle(cx))
            .id("ssh-server-list")
            .overflow_y_scroll()
            .track_scroll(&state.scroll_handle)
            .size_full()
            .child(columns);

        let mut modal_section = Navigable::new(modal_section.into_any_element());

        if selected_tab == RemoteProjectsTab::Ssh {
            modal_section = modal_section.entry(state.add_new_server.clone());
        }

        if selected_tab == RemoteProjectsTab::DevContainers {
            modal_section = modal_section.entry(state.refresh_devcontainer.clone());
            if has_open_project && is_local {
                modal_section = modal_section.entry(state.add_new_devcontainer.clone());
            }
        }

        if selected_tab == RemoteProjectsTab::Wsl && cfg!(target_os = "windows") {
            modal_section = modal_section.entry(state.add_new_wsl.clone());
        }

        for (_, server) in &paged_servers {
            modal_section = modal_section.entry(server.select_entry().clone());
        }

        if let Some((_, server)) = selected_entry.as_ref() {
            match server {
                RemoteEntry::Project {
                    open_folder,
                    projects,
                    configure,
                    ..
                } => {
                    for (navigation_state, _) in projects {
                        modal_section = modal_section.entry(navigation_state.clone());
                    }
                    modal_section = modal_section.entry(open_folder.clone());
                    if server.can_configure() {
                        modal_section = modal_section.entry(configure.clone());
                    }
                }
                RemoteEntry::SshConfig { open_folder, .. } => {
                    modal_section = modal_section.entry(open_folder.clone());
                }
            }
        }
        let mut modal_section = modal_section.render(window, cx).into_any_element();

        let (create_window, reuse_window) = if self.create_new_window {
            (
                window.keystroke_text_for(&menu::Confirm),
                window.keystroke_text_for(&menu::SecondaryConfirm),
            )
        } else {
            (
                window.keystroke_text_for(&menu::SecondaryConfirm),
                window.keystroke_text_for(&menu::Confirm),
            )
        };
        let placeholder_text = Arc::from(format!(
            "{reuse_window} reuses this window, {create_window} opens a new one",
        ));

        Modal::new("remote-projects", None)
            .header(
                ModalHeader::new()
                    .child(Headline::new("Remote Projects").size(HeadlineSize::XSmall))
                    .child(
                        Label::new(placeholder_text)
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    ),
            )
            .section(
                Section::new().padded(false).child(
                    v_flex()
                        .min_h(rems(20.))
                        .size_full()
                        .relative()
                        .child(ListSeparator)
                        .child(
                            h_flex()
                                .px_2()
                                .py_1()
                                .border_b_1()
                                .border_color(cx.theme().colors().border_variant)
                                .child(tabs),
                        )
                        .child(
                            canvas(
                                |bounds, window, cx| {
                                    modal_section.prepaint_as_root(
                                        bounds.origin,
                                        bounds.size.into(),
                                        window,
                                        cx,
                                    );
                                    modal_section
                                },
                                |_, mut modal_section, window, cx| {
                                    modal_section.paint(window, cx);
                                },
                            )
                            .size_full(),
                        )
                        .vertical_scrollbar_for(&state.scroll_handle, window, cx),
                ),
            )
            .into_any_element()
    }

    fn create_host_from_ssh_config(
        &mut self,
        ssh_config_host: &SharedString,
        cx: &mut Context<'_, Self>,
    ) -> SshServerIndex {
        let new_ix = Arc::new(AtomicUsize::new(0));

        let update_new_ix = new_ix.clone();
        self.update_settings_file(cx, move |settings, _| {
            update_new_ix.store(
                settings
                    .ssh_connections
                    .as_ref()
                    .map_or(0, |connections| connections.len()),
                atomic::Ordering::Release,
            );
        });

        self.add_ssh_server(
            SshConnectionOptions {
                host: ssh_config_host.to_string().into(),
                ..SshConnectionOptions::default()
            },
            cx,
        );
        self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
        SshServerIndex(new_ix.load(atomic::Ordering::Acquire))
    }
}

fn spawn_ssh_config_watch(fs: Arc<dyn Fs>, cx: &Context<RemoteServerProjects>) -> Task<()> {
    let (mut user_ssh_config_watcher, user_watcher_task) =
        watch_config_file(cx.background_executor(), fs.clone(), user_ssh_config_file());
    let (mut global_ssh_config_watcher, global_watcher_task) = global_ssh_config_file()
        .map(|it| watch_config_file(cx.background_executor(), fs, it.to_owned()))
        .unwrap_or_else(|| (futures::channel::mpsc::unbounded().1, gpui::Task::ready(())));

    cx.spawn(async move |remote_server_projects, cx| {
        let _user_watcher_task = user_watcher_task;
        let _global_watcher_task = global_watcher_task;
        let mut global_hosts = BTreeSet::default();
        let mut user_hosts = BTreeSet::default();
        let mut global_entries: HashMap<String, SshConfigEntry> = HashMap::new();
        let mut user_entries: HashMap<String, SshConfigEntry> = HashMap::new();
        let mut running_receivers = 2;

        loop {
            select! {
                new_global_file_contents = global_ssh_config_watcher.next().fuse() => {
                    match new_global_file_contents {
                        Some(new_global_file_contents) => {
                            global_hosts = parse_ssh_config_hosts(&new_global_file_contents);
                            global_entries = parse_ssh_config_entries(&new_global_file_contents);
                            if remote_server_projects.update(cx, |remote_server_projects, cx| {
                                let mut merged_entries = global_entries.clone();
                                for (host, entry) in &user_entries {
                                    merged_entries.insert(host.clone(), entry.clone());
                                }
                                remote_server_projects.ssh_config_servers = global_hosts.iter().chain(user_hosts.iter()).map(SharedString::from).collect();
                                remote_server_projects.ssh_config_entries = merged_entries;
                                cx.notify();
                            }).is_err() {
                                return;
                            }
                        },
                        None => {
                            running_receivers -= 1;
                            if running_receivers == 0 {
                                return;
                            }
                        }
                    }
                },
                new_user_file_contents = user_ssh_config_watcher.next().fuse() => {
                    match new_user_file_contents {
                        Some(new_user_file_contents) => {
                            user_hosts = parse_ssh_config_hosts(&new_user_file_contents);
                            user_entries = parse_ssh_config_entries(&new_user_file_contents);
                            if remote_server_projects.update(cx, |remote_server_projects, cx| {
                                let mut merged_entries = global_entries.clone();
                                for (host, entry) in &user_entries {
                                    merged_entries.insert(host.clone(), entry.clone());
                                }
                                remote_server_projects.ssh_config_servers = global_hosts.iter().chain(user_hosts.iter()).map(SharedString::from).collect();
                                remote_server_projects.ssh_config_entries = merged_entries;
                                cx.notify();
                            }).is_err() {
                                return;
                            }
                        },
                        None => {
                            running_receivers -= 1;
                            if running_receivers == 0 {
                                return;
                            }
                        }
                    }
                },
            }
        }
    })
}

fn get_text(element: &Entity<Editor>, cx: &mut App) -> String {
    element.read(cx).text(cx).trim().to_string()
}

fn split_user_host(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (user, host) = value.rsplit_once('@')?;
    if user.is_empty() || host.is_empty() {
        return None;
    }
    Some((user.to_string(), host.to_string()))
}

fn parse_port_forwards(input: &str) -> (Vec<SshPortForwardOption>, Vec<SharedString>) {
    let mut forwards = Vec::new();
    let mut errors = Vec::new();
    for (index, raw) in input
        .split(|c| c == ',' || c == '\n' || c == ';')
        .enumerate()
    {
        let spec = raw.trim();
        if spec.is_empty() {
            continue;
        }
        let spec = spec.strip_prefix("-L").unwrap_or(spec).trim();
        match parse_port_forward_spec(spec) {
            Ok(option) => forwards.push(option),
            Err(err) => {
                errors.push(format!("Port forward {}: {err}", index + 1).into());
            }
        }
    }
    (forwards, errors)
}

impl ModalView for RemoteServerProjects {}

impl Focusable for RemoteServerProjects {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            Mode::ProjectPicker(picker) => picker.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for RemoteServerProjects {}

impl Render for RemoteServerProjects {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .elevation_3(cx)
            .w(rems(34.))
            .key_context("RemoteServerModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if matches!(this.mode, Mode::Default(_)) {
                    cx.emit(DismissEvent)
                }
            }))
            .child(match &self.mode {
                Mode::Default(state) => self
                    .render_default(state.clone(), window, cx)
                    .into_any_element(),
                Mode::ViewServerOptions(state) => self
                    .render_view_options(state.clone(), window, cx)
                    .into_any_element(),
                Mode::ProjectPicker(element) => element.clone().into_any_element(),
                Mode::CreateRemoteServer(state) => {
                    let snapshot = state.snapshot();
                    self.render_create_remote_server(snapshot, window, cx)
                        .into_any_element()
                }
                Mode::CreateRemoteDevContainer(state) => self
                    .render_create_dev_container(state, window, cx)
                    .into_any_element(),
                Mode::EditNickname(state) => self
                    .render_edit_nickname(state, window, cx)
                    .into_any_element(),
                Mode::EditDevContainerName(state) => self
                    .render_edit_dev_container_name(state, window, cx)
                    .into_any_element(),
                #[cfg(target_os = "windows")]
                Mode::AddWslDistro(state) => self
                    .render_add_wsl_distro(state, window, cx)
                    .into_any_element(),
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevContainerProbe {
    Exists,
    Missing,
    Unknown,
}

async fn probe_dev_container(connection: &DevContainerConnection) -> DevContainerProbe {
    let docker_bin = if connection.use_podman {
        "podman"
    } else {
        "docker"
    };
    let args = vec!["inspect".to_string(), connection.container_id.clone()];

    let output = match &connection.host {
        None => new_smol_command(docker_bin)
            .args(&args)
            .output()
            .await
            .map_err(|err| {
                log::warn!("Failed to run {} locally: {err}", docker_bin);
                err
            }),
        Some(DevContainerHost::Wsl { distro_name, user }) => {
            let cmd = build_shell_command(docker_bin, &args);
            let mut command = new_smol_command("wsl.exe");
            command.arg("--distribution");
            command.arg(distro_name);
            if let Some(user) = user {
                command.arg("--user");
                command.arg(user);
            }
            command.arg("--");
            command.arg("sh");
            command.arg("-lc");
            command.arg(wrap_bashrc(&cmd));
            command.output().await.map_err(|err| {
                log::warn!("Failed to run {} in WSL: {err}", docker_bin);
                err
            })
        }
        Some(DevContainerHost::Ssh {
            host,
            username,
            port,
            args: ssh_args,
        }) => {
            let options = SshConnectionOptions {
                host: host.clone().into(),
                username: username.clone(),
                port: *port,
                args: Some(ssh_args.clone()),
                ..SshConnectionOptions::default()
            };
            let mut args_list = options.additional_args();
            args_list.push("-q".to_string());
            args_list.push("-T".to_string());
            args_list.push(options.ssh_destination());

            let cmd = build_shell_command(docker_bin, &args);
            let shell_kind = ShellKind::Posix;
            let wrapped_script = wrap_bashrc(&cmd);
            let wrapped_cmd = shell_kind
                .try_quote(&wrapped_script)
                .unwrap_or_else(|| Cow::Owned(cmd.clone()));
            args_list.push(format!("sh -lc {}", wrapped_cmd));

            new_smol_command("ssh")
                .args(args_list)
                .output()
                .await
                .map_err(|err| {
                    log::warn!("Failed to run {} over SSH: {err}", docker_bin);
                    err
                })
        }
    };

    match output {
        Ok(output) if output.status.success() => DevContainerProbe::Exists,
        Ok(output) => {
            let message = output_message(&output);
            let message = message.to_lowercase();
            if is_docker_unavailable_output(&message) {
                DevContainerProbe::Unknown
            } else if is_missing_container_output(&message, &connection.container_id) {
                DevContainerProbe::Missing
            } else {
                DevContainerProbe::Unknown
            }
        }
        Err(_) => DevContainerProbe::Unknown,
    }
}

async fn stop_dev_container_container(connection: &DevContainerConnection) -> Result<(), String> {
    let docker_bin = if connection.use_podman {
        "podman"
    } else {
        "docker"
    };
    let args = vec!["stop".to_string(), connection.container_id.clone()];

    let output = match &connection.host {
        None => new_smol_command(docker_bin)
            .args(&args)
            .output()
            .await
            .map_err(|err| format!("Failed to run {docker_bin} locally: {err}"))?,
        Some(DevContainerHost::Wsl { distro_name, user }) => {
            let cmd = build_shell_command(docker_bin, &args);
            let mut command = new_smol_command("wsl.exe");
            command.arg("--distribution");
            command.arg(distro_name);
            if let Some(user) = user {
                command.arg("--user");
                command.arg(user);
            }
            command.arg("--");
            command.arg("sh");
            command.arg("-lc");
            command.arg(wrap_bashrc(&cmd));
            command
                .output()
                .await
                .map_err(|err| format!("Failed to run {docker_bin} in WSL: {err}"))?
        }
        Some(DevContainerHost::Ssh {
            host,
            username,
            port,
            args: ssh_args,
        }) => {
            let options = SshConnectionOptions {
                host: host.clone().into(),
                username: username.clone(),
                port: *port,
                args: Some(ssh_args.clone()),
                ..SshConnectionOptions::default()
            };
            let mut args_list = options.additional_args();
            args_list.push("-q".to_string());
            args_list.push("-T".to_string());
            args_list.push(options.ssh_destination());

            let cmd = build_shell_command(docker_bin, &args);
            let shell_kind = ShellKind::Posix;
            let wrapped_script = wrap_bashrc(&cmd);
            let wrapped_cmd = shell_kind
                .try_quote(&wrapped_script)
                .unwrap_or_else(|| Cow::Owned(cmd.clone()));
            args_list.push(format!("sh -lc {}", wrapped_cmd));

            new_smol_command("ssh")
                .args(args_list)
                .output()
                .await
                .map_err(|err| format!("Failed to run {docker_bin} over SSH: {err}"))?
        }
    };

    if output.status.success() {
        Ok(())
    } else {
        let mut message = String::new();
        message.push_str(&String::from_utf8_lossy(&output.stderr));
        message.push_str(&String::from_utf8_lossy(&output.stdout));
        let message = message.trim();
        if message.is_empty() {
            Err(format!(
                "{docker_bin} stop failed with exit code {}",
                output.status
            ))
        } else {
            Err(message.to_string())
        }
    }
}

async fn remove_dev_container_container(connection: &DevContainerConnection) -> Result<(), String> {
    let docker_bin = if connection.use_podman {
        "podman"
    } else {
        "docker"
    };
    let args = vec![
        "rm".to_string(),
        "-f".to_string(),
        connection.container_id.clone(),
    ];

    let output = match &connection.host {
        None => new_smol_command(docker_bin)
            .args(&args)
            .output()
            .await
            .map_err(|err| format!("Failed to run {docker_bin} locally: {err}"))?,
        Some(DevContainerHost::Wsl { distro_name, user }) => {
            let cmd = build_shell_command(docker_bin, &args);
            let mut command = new_smol_command("wsl.exe");
            command.arg("--distribution");
            command.arg(distro_name);
            if let Some(user) = user {
                command.arg("--user");
                command.arg(user);
            }
            command.arg("--");
            command.arg("sh");
            command.arg("-lc");
            command.arg(wrap_bashrc(&cmd));
            command
                .output()
                .await
                .map_err(|err| format!("Failed to run {docker_bin} in WSL: {err}"))?
        }
        Some(DevContainerHost::Ssh {
            host,
            username,
            port,
            args: ssh_args,
        }) => {
            let options = SshConnectionOptions {
                host: host.clone().into(),
                username: username.clone(),
                port: *port,
                args: Some(ssh_args.clone()),
                ..SshConnectionOptions::default()
            };
            let mut args_list = options.additional_args();
            args_list.push("-q".to_string());
            args_list.push("-T".to_string());
            args_list.push(options.ssh_destination());

            let cmd = build_shell_command(docker_bin, &args);
            let shell_kind = ShellKind::Posix;
            let wrapped_script = wrap_bashrc(&cmd);
            let wrapped_cmd = shell_kind
                .try_quote(&wrapped_script)
                .unwrap_or_else(|| Cow::Owned(cmd.clone()));
            args_list.push(format!("sh -lc {}", wrapped_cmd));

            new_smol_command("ssh")
                .args(args_list)
                .output()
                .await
                .map_err(|err| format!("Failed to run {docker_bin} over SSH: {err}"))?
        }
    };

    if output.status.success() {
        Ok(())
    } else {
        let mut message = String::new();
        message.push_str(&String::from_utf8_lossy(&output.stderr));
        message.push_str(&String::from_utf8_lossy(&output.stdout));
        let message = message.trim();
        if message.is_empty() {
            Err(format!(
                "{docker_bin} rm failed with exit code {}",
                output.status
            ))
        } else {
            Err(message.to_string())
        }
    }
}

fn build_shell_command(program: &str, args: &[String]) -> String {
    let shell_kind = ShellKind::Posix;
    let program = shell_kind
        .try_quote_prefix_aware(program)
        .unwrap_or_else(|| Cow::Owned(program.to_string()));
    let mut command = String::new();
    use std::fmt::Write as _;
    let _ = write!(command, "{program}");
    for arg in args {
        let quoted = shell_kind
            .try_quote(arg)
            .unwrap_or_else(|| Cow::Owned(arg.clone()));
        let _ = write!(command, " {quoted}");
    }
    command
}

fn wrap_bashrc(cmd: &str) -> String {
    format!(
        "if [ -f ~/.bash_profile ]; then . ~/.bash_profile >/dev/null 2>&1; fi; \
if [ -f ~/.profile ]; then . ~/.profile >/dev/null 2>&1; fi; \
if [ -f ~/.bashrc ]; then . ~/.bashrc >/dev/null 2>&1; fi; \
if [ -f ~/.zprofile ]; then . ~/.zprofile >/dev/null 2>&1; fi; \
{cmd}"
    )
}

fn output_message(output: &std::process::Output) -> String {
    let mut message = String::new();
    message.push_str(&String::from_utf8_lossy(&output.stderr));
    message.push_str(&String::from_utf8_lossy(&output.stdout));
    message
}

fn is_missing_container_output(message: &str, container_id: &str) -> bool {
    let short_id = container_id.get(..12).unwrap_or(container_id);
    let mentions_container = message.contains(container_id)
        || message.contains(short_id)
        || message.contains("container");
    let missing_marker = message.contains("no such")
        || message.contains("not found")
        || message.contains("does not exist")
        || message.contains("no container")
        || message.contains("no such object");
    missing_marker && mentions_container
}

fn is_docker_unavailable_output(message: &str) -> bool {
    message.contains("cannot connect")
        || message.contains("failed to connect")
        || message.contains("connection refused")
        || message.contains("docker daemon")
        || message.contains("podman socket")
        || message.contains("permission denied")
        || message.contains("access denied")
        || message.contains("is the docker daemon running")
}
