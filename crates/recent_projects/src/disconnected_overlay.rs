use gpui::{
    ClickEvent, DismissEvent, EventEmitter, FocusHandle, Focusable, PromptLevel, Render, WeakEntity,
};
use project::project_settings::ProjectSettings;
use remote::{
    DockerConnectionOptions, DockerHost, RemoteConnectionOptions, SshConnectionOptions,
    WslConnectionOptions,
};
use settings::{DevContainerHost, Settings};
use std::path::PathBuf;
use ui::{
    Button, ButtonCommon, ButtonStyle, Clickable, Context, ElevationIndex, FluentBuilder, Headline,
    HeadlineSize, IconName, IconPosition, InteractiveElement, IntoElement, Label, Modal,
    ModalFooter, ModalHeader, ParentElement, Section, Styled, StyledExt, Window, div, h_flex, rems,
};
use workspace::{ModalView, OpenOptions, Workspace, notifications::DetachAndPromptErr};

use crate::{open_remote_project, remote_connections::RemoteSettings};
use util::ResultExt as _;

enum Host {
    CollabGuestProject,
    RemoteServerProject(RemoteConnectionOptions, bool),
}

pub struct DisconnectedOverlay {
    workspace: WeakEntity<Workspace>,
    host: Host,
    focus_handle: FocusHandle,
    finished: bool,
}

impl EventEmitter<DismissEvent> for DisconnectedOverlay {}
impl Focusable for DisconnectedOverlay {
    fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}
