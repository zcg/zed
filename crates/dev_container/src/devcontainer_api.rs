use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Display,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use futures::channel::mpsc::UnboundedSender;
use futures::io::AsyncBufReadExt;
use gpui::AsyncWindowContext;
use node_runtime::NodeRuntime;
use remote::{DockerConnectionOptions, DockerHost, RemoteConnectionOptions};
use serde::Deserialize;
use settings::{DevContainerConnection, DevContainerHost, Settings as _};
use smol::{fs, io::BufReader, process::Command};
use util::shell::ShellKind;
use workspace::Workspace;

use crate::{DevContainerFeature, DevContainerSettings, DevContainerTemplate};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevContainerUp {
    _outcome: String,
    container_id: String,
    _remote_user: String,
    remote_workspace_folder: String,
}

#[derive(Debug, Deserialize)]
struct DockerMount {
    #[serde(rename = "Destination")]
    destination: String,
    #[serde(rename = "Source")]
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevContainerApply {
    pub(crate) files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevContainerConfiguration {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DevContainerConfigurationOutput {
    configuration: DevContainerConfiguration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevContainerError {
    DockerNotAvailable,
    DevContainerCliNotAvailable,
    DevContainerTemplateApplyFailed(String),
    DevContainerUpFailed(String),
    DevContainerNotFound,
    DevContainerParseFailed,
    NodeRuntimeNotAvailable,
    NotInValidProject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevContainerBuildStep {
    CheckDocker,
    CheckDevcontainerCli,
    DevcontainerUp,
    ReadConfiguration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevContainerLogStream {
    Stdout,
    Stderr,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevContainerLogLine {
    pub stream: DevContainerLogStream,
    pub line: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevContainerProgressEvent {
    StepStarted(DevContainerBuildStep),
    StepCompleted(DevContainerBuildStep),
    StepFailed(DevContainerBuildStep, String),
    LogLine(DevContainerLogLine),
}

impl Display for DevContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                DevContainerError::DockerNotAvailable =>
                    "Docker CLI not found on $PATH".to_string(),
                DevContainerError::DevContainerCliNotAvailable =>
                    "Dev Container CLI not available. Ensure @devcontainers/cli is installed and on PATH for login shells (e.g. ~/.profile or ~/.zprofile)".to_string(),
                DevContainerError::DevContainerUpFailed(message) => {
                    format!("DevContainer creation failed with error: {}", message)
                }
                DevContainerError::DevContainerTemplateApplyFailed(message) => {
                    format!("DevContainer template apply failed with error: {}", message)
                }
                DevContainerError::DevContainerNotFound =>
                    "No valid dev container definition found in project".to_string(),
                DevContainerError::DevContainerParseFailed =>
                    "Failed to parse file .devcontainer/devcontainer.json".to_string(),
                DevContainerError::NodeRuntimeNotAvailable =>
                    "Cannot find a valid node runtime".to_string(),
                DevContainerError::NotInValidProject => "Not within a valid project".to_string(),
            }
        )
    }
}

struct ProjectContext {
    directory: Arc<Path>,
    remote_options: Option<RemoteConnectionOptions>,
}

pub(crate) async fn read_devcontainer_configuration_for_project(
    cx: &mut AsyncWindowContext,
    node_runtime: &NodeRuntime,
) -> Result<DevContainerConfigurationOutput, DevContainerError> {
    let use_podman = use_podman(cx);
    let ProjectContext {
        directory,
        remote_options,
    } = resolve_project_context_for_devcontainer(cx).await?;

    if let Some(remote_options) = remote_options {
        ensure_devcontainer_cli_remote(&remote_options).await?;
        devcontainer_read_configuration_remote(&remote_options, &directory, use_podman).await
    } else {
        let (path_to_devcontainer_cli, found_in_path) =
            ensure_devcontainer_cli(&node_runtime).await?;
        devcontainer_read_configuration(
            &path_to_devcontainer_cli,
            found_in_path,
            node_runtime,
            &directory,
            use_podman,
        )
        .await
    }
}

pub(crate) async fn apply_dev_container_template(
    template: &DevContainerTemplate,
    options_selected: &HashMap<String, String>,
    features_selected: &HashSet<DevContainerFeature>,
    cx: &mut AsyncWindowContext,
    node_runtime: &NodeRuntime,
) -> Result<DevContainerApply, DevContainerError> {
    let ProjectContext {
        directory,
        remote_options,
    } = resolve_project_context_for_devcontainer(cx).await?;

    if let Some(remote_options) = remote_options {
        ensure_devcontainer_cli_remote(&remote_options).await?;
        devcontainer_template_apply_remote(
            template,
            options_selected,
            features_selected,
            &remote_options,
            &directory,
        )
        .await
    } else {
        let (path_to_devcontainer_cli, found_in_path) =
            ensure_devcontainer_cli(&node_runtime).await?;
        devcontainer_template_apply(
            template,
            options_selected,
            features_selected,
            &path_to_devcontainer_cli,
            found_in_path,
            node_runtime,
            &directory,
            false, // devcontainer template apply does not use --docker-path option
        )
        .await
    }
}

fn use_podman(cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| DevContainerSettings::get_global(cx).use_podman)
        .unwrap_or(false)
}

pub async fn start_dev_container(
    cx: &mut AsyncWindowContext,
    node_runtime: NodeRuntime,
) -> Result<(DevContainerConnection, String), DevContainerError> {
    start_dev_container_with_progress(cx, node_runtime, None).await
}

pub async fn start_dev_container_with_progress(
    cx: &mut AsyncWindowContext,
    node_runtime: NodeRuntime,
    progress_tx: Option<UnboundedSender<DevContainerProgressEvent>>,
) -> Result<(DevContainerConnection, String), DevContainerError> {
    let send_progress =
        |event: DevContainerProgressEvent,
         progress_tx: &Option<UnboundedSender<DevContainerProgressEvent>>| {
            if let Some(tx) = progress_tx {
                let _ = tx.unbounded_send(event);
            }
        };

    let log_info =
        |line: &str, progress_tx: &Option<UnboundedSender<DevContainerProgressEvent>>| {
            send_progress(
                DevContainerProgressEvent::LogLine(DevContainerLogLine {
                    stream: DevContainerLogStream::Info,
                    line: line.to_string(),
                }),
                progress_tx,
            );
        };

    let use_podman = use_podman(cx);
    let ProjectContext {
        directory,
        remote_options,
    } = resolve_project_context_for_devcontainer(cx).await?;

    if let Some(remote_options) = remote_options {
        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::CheckDocker),
            &progress_tx,
        );
        log_info("Checking Docker/Podman availability...", &progress_tx);
        if let Err(e) = check_for_docker_remote(&remote_options, use_podman).await {
            send_progress(
                DevContainerProgressEvent::StepFailed(
                    DevContainerBuildStep::CheckDocker,
                    e.to_string(),
                ),
                &progress_tx,
            );
            return Err(e);
        }
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::CheckDocker),
            &progress_tx,
        );

        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::CheckDevcontainerCli),
            &progress_tx,
        );
        log_info("Checking Dev Container CLI...", &progress_tx);
        if let Err(e) = ensure_devcontainer_cli_remote(&remote_options).await {
            send_progress(
                DevContainerProgressEvent::StepFailed(
                    DevContainerBuildStep::CheckDevcontainerCli,
                    e.to_string(),
                ),
                &progress_tx,
            );
            return Err(e);
        }
        send_progress(
            DevContainerProgressEvent::StepCompleted(
                DevContainerBuildStep::CheckDevcontainerCli,
            ),
            &progress_tx,
        );

        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::DevcontainerUp),
            &progress_tx,
        );
        log_info("Running devcontainer up...", &progress_tx);
        let DevContainerUp {
            container_id,
            remote_workspace_folder,
            ..
        } = match devcontainer_up_remote(
            &remote_options,
            &directory,
            use_podman,
            progress_tx.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                send_progress(
                    DevContainerProgressEvent::StepFailed(
                        DevContainerBuildStep::DevcontainerUp,
                        e.to_string(),
                    ),
                    &progress_tx,
                );
                return Err(e);
            }
        };
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::DevcontainerUp),
            &progress_tx,
        );

        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::ReadConfiguration),
            &progress_tx,
        );
        log_info("Reading devcontainer configuration...", &progress_tx);
        let project_name = match devcontainer_read_configuration_remote(
            &remote_options,
            &directory,
            use_podman,
        )
        .await
        {
            Ok(DevContainerConfigurationOutput {
                configuration:
                    DevContainerConfiguration {
                        name: Some(project_name),
                    },
            }) => project_name,
            _ => get_backup_project_name(&remote_workspace_folder, &container_id),
        };
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::ReadConfiguration),
            &progress_tx,
        );

        let connection = DevContainerConnection {
            name: project_name,
            container_id,
            use_podman,
            projects: BTreeSet::new(),
            host_projects: BTreeSet::new(),
            host: devcontainer_host_from_remote_options(&remote_options),
        };

        Ok((connection, remote_workspace_folder))
    } else {
        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::CheckDocker),
            &progress_tx,
        );
        log_info("Checking Docker/Podman availability...", &progress_tx);
        if let Err(e) = check_for_docker(use_podman).await {
            send_progress(
                DevContainerProgressEvent::StepFailed(
                    DevContainerBuildStep::CheckDocker,
                    e.to_string(),
                ),
                &progress_tx,
            );
            return Err(e);
        }
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::CheckDocker),
            &progress_tx,
        );

        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::CheckDevcontainerCli),
            &progress_tx,
        );
        log_info("Checking Dev Container CLI...", &progress_tx);
        let (path_to_devcontainer_cli, found_in_path) =
            match ensure_devcontainer_cli(&node_runtime).await {
                Ok(result) => result,
                Err(e) => {
                    send_progress(
                        DevContainerProgressEvent::StepFailed(
                            DevContainerBuildStep::CheckDevcontainerCli,
                            e.to_string(),
                        ),
                        &progress_tx,
                    );
                    return Err(e);
                }
            };
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::CheckDevcontainerCli),
            &progress_tx,
        );

        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::DevcontainerUp),
            &progress_tx,
        );
        log_info("Running devcontainer up...", &progress_tx);
        let DevContainerUp {
            container_id,
            remote_workspace_folder,
            ..
        } = match devcontainer_up(
            &path_to_devcontainer_cli,
            found_in_path,
            &node_runtime,
            directory.clone(),
            use_podman,
            progress_tx.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                send_progress(
                    DevContainerProgressEvent::StepFailed(
                        DevContainerBuildStep::DevcontainerUp,
                        e.to_string(),
                    ),
                    &progress_tx,
                );
                return Err(e);
            }
        };
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::DevcontainerUp),
            &progress_tx,
        );

        send_progress(
            DevContainerProgressEvent::StepStarted(DevContainerBuildStep::ReadConfiguration),
            &progress_tx,
        );
        log_info("Reading devcontainer configuration...", &progress_tx);
        let project_name = match devcontainer_read_configuration(
            &path_to_devcontainer_cli,
            found_in_path,
            &node_runtime,
            &directory,
            use_podman,
        )
        .await
        {
            Ok(DevContainerConfigurationOutput {
                configuration:
                    DevContainerConfiguration {
                        name: Some(project_name),
                    },
            }) => project_name,
            _ => get_backup_project_name(&remote_workspace_folder, &container_id),
        };
        send_progress(
            DevContainerProgressEvent::StepCompleted(DevContainerBuildStep::ReadConfiguration),
            &progress_tx,
        );

        let connection = DevContainerConnection {
            name: project_name,
            container_id,
            use_podman,
            projects: BTreeSet::new(),
            host_projects: BTreeSet::new(),
            host: None,
        };

        Ok((connection, remote_workspace_folder))
    }
}

