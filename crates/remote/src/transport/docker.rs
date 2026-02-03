use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use collections::HashMap;
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use semver::Version as SemanticVersion;
use smol::process::Command;
use std::time::Instant;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use util::ResultExt;
use util::shell::ShellKind;
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
};

use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext, AsyncApp, Task};
use rpc::proto::Envelope;

use crate::{
    RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive},
    transport::{parse_platform, ssh::SshConnectionOptions, wsl::WslConnectionOptions},
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct DockerConnectionOptions {
    pub name: String,
    pub container_id: String,
    pub remote_user: String,
    pub upload_binary_over_docker_exec: bool,
    pub use_podman: bool,
    pub host: DockerHost,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DockerHost {
    Local,
    Ssh(SshConnectionOptions),
    Wsl(WslConnectionOptions),
}

impl Default for DockerHost {
    fn default() -> Self {
        DockerHost::Local
    }
}

fn docker_cli_name(use_podman: bool) -> &'static str {
    if use_podman { "podman" } else { "docker" }
}

fn build_docker_command_template(
    options: &DockerConnectionOptions,
    docker_args: Vec<String>,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    match &options.host {
        DockerHost::Local => Ok(CommandTemplate {
            program: docker_cli_name(options.use_podman).to_string(),
            args: docker_args,
            env: Default::default(),
        }),
        DockerHost::Wsl(host_options) => {
            let mut wsl_args = vec![
                "--distribution".to_string(),
                host_options.distro_name.clone(),
            ];
            if let Some(user) = &host_options.user {
                wsl_args.push("--user".to_string());
                wsl_args.push(user.clone());
            }
            wsl_args.push("--".to_string());
            wsl_args.push(docker_cli_name(options.use_podman).to_string());
            wsl_args.extend(docker_args);

            Ok(CommandTemplate {
                program: "wsl.exe".to_string(),
                args: wsl_args,
                env: Default::default(),
            })
        }
        DockerHost::Ssh(host_options) => {
            use std::fmt::Write as _;

            let shell_kind = ShellKind::Posix;
            let program = shell_kind
                .try_quote_prefix_aware(docker_cli_name(options.use_podman))
                .context("shell quoting")?;
            let mut exec = String::new();
            write!(exec, "exec {program}").context("build ssh command")?;

            for arg in docker_args {
                let arg = shell_kind.try_quote(&arg).context("shell quoting")?;
                write!(exec, " {arg}").context("build ssh command")?;
            }

            let mut ssh_args = host_options.additional_args();
            ssh_args.push("-q".to_string());
            ssh_args.push(match interactive {
                Interactive::Yes => "-t".to_string(),
                Interactive::No => "-T".to_string(),
            });
            ssh_args.push(host_options.ssh_destination());
            ssh_args.push(exec);

            Ok(CommandTemplate {
                program: "ssh".to_string(),
                args: ssh_args,
                env: Default::default(),
            })
        }
    }
}

async fn docker_cp_source_path(
    options: &DockerConnectionOptions,
    src_path: &Path,
) -> Result<String> {
    match &options.host {
        DockerHost::Local => Ok(src_path.display().to_string()),
        DockerHost::Wsl(options) => {
            #[cfg(target_os = "windows")]
            {
                options
                    .abs_windows_path_to_wsl_path(src_path)
                    .await
                    .context("converting Windows path to WSL path")
            }
            #[cfg(not(target_os = "windows"))]
            {
                Ok(src_path.display().to_string())
            }
        }
        DockerHost::Ssh(_) => {
            anyhow::bail!(
                "Cannot upload local files to a container over SSH. Ensure the container can download the server binary."
            )
        }
    }
}

fn command_from_template(template: CommandTemplate) -> Command {
    let mut command = util::command::new_smol_command(template.program);
    command.args(template.args).envs(template.env);
    command
}

pub(crate) struct DockerExecConnection {
    proxy_process: Mutex<Option<u32>>,
    remote_dir_for_server: String,
    remote_binary_relpath: Option<Arc<RelPath>>,
    connection_options: DockerConnectionOptions,
    remote_platform: Option<RemotePlatform>,
    path_style: Option<PathStyle>,
    shell: String,
}