impl ModalView for DisconnectedOverlay {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        workspace::DismissDecision::Dismiss(self.finished)
    }
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl DisconnectedOverlay {
    pub fn register(
        workspace: &mut Workspace,
        window: Option<&mut Window>,
        cx: &mut Context<Workspace>,
    ) {
        let Some(window) = window else {
            return;
        };
        cx.subscribe_in(
            workspace.project(),
            window,
            |workspace, project, event, window, cx| {
                if !matches!(
                    event,
                    project::Event::DisconnectedFromHost
                        | project::Event::DisconnectedFromRemote { .. }
                ) {
                    return;
                }
                let handle = cx.entity().downgrade();

                let remote_connection_options = project.read(cx).remote_connection_options(cx);
                if let Some(RemoteConnectionOptions::Docker(options)) = remote_connection_options.clone()
                {
                    if Self::return_devcontainer_to_host_on_disconnect(
                        &options,
                        workspace,
                        window,
                        cx,
                    ) {
                        return;
                    }
                }
                let host = if let Some(remote_connection_options) = remote_connection_options {
                    Host::RemoteServerProject(
                        remote_connection_options,
                        matches!(
                            event,
                            project::Event::DisconnectedFromRemote {
                                server_not_running: true
                            }
                        ),
                    )
                } else {
                    Host::CollabGuestProject
                };

                workspace.toggle_modal(window, cx, |_, cx| DisconnectedOverlay {
                    finished: false,
                    workspace: handle,
                    host,
                    focus_handle: cx.focus_handle(),
                });
            },
        )
        .detach();
    }

    fn handle_reconnect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent);

        if let Host::RemoteServerProject(remote_connection_options, _) = &self.host {
            self.reconnect_to_remote_project(remote_connection_options.clone(), window, cx);
        }
    }

    fn handle_return_to_host(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent);

        let Host::RemoteServerProject(RemoteConnectionOptions::Docker(options), _) = &self.host else {
            return;
        };

        self.return_devcontainer_to_host(options, window, cx);
    }

    fn reconnect_to_remote_project(
        &self,
        connection_options: RemoteConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let Some(window_handle) = window.window_handle().downcast::<Workspace>() else {
            return;
        };

        let app_state = workspace.read(cx).app_state().clone();
        let paths = workspace
            .read(cx)
            .root_paths(cx)
            .iter()
            .map(|path| path.to_path_buf())
            .collect();

        cx.spawn_in(window, async move |_, cx| {
            open_remote_project(
                connection_options,
                paths,
                app_state,
                OpenOptions {
                    replace_window: Some(window_handle),
                    ..Default::default()
                },
                cx,
            )
            .await?;
            Ok(())
        })
        .detach_and_prompt_err("Failed to reconnect", window, cx, |_, _, _| None);
    }

    fn return_devcontainer_to_host(
        &self,
        options: &DockerConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let old_window = window.window_handle();

        let host = devcontainer_host_from_docker_host(&options.host);
        let paths = host_project_paths_from_settings(options, host.as_ref(), cx);
        let Some(paths) = paths else {
            drop(window.prompt(
                PromptLevel::Warning,
                "No host folder recorded",
                Some("No host folder is recorded for this dev container."),
                &["Ok"],
                cx,
            ));
            return;
        };

        if let Some(connection_options) = host_connection_options(host.as_ref()) {
            let app_state = workspace.read(cx).app_state().clone();
            cx.spawn_in(window, async move |_, cx| {
                open_remote_project(
                    connection_options,
                    paths,
                    app_state,
                    OpenOptions {
                        replace_window: None,
                        ..Default::default()
                    },
                    cx,
                )
                .await?;
                let _ = old_window.update(cx, |_, window, _| window.remove_window());
                Ok(())
            })
            .detach_and_prompt_err("Failed to return to host folder", window, cx, |_, _, _| None);
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

    fn return_devcontainer_to_host_on_disconnect(
        options: &DockerConnectionOptions,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let old_window = window.window_handle();
        let host = devcontainer_host_from_docker_host(&options.host);
        let Some(paths) = host_project_paths_from_settings(options, host.as_ref(), cx) else {
            drop(window.prompt(
                PromptLevel::Warning,
                "No host folder recorded",
                Some("No host folder is recorded for this dev container."),
                &["Ok"],
                cx,
            ));
            return true;
        };

        if let Some(connection_options) = host_connection_options(host.as_ref()) {
            let app_state = workspace.app_state().clone();
            cx.spawn_in(window, async move |_, cx| {
                open_remote_project(
                    connection_options,
                    paths,
                    app_state,
                    OpenOptions {
                        replace_window: None,
                        ..Default::default()
                    },
                    cx,
                )
                .await?;
                let _ = old_window.update(cx, |_, window, _| window.remove_window());
                Ok(())
            })
            .detach_and_prompt_err("Failed to return to host folder", window, cx, |_, _, _| None);
        } else {
            let workspace_handle = cx.entity();
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

        true
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent)
    }
}

impl Render for DisconnectedOverlay {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_reconnect = matches!(self.host, Host::RemoteServerProject(..));
        let can_return_to_host = matches!(
            self.host,
            Host::RemoteServerProject(RemoteConnectionOptions::Docker(_), _)
        );

        let message = match &self.host {
            Host::CollabGuestProject => {
                "Your connection to the remote project has been lost.".to_string()
            }
            Host::RemoteServerProject(options, server_not_running) => {
                let autosave = if ProjectSettings::get_global(cx)
                    .session
                    .restore_unsaved_buffers
                {
                    "\nUnsaved changes are stored locally."
                } else {
                    ""
                };
                let reason = if *server_not_running {
                    "process exiting unexpectedly"
                } else {
                    "not responding"
                };
                format!(
                    "Your connection to {} has been lost due to the server {reason}.{autosave}",
                    options.display_name(),
                )
            }
        };

        div()
            .track_focus(&self.focus_handle(cx))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .occlude()
            .w(rems(24.))
            .max_h(rems(40.))
            .child(
                Modal::new("disconnected", None)
                    .header(
                        ModalHeader::new()
                            .show_dismiss_button(true)
                            .child(Headline::new("Disconnected").size(HeadlineSize::Small)),
                    )
                    .section(Section::new().child(Label::new(message)))
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("close-window", "Close Window")
                                        .style(ButtonStyle::Filled)
                                        .layer(ElevationIndex::ModalSurface)
                                        .on_click(cx.listener(move |_, _, window, _| {
                                            window.remove_window();
                                        })),
                                )
                                .when(can_return_to_host, |el| {
                                    el.child(
                                        Button::new("return-to-host", "Return to Host Folder")
                                            .style(ButtonStyle::Filled)
                                            .layer(ElevationIndex::ModalSurface)
                                            .icon(IconName::ArrowLeft)
                                            .icon_position(IconPosition::Start)
                                            .on_click(cx.listener(Self::handle_return_to_host)),
                                    )
                                })
                                .when(can_reconnect, |el| {
                                    el.child(
                                        Button::new("reconnect", "Reconnect")
                                            .style(ButtonStyle::Filled)
                                            .layer(ElevationIndex::ModalSurface)
                                            .icon(IconName::ArrowCircle)
                                            .icon_position(IconPosition::Start)
                                            .on_click(cx.listener(Self::handle_reconnect)),
                                    )
                                }),
                        ),
                    ),
            )
    }
}

fn devcontainer_host_from_docker_host(host: &DockerHost) -> Option<DevContainerHost> {
    match host {
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
    }
}

fn host_project_paths_from_settings(
    options: &DockerConnectionOptions,
    host: Option<&DevContainerHost>,
    cx: &gpui::App,
) -> Option<Vec<PathBuf>> {
    RemoteSettings::get_global(cx)
        .dev_container_connections()
        .find_map(|connection| {
            if connection.container_id == options.container_id
                && connection.use_podman == options.use_podman
                && connection.host.as_ref() == host
            {
                connection
                    .host_projects
                    .iter()
                    .next()
                    .map(|project| project.paths.iter().map(PathBuf::from).collect())
            } else {
                None
            }
        })
}

fn host_connection_options(host: Option<&DevContainerHost>) -> Option<RemoteConnectionOptions> {
    match host? {
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