#[cfg(not(target_os = "windows"))]
fn dev_container_cli() -> String {
    "devcontainer".to_string()
}

#[cfg(target_os = "windows")]
fn dev_container_cli() -> String {
    "devcontainer.cmd".to_string()
}

fn dev_container_script() -> &'static str {
    "devcontainer.js"
}

fn docker_cli_name(use_podman: bool) -> &'static str {
    if use_podman { "podman" } else { "docker" }
}

fn devcontainer_host_from_remote_options(
    options: &RemoteConnectionOptions,
) -> Option<DevContainerHost> {
    match options {
        RemoteConnectionOptions::Ssh(options) => Some(DevContainerHost::Ssh {
            host: options.host.to_string(),
            username: options.username.clone(),
            port: options.port,
            args: options.args.clone().unwrap_or_default(),
        }),
        RemoteConnectionOptions::Wsl(options) => Some(DevContainerHost::Wsl {
            distro_name: options.distro_name.clone(),
            user: options.user.clone(),
        }),
        _ => None,
    }
}

fn profile_snippet() -> &'static str {
    "if [ -f ~/.bash_profile ]; then . ~/.bash_profile >/dev/null 2>&1; fi; \
if [ -f ~/.profile ]; then . ~/.profile >/dev/null 2>&1; fi; \
if [ -f ~/.bashrc ]; then . ~/.bashrc >/dev/null 2>&1; fi; \
if [ -f ~/.zprofile ]; then . ~/.zprofile >/dev/null 2>&1; fi;"
}