impl DockerExecConnection {
    pub async fn new(
        connection_options: DockerConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let mut this = Self {
            proxy_process: Mutex::new(None),
            remote_dir_for_server: "/".to_string(),
            remote_binary_relpath: None,
            connection_options,
            remote_platform: None,
            path_style: None,
            shell: "sh".to_owned(),
        };
        let (release_channel, version, commit) = cx.update(|cx| {
            (
                ReleaseChannel::global(cx),
                AppVersion::global(cx),
                AppCommitSha::try_global(cx),
            )
        });
        let remote_platform = this.check_remote_platform().await?;

        this.path_style = match remote_platform.os {
            RemoteOs::Windows => Some(PathStyle::Windows),
            _ => Some(PathStyle::Posix),
        };

        this.remote_platform = Some(remote_platform);
        log::info!("Remote platform discovered: {:?}", this.remote_platform);

        this.shell = this.discover_shell().await;
        log::info!("Remote shell discovered: {}", this.shell);

        this.remote_dir_for_server = this.docker_user_home_dir().await?.trim().to_string();

        this.remote_binary_relpath = Some(
            this.ensure_server_binary(
                &delegate,
                release_channel,
                version,
                &this.remote_dir_for_server,
                commit,
                cx,
            )
            .await?,
        );

        Ok(this)
    }

    fn docker_command_template(
        &self,
        docker_args: Vec<String>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        build_docker_command_template(&self.connection_options, docker_args, interactive)
    }

    async fn discover_shell(&self) -> String {
        const DEFAULT_SHELL: &str = "sh";
        // Try $SHELL first; fall back to passwd entry, then default.
        const SHELL_PROBE: &str = "printf %s \"$SHELL\"";

        match self
            .run_docker_exec("sh", None, &Default::default(), &["-c", SHELL_PROBE])
            .await
        {
            Ok(shell) => match shell.trim() {
                "" => {
                    log::info!("$SHELL is not set, checking passwd for user");
                }
                shell => {
                    if self.is_shell_executable(shell).await {
                        return shell.to_owned();
                    }
                    log::info!("$SHELL points to a missing shell, checking passwd for user");
                }
            },
            Err(e) => {
                log::error!("Failed to get $SHELL: {e}. Checking passwd for user");
            }
        }

        match self
            .run_docker_exec(
                "sh",
                None,
                &Default::default(),
                &["-c", "getent passwd \"$(id -un)\" | cut -d: -f7"],
            )
            .await
        {
            Ok(shell) => match shell.trim() {
                "" => {
                    log::info!("No shell found in passwd, falling back to bash");
                }
                shell => {
                    if self.is_shell_executable(shell).await {
                        return shell.to_owned();
                    }
                    log::info!("Shell from passwd is missing, falling back to bash");
                }
            },
            Err(e) => {
                log::info!(
                    "Error getting shell from passwd: {e}. Falling back to bash",
                );
            }
        }

        if let Some(bash) = self.find_shell_in_path("bash").await {
            return bash;
        }
        DEFAULT_SHELL.to_owned()
    }

    async fn is_shell_executable(&self, shell: &str) -> bool {
        let shell = shell.trim();
        if shell.is_empty() {
            return false;
        }
        let shell_kind = ShellKind::Posix;
        let Some(quoted_shell) = shell_kind.try_quote(shell) else {
            return false;
        };
        let test_cmd = format!("test -x {quoted_shell}");
        self.run_docker_exec("sh", None, &Default::default(), &["-c", &test_cmd])
            .await
            .is_ok()
    }

    async fn find_shell_in_path(&self, name: &str) -> Option<String> {
        let shell_kind = ShellKind::Posix;
        let quoted = shell_kind.try_quote(name)?;
        let probe = format!("command -v {quoted}");
        let output = self
            .run_docker_exec("sh", None, &Default::default(), &["-c", &probe])
            .await
            .ok()?;
        let path = output.trim();
        if path.is_empty() {
            None
        } else {
            Some(path.to_owned())
        }
    }