fn wrap_in_login_shell(exec: &str) -> Result<String, DevContainerError> {
    let shell_kind = ShellKind::Posix;
    let script = format!("{} {exec}", profile_snippet());
    let wrapped_bash_exec = shell_kind.try_quote(&script).ok_or_else(|| {
        DevContainerError::DevContainerUpFailed(
            "Shell quoting failed for remote command".to_string(),
        )
    })?;
    let wrapped_exec = shell_kind.try_quote(&script).ok_or_else(|| {
        DevContainerError::DevContainerUpFailed(
            "Shell quoting failed for remote command".to_string(),
        )
    })?;
    Ok(format!(
        "if command -v bash >/dev/null 2>&1; then exec bash -lc {wrapped_bash_exec}; else exec sh -lc {wrapped_exec}; fi"
    ))
}

fn wrap_in_sh_command(exec: &str) -> Result<String, DevContainerError> {
    let shell_kind = ShellKind::Posix;
    let script = format!("{} {exec}", profile_snippet());
    let wrapped_exec = shell_kind.try_quote(&script).ok_or_else(|| {
        DevContainerError::DevContainerUpFailed(
            "Shell quoting failed for remote command".to_string(),
        )
    })?;
    Ok(format!("sh -lc {wrapped_exec}"))
}

fn build_remote_shell_command(
    options: &RemoteConnectionOptions,
    snippet: &str,
) -> Result<Command, DevContainerError> {
    match options {
        RemoteConnectionOptions::Wsl(options) => {
            let exec = wrap_in_login_shell(snippet)?;
            let mut command = util::command::new_smol_command("wsl.exe");
            command.arg("--distribution");
            command.arg(&options.distro_name);
            if let Some(user) = &options.user {
                command.arg("--user");
                command.arg(user);
            }
            command.arg("--");
            command.arg("sh");
            command.arg("-lc");
            command.arg(exec);
            Ok(command)
        }
        RemoteConnectionOptions::Ssh(options) => {
            let exec = wrap_in_sh_command(snippet)?;
            let mut ssh_args = options.additional_args();
            ssh_args.push("-q".to_string());
            ssh_args.push("-T".to_string());
            ssh_args.push(options.ssh_destination());
            ssh_args.push(exec);

            let mut command = util::command::new_smol_command("ssh");
            command.args(ssh_args);
            Ok(command)
        }
        _ => Err(DevContainerError::DevContainerUpFailed(
            "Unsupported remote connection for devcontainer command".to_string(),
        )),
    }
}

fn build_remote_command(
    options: &RemoteConnectionOptions,
    program: &str,
    args: &[String],
    interactive: bool,
) -> Result<Command, DevContainerError> {
    match options {
        RemoteConnectionOptions::Wsl(options) => {
            let shell_kind = ShellKind::Posix;
            let mut exec = String::new();
            use std::fmt::Write as _;
            let program = shell_kind.try_quote_prefix_aware(program).ok_or_else(|| {
                DevContainerError::DevContainerUpFailed(
                    "Shell quoting failed for remote command".to_string(),
                )
            })?;
            write!(exec, "exec {program}").map_err(|err| {
                DevContainerError::DevContainerUpFailed(format!(
                    "Failed to build remote command: {err}"
                ))
            })?;
            for arg in args {
                let quoted = shell_kind.try_quote(arg).ok_or_else(|| {
                    DevContainerError::DevContainerUpFailed(
                        "Shell quoting failed for remote argument".to_string(),
                    )
                })?;
                write!(exec, " {quoted}").map_err(|err| {
                    DevContainerError::DevContainerUpFailed(format!(
                        "Failed to build remote command: {err}"
                    ))
                })?;
            }

            let exec = wrap_in_login_shell(&exec)?;

            let mut command = util::command::new_smol_command("wsl.exe");
            command.arg("--distribution");
            command.arg(&options.distro_name);
            if let Some(user) = &options.user {
                command.arg("--user");
                command.arg(user);
            }
            // Run through a login shell to pick up the user's PATH/environment inside the distro.
            command.arg("--");
            command.arg("sh");
            command.arg("-lc");
            command.arg(exec);
            Ok(command)
        }
        RemoteConnectionOptions::Ssh(options) => {
            let shell_kind = ShellKind::Posix;
            let mut exec = String::new();
            use std::fmt::Write as _;
            let program = shell_kind.try_quote_prefix_aware(program).ok_or_else(|| {
                DevContainerError::DevContainerUpFailed(
                    "Shell quoting failed for remote command".to_string(),
                )
            })?;
            write!(exec, "exec {program}").map_err(|err| {
                DevContainerError::DevContainerUpFailed(format!(
                    "Failed to build remote command: {err}"
                ))
            })?;
            for arg in args {
                let quoted = shell_kind.try_quote(arg).ok_or_else(|| {
                    DevContainerError::DevContainerUpFailed(
                        "Shell quoting failed for remote argument".to_string(),
                    )
                })?;
                write!(exec, " {quoted}").map_err(|err| {
                    DevContainerError::DevContainerUpFailed(format!(
                        "Failed to build remote command: {err}"
                    ))
                })?;
            }

            let exec = wrap_in_sh_command(&exec)?;

            let mut ssh_args = options.additional_args();
            ssh_args.push("-q".to_string());
            ssh_args.push(if interactive { "-t" } else { "-T" }.to_string());
            ssh_args.push(options.ssh_destination());
            ssh_args.push(exec);

            let mut command = util::command::new_smol_command("ssh");
            command.args(ssh_args);
            Ok(command)
        }
        _ => Err(DevContainerError::DevContainerUpFailed(
            "Unsupported remote connection for devcontainer command".to_string(),
        )),
    }
}

async fn ensure_devcontainer_cli_remote(
    options: &RemoteConnectionOptions,
) -> Result<(), DevContainerError> {
    let mut probe = build_remote_shell_command(options, "command -v devcontainer")?;
    match probe.output().await {
        Ok(output) if output.status.success() => {
            let resolved = String::from_utf8_lossy(&output.stdout);
            log::info!(
                "devcontainer CLI resolved on remote host: {}",
                resolved.trim()
            );
        }
        Ok(output) => {
            log::error!(
                "devcontainer CLI not found on remote host. Install @devcontainers/cli and ensure it is on PATH for login shells. out: {:?}, err: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(DevContainerError::DevContainerCliNotAvailable);
        }
        Err(e) => {
            log::error!(
                "Unable to probe devcontainer CLI on remote host: {:?}. Install @devcontainers/cli and ensure it is on PATH for login shells.",
                e
            );
            return Err(DevContainerError::DevContainerCliNotAvailable);
        }
    }

    let mut command =
        build_remote_command(options, "devcontainer", &["--version".to_string()], false)?;
    match command.output().await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            log::error!(
                "devcontainer CLI present but failed to run on remote host: out: {:?}, err: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Err(DevContainerError::DevContainerCliNotAvailable)
        }
        Err(e) => {
            log::error!("Unable to run devcontainer CLI on remote host: {:?}", e);
            Err(DevContainerError::DevContainerCliNotAvailable)
        }
    }
}

async fn check_for_docker_remote(
    options: &RemoteConnectionOptions,
    use_podman: bool,
) -> Result<(), DevContainerError> {
    let docker_cli = if use_podman { "podman" } else { "docker" };
    let mut command = build_remote_command(options, docker_cli, &["--version".to_string()], false)?;
    match command.output().await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            log::error!(
                "Docker CLI not available on remote host: out: {:?}, err: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Err(DevContainerError::DockerNotAvailable)
        }
        Err(e) => {
            log::error!("Unable to run docker on remote host: {:?}", e);
            Err(DevContainerError::DockerNotAvailable)
        }
    }
}
async fn check_for_docker(use_podman: bool) -> Result<(), DevContainerError> {
    let mut command = if use_podman {
        util::command::new_smol_command("podman")
    } else {
        util::command::new_smol_command("docker")
    };
    command.arg("--version");

    match command.output().await {
        Ok(_) => Ok(()),
        Err(e) => {
            log::error!("Unable to find docker in $PATH: {:?}", e);
            Err(DevContainerError::DockerNotAvailable)
        }
    }
}