    async fn check_remote_platform(&self) -> Result<RemotePlatform> {
        let uname = self
            .run_docker_exec("uname", None, &Default::default(), &["-sm"])
            .await?;
        parse_platform(&uname)
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: SemanticVersion,
        remote_dir_for_server: &str,
        commit: Option<AppCommitSha>,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        let remote_platform = self
            .remote_platform
            .context("No remote platform defined; cannot proceed.")?;

        let version_str = match release_channel {
            ReleaseChannel::Nightly => {
                let commit = commit.map(|s| s.full()).unwrap_or_default();
                format!("{}-{}", version, commit)
            }
            ReleaseChannel::Dev => "build".to_string(),
            _ => version.to_string(),
        };
        let binary_name = format!(
            "zed-remote-server-{}-{}",
            release_channel.dev_name(),
            version_str
        );
        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::unix(&binary_name).unwrap());

        let binary_exists_on_server = self
            .run_docker_exec(
                &dst_path.display(self.path_style()),
                Some(&remote_dir_for_server),
                &Default::default(),
                &["version"],
            )
            .await
            .is_ok();
        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        if matches!(
            self.connection_options.host,
            DockerHost::Local | DockerHost::Wsl(_)
        ) {
            match super::build_remote_server_from_source(
                &remote_platform,
                delegate.as_ref(),
                binary_exists_on_server,
                cx,
            )
            .await
            {
                Ok(Some(remote_server_path)) => {
                    let tmp_path = paths::remote_server_dir_relative().join(
                        RelPath::unix(&format!(
                            "download-{}-{}",
                            std::process::id(),
                            remote_server_path.file_name().unwrap().to_string_lossy()
                        ))
                        .unwrap(),
                    );
                    self.upload_local_server_binary(
                        &remote_server_path,
                        &tmp_path,
                        &remote_dir_for_server,
                        delegate,
                        cx,
                    )
                    .await?;
                    self.extract_server_binary(
                        &dst_path,
                        &tmp_path,
                        &remote_dir_for_server,
                        delegate,
                        cx,
                    )
                    .await?;
                    return Ok(dst_path);
                }
                Ok(None) => {}
                Err(err) => {
                    if matches!(release_channel, ReleaseChannel::Dev) {
                        return Err(err);
                    }
                    log::warn!(
                        "Failed to build remote server from source, falling back to download: {err:#}",
                    );
                }
            }
        }