async fn ensure_devcontainer_cli(
    node_runtime: &NodeRuntime,
) -> Result<(PathBuf, bool), DevContainerError> {
    let mut command = util::command::new_smol_command(&dev_container_cli());
    command.arg("--version");

    if let Err(e) = command.output().await {
        log::error!(
            "Unable to find devcontainer CLI in $PATH. Checking for a zed installed version. Error: {:?}",
            e
        );

        let Ok(node_runtime_path) = node_runtime.binary_path().await else {
            return Err(DevContainerError::NodeRuntimeNotAvailable);
        };

        let datadir_cli_path = paths::devcontainer_dir()
            .join("node_modules")
            .join("@devcontainers")
            .join("cli")
            .join(dev_container_script());

        log::debug!(
            "devcontainer not found in path, using local location: ${}",
            datadir_cli_path.display()
        );

        let mut command =
            util::command::new_smol_command(node_runtime_path.as_os_str().display().to_string());
        command.arg(datadir_cli_path.display().to_string());
        command.arg("--version");

        match command.output().await {
            Err(e) => log::error!(
                "Unable to find devcontainer CLI in Data dir. Will try to install. Error: {:?}",
                e
            ),
            Ok(output) => {
                if output.status.success() {
                    log::info!("Found devcontainer CLI in Data dir");
                    return Ok((datadir_cli_path.clone(), false));
                } else {
                    log::error!(
                        "Could not run devcontainer CLI from data_dir. Will try once more to install. Output: {:?}",
                        output
                    );
                }
            }
        }

        if let Err(e) = fs::create_dir_all(paths::devcontainer_dir()).await {
            log::error!("Unable to create devcontainer directory. Error: {:?}", e);
            return Err(DevContainerError::DevContainerCliNotAvailable);
        }

        if let Err(e) = node_runtime
            .npm_install_packages(
                &paths::devcontainer_dir(),
                &[("@devcontainers/cli", "latest")],
            )
            .await
        {
            log::error!(
                "Unable to install devcontainer CLI to data directory. Error: {:?}",
                e
            );
            return Err(DevContainerError::DevContainerCliNotAvailable);
        };

        let mut command =
            util::command::new_smol_command(node_runtime_path.as_os_str().display().to_string());
        command.arg(datadir_cli_path.display().to_string());
        command.arg("--version");
        if let Err(e) = command.output().await {
            log::error!(
                "Unable to find devcontainer cli after NPM install. Error: {:?}",
                e
            );
            Err(DevContainerError::DevContainerCliNotAvailable)
        } else {
            Ok((datadir_cli_path, false))
        }
    } else {
        log::info!("Found devcontainer cli on $PATH, using it");
        Ok((PathBuf::from(&dev_container_cli()), true))
    }
}

async fn devcontainer_up(
    path_to_cli: &PathBuf,
    found_in_path: bool,
    node_runtime: &NodeRuntime,
    path: Arc<Path>,
    use_podman: bool,
    progress_tx: Option<&UnboundedSender<DevContainerProgressEvent>>,
) -> Result<DevContainerUp, DevContainerError> {
    let Ok(node_runtime_path) = node_runtime.binary_path().await else {
        log::error!("Unable to find node runtime path");
        return Err(DevContainerError::NodeRuntimeNotAvailable);
    };

    let mut command =
        devcontainer_cli_command(path_to_cli, found_in_path, &node_runtime_path, use_podman);
    command.arg("up");
    command.arg("--workspace-folder");
    command.arg(path.display().to_string());

    log::info!("Running full devcontainer up command: {:?}", command);

    match run_command_with_logging(command, progress_tx).await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running devcontainer up for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerUpFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running devcontainer up: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
    }
}

async fn devcontainer_up_remote(
    remote_options: &RemoteConnectionOptions,
    path: &Arc<Path>,
    use_podman: bool,
    progress_tx: Option<&UnboundedSender<DevContainerProgressEvent>>,
) -> Result<DevContainerUp, DevContainerError> {
    let mut args = vec![
        "up".to_string(),
        "--workspace-folder".to_string(),
        path.display().to_string(),
    ];

    if use_podman {
        args.push("--docker-path".to_string());
        args.push("podman".to_string());
    }

    let command = build_remote_command(remote_options, "devcontainer", &args, false)?;
    log::info!("Running remote devcontainer up command: {:?}", command);

    match run_command_with_logging(command, progress_tx).await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                serde_json::from_str::<DevContainerUp>(&raw).map_err(|e| {
                    log::error!(
                        "Unable to parse response from remote 'devcontainer up' command, error: {:?}",
                        e
                    );
                    DevContainerError::DevContainerParseFailed
                })
            } else {
                let message = format!(
                    "Non-success status running devcontainer up for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerUpFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running remote devcontainer up: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
    }
}
async fn devcontainer_read_configuration(
    path_to_cli: &PathBuf,
    found_in_path: bool,
    node_runtime: &NodeRuntime,
    path: &Arc<Path>,
    use_podman: bool,
) -> Result<DevContainerConfigurationOutput, DevContainerError> {
    let Ok(node_runtime_path) = node_runtime.binary_path().await else {
        log::error!("Unable to find node runtime path");
        return Err(DevContainerError::NodeRuntimeNotAvailable);
    };

    let mut command =
        devcontainer_cli_command(path_to_cli, found_in_path, &node_runtime_path, use_podman);
    command.arg("read-configuration");
    command.arg("--workspace-folder");
    command.arg(path.display().to_string());

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running devcontainer read-configuration for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                log::error!("{}", &message);
                Err(DevContainerError::DevContainerNotFound)
            }
        }
        Err(e) => {
            let message = format!("Error running devcontainer read-configuration: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerNotFound)
        }
    }
}
async fn devcontainer_read_configuration_remote(
    remote_options: &RemoteConnectionOptions,
    path: &Arc<Path>,
    use_podman: bool,
) -> Result<DevContainerConfigurationOutput, DevContainerError> {
    let mut args = vec![
        "read-configuration".to_string(),
        "--workspace-folder".to_string(),
        path.display().to_string(),
    ];
    if use_podman {
        args.push("--docker-path".to_string());
        args.push("podman".to_string());
    }
    let mut command = build_remote_command(remote_options, "devcontainer", &args, false)?;

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                serde_json::from_str::<DevContainerConfigurationOutput>(&raw).map_err(|e| {
                    log::error!(
                        "Unable to parse response from remote 'devcontainer read-configuration' command, error: {:?}",
                        e
                    );
                    DevContainerError::DevContainerParseFailed
                })
            } else {
                let message = format!(
                    "Non-success status running devcontainer read-configuration for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                log::error!("{}", &message);
                Err(DevContainerError::DevContainerNotFound)
            }
        }
        Err(e) => {
            let message = format!(
                "Error running remote devcontainer read-configuration: {:?}",
                e
            );
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerNotFound)
        }
    }
}

async fn devcontainer_template_apply(
    template: &DevContainerTemplate,
    template_options: &HashMap<String, String>,
    features_selected: &HashSet<DevContainerFeature>,
    path_to_cli: &PathBuf,
    found_in_path: bool,
    node_runtime: &NodeRuntime,
    path: &Arc<Path>,
    use_podman: bool,
) -> Result<DevContainerApply, DevContainerError> {
    let Ok(node_runtime_path) = node_runtime.binary_path().await else {
        log::error!("Unable to find node runtime path");
        return Err(DevContainerError::NodeRuntimeNotAvailable);
    };

    let mut command =
        devcontainer_cli_command(path_to_cli, found_in_path, &node_runtime_path, use_podman);

    let Ok(serialized_options) = serde_json::to_string(template_options) else {
        log::error!("Unable to serialize options for {:?}", template_options);
        return Err(DevContainerError::DevContainerParseFailed);
    };

    command.arg("templates");
    command.arg("apply");
    command.arg("--workspace-folder");
    command.arg(path.display().to_string());
    command.arg("--template-id");
    command.arg(format!(
        "{}/{}",
        template
            .source_repository
            .as_ref()
            .unwrap_or(&String::from("")),
        template.id
    ));
    command.arg("--template-args");
    command.arg(serialized_options);
    command.arg("--features");
    command.arg(template_features_to_json(features_selected));

    log::debug!("Running full devcontainer apply command: {:?}", command);

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                parse_json_from_cli(&raw)
            } else {
                let message = format!(
                    "Non-success status running devcontainer templates apply for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerTemplateApplyFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running devcontainer templates apply: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerTemplateApplyFailed(message))
        }
    }
}
// Try to parse directly first (newer versions output pure JSON)
// If that fails, look for JSON start (older versions have plaintext prefix)
fn parse_json_from_cli<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, DevContainerError> {
    serde_json::from_str::<T>(&raw)
        .or_else(|e| {
            log::error!("Error parsing json: {} - will try to find json object in larger plaintext", e);
            let json_start = raw
                .find(|c| c == '{')
                .ok_or_else(|| {
                    log::error!("No JSON found in devcontainer up output");
                    DevContainerError::DevContainerParseFailed
                })?;

            serde_json::from_str(&raw[json_start..]).map_err(|e| {
                log::error!(
                    "Unable to parse JSON from devcontainer up output (starting at position {}), error: {:?}",
                    json_start,
                    e
                );
                DevContainerError::DevContainerParseFailed
            })
        })
}

fn parse_json_array_from_cli<T: serde::de::DeserializeOwned>(
    raw: &str,
) -> Result<T, DevContainerError> {
    serde_json::from_str::<T>(raw).or_else(|e| {
        log::error!("Error parsing json: {} - will try to find json array in larger plaintext", e);
        let json_start = raw.find('[').ok_or_else(|| {
            log::error!("No JSON array found in docker inspect output");
            DevContainerError::DevContainerParseFailed
        })?;

        serde_json::from_str(&raw[json_start..]).map_err(|e| {
            log::error!(
                "Unable to parse JSON array from docker inspect output (starting at position {}), error: {:?}",
                json_start,
                e
            );
            DevContainerError::DevContainerParseFailed
        })
    })
}

async fn devcontainer_template_apply_remote(
    template: &DevContainerTemplate,
    template_options: &HashMap<String, String>,
    features_selected: &HashSet<DevContainerFeature>,
    remote_options: &RemoteConnectionOptions,
    path: &Arc<Path>,
) -> Result<DevContainerApply, DevContainerError> {
    let Ok(serialized_options) = serde_json::to_string(template_options) else {
        log::error!("Unable to serialize options for {:?}", template_options);
        return Err(DevContainerError::DevContainerParseFailed);
    };

    let args = vec![
        "templates".to_string(),
        "apply".to_string(),
        "--workspace-folder".to_string(),
        path.display().to_string(),
        "--template-id".to_string(),
        format!(
            "{}/{}",
            template
                .source_repository
                .as_ref()
                .unwrap_or(&String::from("")),
            template.id
        ),
        "--template-args".to_string(),
        serialized_options,
        "--features".to_string(),
        template_features_to_json(features_selected),
    ];

    let mut command = build_remote_command(remote_options, "devcontainer", &args, false)?;

    log::debug!("Running remote devcontainer apply command: {:?}", command);

    match command.output().await {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout);
                serde_json::from_str::<DevContainerApply>(&raw).map_err(|e| {
                    log::error!(
                        "Unable to parse response from remote 'devcontainer templates apply' command, error: {:?}",
                        e
                    );
                    DevContainerError::DevContainerParseFailed
                })
            } else {
                let message = format!(
                    "Non-success status running devcontainer templates apply for workspace: out: {:?}, err: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );

                log::error!("{}", &message);
                Err(DevContainerError::DevContainerTemplateApplyFailed(message))
            }
        }
        Err(e) => {
            let message = format!("Error running remote devcontainer templates apply: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerTemplateApplyFailed(message))
        }
    }
}