        if binary_exists_on_server {
            return Ok(dst_path);
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => anyhow::bail!(
                "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at ({:?})",
                dst_path
            ),
            _ => Ok(Some(AppVersion::global(cx))),
        })?;

        let tmp_path_gz = paths::remote_server_dir_relative().join(
            RelPath::unix(&format!(
                "{}-download-{}.gz",
                binary_name,
                std::process::id()
            ))
            .unwrap(),
        );
        if !self.connection_options.upload_binary_over_docker_exec
            && let Some(url) = delegate
                .get_download_url(remote_platform, release_channel, wanted_version.clone(), cx)
                .await?
        {
            match self
                .download_binary_on_server(&url, &tmp_path_gz, &remote_dir_for_server, delegate, cx)
                .await
            {
                Ok(_) => {
                    self.extract_server_binary(
                        &dst_path,
                        &tmp_path_gz,
                        &remote_dir_for_server,
                        delegate,
                        cx,
                    )
                    .await
                    .context("extracting server binary")?;
                    return Ok(dst_path);
                }
                Err(e) => {
                    log::error!(
                        "Failed to download binary on server, attempting to download locally and then upload it the server: {e:#}",
                    )
                }
            }
        }

        if matches!(self.connection_options.host, DockerHost::Ssh(_)) {
            anyhow::bail!(
                "Failed to download the remote server binary inside the container. Uploading a local binary over SSH is not supported."
            );
        }

        let src_path = delegate
            .download_server_binary_locally(remote_platform, release_channel, wanted_version, cx)
            .await
            .context("downloading server binary locally")?;
        self.upload_local_server_binary(
            &src_path,
            &tmp_path_gz,
            &remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("uploading server binary")?;
        self.extract_server_binary(
            &dst_path,
            &tmp_path_gz,
            &remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("extracting server binary")?;
        Ok(dst_path)
    }

    async fn docker_user_home_dir(&self) -> Result<String> {
        let inner_program = self.shell();
        const HOME_PROBE: &str = r#"
user="$(id -un 2>/dev/null || true)"
uid="$(id -u 2>/dev/null || true)"
home_from_uid=""
home_from_user=""
home_from_tilde=""
home_from_user_dir=""
if [ -n "$uid" ]; then
  home_from_uid="$(getent passwd "$uid" 2>/dev/null | cut -d: -f6)"
fi
if [ -n "$user" ]; then
  home_from_user="$(getent passwd "$user" 2>/dev/null | cut -d: -f6)"
  home_from_user_dir="/home/$user"
  home_from_tilde="$(eval "printf %s ~$user" 2>/dev/null)"
else
  home_from_tilde="$(eval "printf %s ~" 2>/dev/null)"
fi
for home in "$home_from_user" "$home_from_uid" "$HOME" "$home_from_tilde" "$home_from_user_dir" "/workspace" "/tmp" "/var/tmp"; do
  if [ -n "$home" ] && [ -d "$home" ] && [ -w "$home" ]; then
    echo "$home"
    exit 0
  fi
done
echo "/tmp"
"#;
        self.run_docker_exec(
            &inner_program,
            None,
            &Default::default(),
            &["-c", HOME_PROBE],
        )
        .await
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some("Extracting remote development server"), cx);
        let server_mode = 0o755;

        // TODO: Consider using the remote's actual shell instead of hardcoding "sh"
        let shell_kind = ShellKind::Posix;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let server_mode = format!("{:o}", server_mode);
        let server_mode = shell_kind
            .try_quote(&server_mode)
            .context("shell quoting")?;
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;
        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".gz") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(&tmp_path).context("shell quoting")?;
            format!(
                "gunzip -f {orig_tmp_path} && chmod {server_mode} {tmp_path} && mv {tmp_path} {dst_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("chmod {server_mode} {orig_tmp_path} && mv {orig_tmp_path} {dst_path}",)
        };
        let args = shell_kind.args_for_shell(false, script.to_string());
        self.run_docker_exec(
            "sh",
            Some(&remote_dir_for_server),
            &Default::default(),
            &args,
        )
        .await
        .log_err();
        Ok(())
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path_gz: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path_gz.parent() {
            self.run_docker_exec(
                "mkdir",
                Some(remote_dir_for_server),
                &Default::default(),
                &["-p", parent.display(self.path_style()).as_ref()],
            )
            .await?;
        }

        let src_stat = smol::fs::metadata(&src_path).await?;
        let size = src_stat.len();

        let t0 = Instant::now();
        delegate.set_status(Some("Uploading remote development server"), cx);
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path_gz,
            size / 1024
        );
        self.upload_file(src_path, tmp_path_gz, remote_dir_for_server)
            .await
            .context("failed to upload server binary")?;
        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn upload_and_chown(
        connection_options: &DockerConnectionOptions,
        src_path: String,
        dst_path: String,
    ) -> Result<()> {
        let docker_args = vec![
            "cp".to_string(),
            "-a".to_string(),
            src_path.clone(),
            format!("{}:{}", connection_options.container_id, dst_path),
        ];
        let template =
            build_docker_command_template(connection_options, docker_args, Interactive::No)?;
        let mut command = command_from_template(template);
        let output = command.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("failed to upload via docker cp {src_path} -> {dst_path}: {stderr}",);
            anyhow::bail!(
                "failed to upload via docker cp {} -> {}: {}",
                src_path,
                dst_path,
                stderr,
            );
        }

        let docker_args = vec![
            "exec".to_string(),
            connection_options.container_id.clone(),
            "chown".to_string(),
            format!(
                "{}:{}",
                connection_options.remote_user, connection_options.remote_user,
            ),
            dst_path.clone(),
        ];
        let template =
            build_docker_command_template(connection_options, docker_args, Interactive::No)?;
        let mut chown_command = command_from_template(template);

        let output = chown_command.output().await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!("failed to change ownership for via chown: {stderr}",);
        anyhow::bail!(
            "failed to change ownership for zed_remote_server via chown: {}",
            stderr,
        );
    }

    async fn upload_file(
        &self,
        src_path: &Path,
        dest_path: &RelPath,
        remote_dir_for_server: &str,
    ) -> Result<()> {
        log::debug!("uploading file {:?} to {:?}", src_path, dest_path);

        let src_path_display = docker_cp_source_path(&self.connection_options, src_path).await?;
        let dest_path_str = dest_path.display(self.path_style());
        let full_server_path = format!("{}/{}", remote_dir_for_server, dest_path_str);

        Self::upload_and_chown(&self.connection_options, src_path_display, full_server_path).await
    }

    async fn run_docker_command(
        &self,
        subcommand: &str,
        args: &[impl AsRef<str>],
    ) -> Result<String> {
        let mut docker_args = Vec::with_capacity(1 + args.len());
        docker_args.push(subcommand.to_string());
        for arg in args {
            docker_args.push(arg.as_ref().to_string());
        }

        let template = self.docker_command_template(docker_args, Interactive::No)?;
        let mut command = command_from_template(template);
        let output = command.output().await?;
        log::debug!("{:?}: {:?}", command, output);
        anyhow::ensure!(
            output.status.success(),
            "failed to run command {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn run_docker_exec(
        &self,
        inner_program: &str,
        working_directory: Option<&str>,
        env: &HashMap<String, String>,
        program_args: &[impl AsRef<str>],
    ) -> Result<String> {
        let mut args = match working_directory {
            Some(dir) => vec!["-w".to_string(), dir.to_string()],
            None => vec![],
        };

        args.push("-u".to_string());
        args.push(self.connection_options.remote_user.clone());

        for (k, v) in env.iter() {
            args.push("-e".to_string());
            let env_declaration = format!("{}={}", k, v);
            args.push(env_declaration);
        }

        args.push(self.connection_options.container_id.clone());
        args.push(inner_program.to_string());

        for arg in program_args {
            args.push(arg.as_ref().to_owned());
        }
        self.run_docker_command("exec", args.as_ref()).await
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path_gz: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path_gz.parent() {
            self.run_docker_exec(
                "mkdir",
                Some(remote_dir_for_server),
                &Default::default(),
                &["-p", parent.display(self.path_style()).as_ref()],
            )
            .await?;
        }

        delegate.set_status(Some("Downloading remote development server on host"), cx);

        match self
            .run_docker_exec(
                "curl",
                Some(remote_dir_for_server),
                &Default::default(),
                &[
                    "-f",
                    "-L",
                    url,
                    "-o",
                    &tmp_path_gz.display(self.path_style()),
                ],
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if self
                    .run_docker_exec("which", None, &Default::default(), &["curl"])
                    .await
                    .is_ok()
                {
                    return Err(e);
                }

                log::info!("curl is not available, trying wget");
                match self
                    .run_docker_exec(
                        "wget",
                        Some(remote_dir_for_server),
                        &Default::default(),
                        &[url, "-O", &tmp_path_gz.display(self.path_style())],
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        if self
                            .run_docker_exec("which", None, &Default::default(), &["wget"])
                            .await
                            .is_ok()
                        {
                            return Err(e);
                        } else {
                            anyhow::bail!("Neither curl nor wget is available");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn kill_inner(&self) -> Result<()> {
        if let Some(pid) = self.proxy_process.lock().take() {
            #[cfg(target_os = "windows")]
            {
                let status = util::command::new_std_command("taskkill")
                    .arg("/PID")
                    .arg(pid.to_string())
                    .arg("/T")
                    .arg("/F")
                    .status();
                if status.map(|s| s.success()).unwrap_or(false) {
                    Ok(())
                } else {
                    // If the process is already gone or we can't kill it, don't fail reconnect.
                    Ok(())
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                if util::command::new_smol_command("kill")
                    .arg(pid.to_string())
                    .spawn()
                    .is_ok()
                {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Failed to kill process"))
                }
            }
        } else {
            Ok(())
        }
    }
}

#[async_trait(?Send)]
impl RemoteConnection for DockerExecConnection {
    fn has_wsl_interop(&self) -> bool {
        false
    }
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        // We'll try connecting anew every time we open a devcontainer, so proactively try to kill any old connections.
        if !self.has_been_killed() {
            if let Err(e) = self.kill_inner() {
                return Task::ready(Err(e));
            };
        }

        delegate.set_status(Some("Starting proxy"), cx);

        let Some(remote_binary_relpath) = self.remote_binary_relpath.clone() else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let mut docker_args = vec!["exec".to_string()];
        for env_var in ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS", "GITHUB_TOKEN"] {
            if let Some(value) = std::env::var(env_var).ok() {
                docker_args.push("-e".to_string());
                docker_args.push(format!("{}='{}'", env_var, value));
            }
        }

        docker_args.extend([
            "-u".to_string(),
            self.connection_options.remote_user.to_string(),
            "-w".to_string(),
            self.remote_dir_for_server.clone(),
            "-i".to_string(),
            self.connection_options.container_id.to_string(),
        ]);

        let val = remote_binary_relpath
            .display(self.path_style())
            .into_owned();
        docker_args.push(val);
        docker_args.push("proxy".to_string());
        docker_args.push("--identifier".to_string());
        docker_args.push(unique_identifier);
        if reconnect {
            docker_args.push("--reconnect".to_string());
        }
        let template = match self.docker_command_template(docker_args, Interactive::No) {
            Ok(template) => template,
            Err(err) => return Task::ready(Err(err)),
        };
        let mut command = command_from_template(template);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let Ok(child) = command.spawn() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Failed to start remote server process"
            )));
        };

        let mut proxy_process = self.proxy_process.lock();
        *proxy_process = Some(child.id());

        cx.spawn(async move |cx| {
            super::handle_rpc_messages_over_child_process_stdio(
                child,
                incoming_tx,
                outgoing_rx,
                connection_activity_tx,
                cx,
            )
            .await
            .and_then(|status| {
                if status != 0 {
                    anyhow::bail!("Remote server exited with status {status}");
                }
                Ok(0)
            })
        })
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let connection_options = self.connection_options.clone();
        let dest_path_str = dest_path.to_string();

        cx.background_spawn(async move {
            let src_path_display = docker_cp_source_path(&connection_options, &src_path).await?;
            Self::upload_and_chown(&connection_options, src_path_display, dest_path_str).await
        })
    }

    async fn kill(&self) -> Result<()> {
        self.kill_inner()
    }

    fn has_been_killed(&self) -> bool {
        self.proxy_process.lock().is_none()
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        _port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let mut parsed_working_dir = None;

        let path_style = self.path_style();

        if let Some(working_dir) = working_dir {
            let working_dir = RemotePathBuf::new(working_dir, path_style).to_string();

            const TILDE_PREFIX: &'static str = "~/";
            if working_dir.starts_with(TILDE_PREFIX) {
                let working_dir = working_dir.trim_start_matches("~").trim_start_matches("/");
                parsed_working_dir = Some(format!("$HOME/{working_dir}"));
            } else {
                parsed_working_dir = Some(working_dir);
            }
        }

        let mut inner_program = Vec::new();

        if let Some(program) = program {
            inner_program.push(program);
            for arg in args {
                inner_program.push(arg.clone());
            }
        } else {
            inner_program.push(self.shell());
            inner_program.push("-l".to_string());
        };

        let mut docker_args = vec![
            "exec".to_string(),
            "-u".to_string(),
            self.connection_options.remote_user.clone(),
        ];

        if let Some(parsed_working_dir) = parsed_working_dir {
            docker_args.push("-w".to_string());
            docker_args.push(parsed_working_dir);
        }

        for (k, v) in env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{}={}", k, v));
        }

        match interactive {
            Interactive::Yes => docker_args.push("-it".to_string()),
            Interactive::No => docker_args.push("-i".to_string()),
        }
        docker_args.push(self.connection_options.container_id.to_string());

        docker_args.append(&mut inner_program);

        self.docker_command_template(docker_args, interactive)
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        Err(anyhow::anyhow!("Not currently supported for docker_exec"))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Docker(self.connection_options.clone())
    }

    fn path_style(&self) -> PathStyle {
        self.path_style.unwrap_or(PathStyle::Posix)
    }

    fn shell(&self) -> String {
        self.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.shell.clone()
    }
}