async fn read_log_stream(
    reader: impl smol::io::AsyncRead + Unpin,
    stream: DevContainerLogStream,
    progress_tx: Option<UnboundedSender<DevContainerProgressEvent>>,
) -> Result<Vec<u8>, std::io::Error> {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line).await?;
        if bytes == 0 {
            break;
        }
        buffer.extend_from_slice(&line);
        if let Some(tx) = &progress_tx {
            let line_text = String::from_utf8_lossy(&line);
            let _ = tx.unbounded_send(DevContainerProgressEvent::LogLine(DevContainerLogLine {
                stream,
                line: line_text.into_owned(),
            }));
        }
    }
    Ok(buffer)
}

async fn run_command_with_logging(
    mut command: Command,
    progress_tx: Option<&UnboundedSender<DevContainerProgressEvent>>,
) -> Result<std::process::Output, DevContainerError> {
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| {
        DevContainerError::DevContainerUpFailed(format!("Failed to spawn command: {e:?}"))
    })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        DevContainerError::DevContainerUpFailed("Failed to capture stdout".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        DevContainerError::DevContainerUpFailed("Failed to capture stderr".to_string())
    })?;

    let progress_tx = progress_tx.cloned();
    let stdout_task = smol::spawn(read_log_stream(
        stdout,
        DevContainerLogStream::Stdout,
        progress_tx.clone(),
    ));
    let stderr_task = smol::spawn(read_log_stream(
        stderr,
        DevContainerLogStream::Stderr,
        progress_tx.clone(),
    ));

    let status = child
        .status()
        .await
        .map_err(|e| DevContainerError::DevContainerUpFailed(format!("Command failed: {e:?}")))?;

    let stdout = stdout_task.await.map_err(|e| {
        DevContainerError::DevContainerUpFailed(format!("Failed to read stdout: {e:?}"))
    })?;
    let stderr = stderr_task.await.map_err(|e| {
        DevContainerError::DevContainerUpFailed(format!("Failed to read stderr: {e:?}"))
    })?;

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn devcontainer_cli_command(
    path_to_cli: &PathBuf,
    found_in_path: bool,
    node_runtime_path: &PathBuf,
    use_podman: bool,
) -> Command {
    let mut command = if found_in_path {
        util::command::new_smol_command(path_to_cli.display().to_string())
    } else {
        let mut command =
            util::command::new_smol_command(node_runtime_path.as_os_str().display().to_string());
        command.arg(path_to_cli.display().to_string());
        command
    };

    if use_podman {
        command.arg("--docker-path");
        command.arg("podman");
    }
    command
}

fn get_backup_project_name(remote_workspace_folder: &str, container_id: &str) -> String {
    Path::new(remote_workspace_folder)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|string| string.to_string())
        .unwrap_or_else(|| container_id.to_string())
}

fn project_context(cx: &mut AsyncWindowContext) -> Option<ProjectContext> {
    let Some(workspace) = cx.window_handle().downcast::<Workspace>() else {
        return None;
    };

    match workspace.update(cx, |workspace, _, cx| {
        let project = workspace.project().read(cx);
        let directory = project.active_project_directory(cx);
        let remote_options = project
            .remote_client()
            .map(|remote_client| remote_client.read(cx).connection_options());
        (directory, remote_options)
    }) {
        Ok((Some(directory), remote_options)) => Some(ProjectContext {
            directory,
            remote_options,
        }),
        Ok((None, _)) => None,
        Err(e) => {
            log::error!("Error getting project context from workspace: {:?}", e);
            None
        }
    }
}

async fn resolve_project_context_for_devcontainer(
    cx: &mut AsyncWindowContext,
) -> Result<ProjectContext, DevContainerError> {
    let Some(ProjectContext {
        directory,
        remote_options,
    }) = project_context(cx)
    else {
        return Err(DevContainerError::NotInValidProject);
    };

    let Some(remote_options) = remote_options else {
        return Ok(ProjectContext {
            directory,
            remote_options: None,
        });
    };

    match remote_options {
        RemoteConnectionOptions::Docker(options) => {
            resolve_docker_project_context(directory, options).await
        }
        _ => Ok(ProjectContext {
            directory,
            remote_options: Some(remote_options),
        }),
    }
}

async fn resolve_docker_project_context(
    directory: Arc<Path>,
    options: DockerConnectionOptions,
) -> Result<ProjectContext, DevContainerError> {
    let host_remote_options = remote_options_for_docker_host(&options.host);
    let host_directory =
        resolve_host_directory_for_docker(&host_remote_options, &options, &directory).await?;

    Ok(ProjectContext {
        directory: host_directory,
        remote_options: host_remote_options,
    })
}

fn remote_options_for_docker_host(host: &DockerHost) -> Option<RemoteConnectionOptions> {
    match host {
        DockerHost::Local => None,
        DockerHost::Wsl(options) => Some(RemoteConnectionOptions::Wsl(options.clone())),
        DockerHost::Ssh(options) => Some(RemoteConnectionOptions::Ssh(options.clone())),
    }
}

async fn resolve_host_directory_for_docker(
    host_remote_options: &Option<RemoteConnectionOptions>,
    options: &DockerConnectionOptions,
    container_directory: &Arc<Path>,
) -> Result<Arc<Path>, DevContainerError> {
    let mounts =
        docker_inspect_mounts(host_remote_options, &options.container_id, options.use_podman)
            .await?;
    let container_path = container_directory.display().to_string();
    let host_path = host_path_from_mounts(&mounts, &container_path)?;
    Ok(Arc::from(PathBuf::from(host_path)))
}

async fn docker_inspect_mounts(
    host_remote_options: &Option<RemoteConnectionOptions>,
    container_id: &str,
    use_podman: bool,
) -> Result<Vec<DockerMount>, DevContainerError> {
    let args = vec![
        "inspect".to_string(),
        "--format".to_string(),
        "{{json .Mounts}}".to_string(),
        container_id.to_string(),
    ];
    let mut command = build_host_docker_command(host_remote_options, use_podman, &args)?;

    match command.output().await {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            parse_json_array_from_cli::<Vec<DockerMount>>(&raw)
        }
        Ok(output) => {
            let message = format!(
                "Non-success status running docker inspect for container: out: {:?}, err: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
        Err(e) => {
            let message = format!("Error running docker inspect: {:?}", e);
            log::error!("{}", &message);
            Err(DevContainerError::DevContainerUpFailed(message))
        }
    }
}

fn build_host_docker_command(
    host_remote_options: &Option<RemoteConnectionOptions>,
    use_podman: bool,
    args: &[String],
) -> Result<Command, DevContainerError> {
    let docker_cli = docker_cli_name(use_podman);
    if let Some(remote_options) = host_remote_options {
        build_remote_command(remote_options, docker_cli, args, false)
    } else {
        let mut command = util::command::new_smol_command(docker_cli);
        command.args(args);
        Ok(command)
    }
}

fn host_path_from_mounts(
    mounts: &[DockerMount],
    container_path: &str,
) -> Result<String, DevContainerError> {
    let container_path = trim_trailing_slash(container_path);
    let mut best: Option<(&DockerMount, &str)> = None;

    for mount in mounts {
        let destination = trim_trailing_slash(&mount.destination);
        let rest = container_path.strip_prefix(destination);
        let is_match = match rest {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        };
        if is_match {
            let replace = best
                .as_ref()
                .map_or(true, |(_, best_dest)| destination.len() > best_dest.len());
            if replace {
                best = Some((mount, destination));
            }
        }
    }

    let Some((mount, destination)) = best else {
        return Err(DevContainerError::DevContainerUpFailed(
            "Unable to resolve host workspace path for dev container".to_string(),
        ));
    };

    let suffix = container_path.strip_prefix(destination).unwrap_or("");
    Ok(join_host_path(&mount.source, suffix))
}

fn trim_trailing_slash(path: &str) -> &str {
    let trimmed = path.trim_end_matches(&['/', '\\'][..]);
    if trimmed.is_empty() { path } else { trimmed }
}

fn join_host_path(source: &str, suffix: &str) -> String {
    let source = trim_trailing_slash(source);
    let suffix = suffix.trim_start_matches(&['/', '\\'][..]);
    if suffix.is_empty() {
        return source.to_string();
    }

    let is_windows = source.contains('\\') || source.contains(':');
    let sep = if is_windows { '\\' } else { '/' };
    let mut base = source.to_string();
    if !base.ends_with(sep) && !base.ends_with('/') && !base.ends_with('\\') {
        base.push(sep);
    }
    let tail = if is_windows {
        suffix.replace('/', "\\")
    } else {
        suffix.to_string()
    };
    base.push_str(&tail);
    base
}

fn template_features_to_json(features_selected: &HashSet<DevContainerFeature>) -> String {
    let features_map = features_selected
        .iter()
        .map(|feature| {
            let mut map = HashMap::new();
            map.insert(
                "id",
                format!(
                    "{}/{}:{}",
                    feature
                        .source_repository
                        .as_ref()
                        .unwrap_or(&String::from("")),
                    feature.id,
                    feature.major_version()
                ),
            );
            map
        })
        .collect::<Vec<HashMap<&str, String>>>();
    serde_json::to_string(&features_map).unwrap()
}

#[cfg(test)]
mod tests {
    use crate::devcontainer_api::{DevContainerUp, parse_json_from_cli};

    #[test]
    fn should_parse_from_devcontainer_json() {
        let json = r#"{"outcome":"success","containerId":"826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/zed"}"#;
        let up: DevContainerUp = parse_json_from_cli(json).unwrap();
        assert_eq!(up._outcome, "success");
        assert_eq!(
            up.container_id,
            "826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a"
        );
        assert_eq!(up._remote_user, "vscode");
        assert_eq!(up.remote_workspace_folder, "/workspaces/zed");

        let json_in_plaintext = r#"[2026-01-22T16:19:08.802Z] @devcontainers/cli 0.80.1. Node.js v22.21.1. darwin 24.6.0 arm64.
            {"outcome":"success","containerId":"826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/zed"}"#;
        let up: DevContainerUp = parse_json_from_cli(json_in_plaintext).unwrap();
        assert_eq!(up._outcome, "success");
        assert_eq!(
            up.container_id,
            "826abcac45afd412abff083ab30793daff2f3c8ce2c831df728baf39933cb37a"
        );
        assert_eq!(up._remote_user, "vscode");
        assert_eq!(up.remote_workspace_folder, "/workspaces/zed");
    }
}
