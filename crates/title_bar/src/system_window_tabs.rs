use settings::{Settings, SettingsStore};

use gpui::{
    AnyElement, AnyWindowHandle, App, Bounds, Context, Hsla, InteractiveElement, MouseButton,
    DragMoveEvent, ParentElement, ScrollHandle, Styled, SystemWindowTab, SystemWindowTabController,
    Window, WindowId, actions, canvas, div, point, size,
};

use theme::ThemeSettings;
use ui::{
    Color, ContextMenu, DynamicSpacing, IconButton, IconButtonShape, IconName, IconSize, Label,
    LabelSize, Tab, h_flex, prelude::*, right_click_menu,
};
use workspace::{
    CloseWindow, ItemSettings, Workspace, WorkspaceSettings,
    item::{ClosePosition, ShowCloseButton},
};

actions!(
    window,
    [
        ShowNextWindowTab,
        ShowPreviousWindowTab,
        MergeAllWindows,
        MoveTabToNewWindow
    ]
);

#[cfg(target_os = "windows")]
use gpui::{SystemWindowTabBarMetrics, SystemWindowTabDragPreview};

#[derive(Clone)]
pub struct DraggedWindowTab {
    pub id: WindowId,
    pub ix: usize,
    pub handle: AnyWindowHandle,
    pub title: String,
    pub width: Pixels,
    pub is_active: bool,
    pub active_background_color: Hsla,
    pub inactive_background_color: Hsla,
}

pub struct SystemWindowTabs {
    tab_bar_scroll_handle: ScrollHandle,
    measured_tab_width: Pixels,
    last_dragged_tab: Option<DraggedWindowTab>,
}

impl SystemWindowTabs {
    pub fn new() -> Self {
        Self {
            tab_bar_scroll_handle: ScrollHandle::new(),
            measured_tab_width: px(0.),
            last_dragged_tab: None,
        }
    }

    pub fn init(cx: &mut App) {
        let mut was_use_system_window_tabs =
            WorkspaceSettings::get_global(cx).use_system_window_tabs;

        // Initialize on startup if setting is already enabled
        if was_use_system_window_tabs {
            SystemWindowTabController::init(cx);
            #[cfg(target_os = "windows")]
            SystemWindowTabController::set_visible(cx, true);
        }

        cx.observe_global::<SettingsStore>(move |cx| {
            let use_system_window_tabs = WorkspaceSettings::get_global(cx).use_system_window_tabs;
            if use_system_window_tabs == was_use_system_window_tabs {
                return;
            }
            was_use_system_window_tabs = use_system_window_tabs;

            let tabbing_identifier = if use_system_window_tabs {
                Some(String::from("zed"))
            } else {
                None
            };

            if use_system_window_tabs {
                SystemWindowTabController::init(cx);
                // On Windows, we need to explicitly set visibility since
                // there's no native tab bar to query
                #[cfg(target_os = "windows")]
                SystemWindowTabController::set_visible(cx, true);
            }

            cx.windows().iter().for_each(|handle| {
                handle
                    .update(cx, |_, window, cx| {
                    window.set_tabbing_identifier(tabbing_identifier.clone());
                    if use_system_window_tabs {
                        let tabs = if let Some(tabs) = window.tabbed_windows() {
                            tabs
                        } else {
                            vec![SystemWindowTab::new(
                                SharedString::from(window.window_title()),
                                window.window_handle(),
                            )]
                        };

                        SystemWindowTabController::add_tab(cx, handle.window_id(), tabs);
                    }
                })
                    .ok();
            });
        })
        .detach();

        cx.observe_new(|workspace: &mut Workspace, _, _| {
            workspace.register_action_renderer(|div, _, window, cx| {
                let window_id = window.window_handle().window_id();
                let controller = cx.global::<SystemWindowTabController>();

                let tab_groups = controller.tab_groups();
                let tabs = controller.tabs(window_id);
                let Some(tabs) = tabs else {
                    return div;
                };

                div.when(tabs.len() > 1, |div| {
                    div.on_action(move |_: &ShowNextWindowTab, window, cx| {
                        SystemWindowTabController::select_next_tab(
                            cx,
                            window.window_handle().window_id(),
                        );
                    })
                    .on_action(move |_: &ShowPreviousWindowTab, window, cx| {
                        SystemWindowTabController::select_previous_tab(
                            cx,
                            window.window_handle().window_id(),
                        );
                    })
                    .on_action(move |_: &MoveTabToNewWindow, window, cx| {
                        #[cfg(target_os = "windows")]
                        {
                            SystemWindowTabController::move_tab_to_new_window(
                                cx,
                                window.window_handle().window_id(),
                            );
                            SystemWindowTabController::refresh_all_windows(cx);
                        }

                        window.move_tab_to_new_window();
                    })
                })
                .when(tab_groups.len() > 1, |div| {
                    div.on_action(move |_: &MergeAllWindows, window, cx| {
                        #[cfg(target_os = "windows")]
                        {
                            SystemWindowTabController::merge_all_windows(
                                cx,
                                window.window_handle().window_id(),
                            );
                            SystemWindowTabController::refresh_all_windows(cx);
                        }

                        window.merge_all_windows();
                    })
                })
            });
        })
        .detach();
    }

    fn render_tab(
        &self,
        ix: usize,
        item: SystemWindowTab,
        tabs: Vec<SystemWindowTab>,
        active_background_color: Hsla,
        inactive_background_color: Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let entity = cx.entity();
        let settings = ItemSettings::get_global(cx);
        let close_side = &settings.close_position;
        let show_close_button = &settings.show_close_button;

        let rem_size = window.rem_size();
        let width = self.measured_tab_width.max(rem_size * 10);
        let is_active = window.window_handle().window_id() == item.id;
        let title = item.title.to_string();

        let label = Label::new(&title)
            .size(LabelSize::Small)
            .truncate()
            .color(if is_active {
                Color::Default
            } else {
                Color::Muted
            });

        let tab = h_flex()
            .id(ix)
            .group("tab")
            .w_full()
            .overflow_hidden()
            .h(Tab::content_height(cx))
            .relative()
            .px(DynamicSpacing::Base16.px(cx))
            .justify_center()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .cursor_pointer()
            .on_drag(
                DraggedWindowTab {
                    id: item.id,
                    ix,
                    handle: item.handle,
                    title: item.title.to_string(),
                    width,
                    is_active,
                    active_background_color,
                    inactive_background_color,
                },
                move |tab, _, _, cx| {
                    entity.update(cx, |this, _cx| {
                        this.last_dragged_tab = Some(tab.clone());
                    });
                    cx.new(|_| tab.clone())
                },
            )
            .drag_over::<DraggedWindowTab>({
                let tab_ix = ix;
                move |element, dragged_tab: &DraggedWindowTab, _, cx| {
                    let mut styled_tab = element
                        .bg(cx.theme().colors().drop_target_background)
                        .border_color(cx.theme().colors().drop_target_border)
                        .border_0();

                    if tab_ix < dragged_tab.ix {
                        styled_tab = styled_tab.border_l_2();
                    } else if tab_ix > dragged_tab.ix {
                        styled_tab = styled_tab.border_r_2();
                    }

                    styled_tab
                }
            })
            .on_drop({
                let tab_ix = ix;
                let target_window_id = item.id;
                cx.listener(move |this, dragged_tab: &DraggedWindowTab, window, cx| {
                    this.last_dragged_tab = None;
                    #[cfg(target_os = "windows")]
                    Self::clear_drag_preview(cx);
                    #[cfg(target_os = "windows")]
                    {
                        let same_group = cx
                            .global::<SystemWindowTabController>()
                            .tabs(target_window_id)
                            .is_some_and(|tabs| tabs.iter().any(|tab| tab.id == dragged_tab.id));

                        if same_group {
                            Self::handle_tab_drop(dragged_tab, tab_ix, target_window_id, window, cx);
                        } else {
                            let dragged_tab = dragged_tab.clone();
                            cx.defer(move |cx| {
                                Self::merge_tab_into_target_group(
                                    cx,
                                    dragged_tab,
                                    target_window_id,
                                    tab_ix,
                                );
                            });
                        }
                        return;
                    }

                    #[cfg(not(target_os = "windows"))]
                    Self::handle_tab_drop(dragged_tab, tab_ix, target_window_id, window, cx);
                })
            })
            .on_click(move |_, _, cx| {
                item.handle
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            })
            .on_mouse_up(MouseButton::Middle, move |_, window, cx| {
                if item.handle.window_id() == window.window_handle().window_id() {
                    window.dispatch_action(Box::new(CloseWindow), cx);
                } else {
                    item.handle
                        .update(cx, |_, window, cx| {
                            window.dispatch_action(Box::new(CloseWindow), cx);
                        })
                        .ok();
                }
            })
            .child(label)
            .map(|this| match show_close_button {
                ShowCloseButton::Hidden => this,
                _ => this.child(
                    div()
                        .absolute()
                        .top_2()
                        .w_4()
                        .h_4()
                        .map(|this| match close_side {
                            ClosePosition::Left => this.left_1(),
                            ClosePosition::Right => this.right_1(),
                        })
                        .child(
                            IconButton::new("close", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_color(Color::Muted)
                                .icon_size(IconSize::XSmall)
                                .on_click({
                                    move |_, window, cx| {
                                        if item.handle.window_id()
                                            == window.window_handle().window_id()
                                        {
                                            window.dispatch_action(Box::new(CloseWindow), cx);
                                        } else {
                                            item.handle
                                                .update(cx, |_, window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(CloseWindow),
                                                        cx,
                                                    );
                                                })
                                                .ok();
                                        }
                                    }
                                })
                                .map(|this| match show_close_button {
                                    ShowCloseButton::Hover => this.visible_on_hover("tab"),
                                    _ => this,
                                }),
                        ),
                ),
            })
            .into_any();

        let menu = right_click_menu(ix)
            .trigger(|_, _, _| tab)
            .menu(move |window, cx| {
                let focus_handle = cx.focus_handle();
                let tabs = tabs.clone();
                let other_tabs = tabs.clone();
                let move_tabs = tabs.clone();
                let merge_tabs = tabs.clone();
                let merge_all_tabs = tabs.clone();

                ContextMenu::build(window, cx, move |mut menu, _window_, cx| {
                    menu = menu.entry("Close Tab", None, move |window, cx| {
                        Self::handle_right_click_action(
                            cx,
                            window,
                            &tabs,
                            |tab| tab.id == item.id,
                            |window, cx| {
                                window.dispatch_action(Box::new(CloseWindow), cx);
                            },
                        );
                    });

                    menu = menu.entry("Close Other Tabs", None, move |window, cx| {
                        Self::handle_right_click_action(
                            cx,
                            window,
                            &other_tabs,
                            |tab| tab.id != item.id,
                            |window, cx| {
                                window.dispatch_action(Box::new(CloseWindow), cx);
                            },
                        );
                    });

                    menu = menu.entry("Move Tab to New Window", None, move |window, cx| {
                        Self::handle_right_click_action(
                            cx,
                            window,
                            &move_tabs,
                            |tab| tab.id == item.id,
                            |window, cx| {
                                #[cfg(target_os = "windows")]
                                {
                                    SystemWindowTabController::move_tab_to_new_window(
                                        cx,
                                        window.window_handle().window_id(),
                                    );
                                    SystemWindowTabController::refresh_all_windows(cx);
                                }

                                window.move_tab_to_new_window();
                            },
                        );
                    });

                    // Add "Merge All Windows" when there are multiple tab groups
                    let controller = cx.global::<SystemWindowTabController>();
                    if controller.tab_groups_count() > 1 {
                        menu = menu.entry("Merge All Windows", None, move |window, cx| {
                            Self::handle_right_click_action(
                                cx,
                                window,
                                &merge_all_tabs,
                                |tab| tab.id == item.id,
                                |window, cx| {
                                    #[cfg(target_os = "windows")]
                                    {
                                        SystemWindowTabController::merge_all_windows(
                                            cx,
                                            window.window_handle().window_id(),
                                        );
                                        SystemWindowTabController::refresh_all_windows(cx);
                                    }

                                    window.merge_all_windows();
                                },
                            );
                        });
                    }

                    menu = menu.entry("Show All Tabs", None, move |window, cx| {
                        Self::handle_right_click_action(
                            cx,
                            window,
                            &merge_tabs,
                            |tab| tab.id == item.id,
                            |window, _cx| {
                                window.toggle_window_tab_overview();
                            },
                        );
                    });

                    menu.context(focus_handle)
                })
            });

        div()
            .flex_1()
            .min_w(rem_size * 10)
            .when(is_active, |this| this.bg(active_background_color))
            .border_t_1()
            .border_color(if is_active {
                active_background_color
            } else {
                cx.theme().colors().border
            })
            .child(menu)
    }

    fn handle_tab_drop(
        dragged_tab: &DraggedWindowTab,
        ix: usize,
        target_window_id: WindowId,
        _target_window: &mut Window,
        cx: &mut App,
    ) {
        let controller = cx.global::<SystemWindowTabController>();
        let same_group = controller
            .tabs(target_window_id)
            .is_some_and(|tabs| tabs.iter().any(|tab| tab.id == dragged_tab.id));

        if same_group {
            let controller = cx.global::<SystemWindowTabController>();
            let clamped_ix = controller
                .tabs(target_window_id)
                .map(|tabs| ix.min(tabs.len().saturating_sub(1)))
                .unwrap_or(ix);

            SystemWindowTabController::update_tab_position(cx, dragged_tab.id, clamped_ix);
            return;
        }
    }

    #[cfg(target_os = "windows")]
    fn merge_tab_into_target_group(
        cx: &mut App,
        dragged_tab: DraggedWindowTab,
        target_window_id: WindowId,
        ix: usize,
    ) {
        let Some(target_handle) = cx
            .windows()
            .into_iter()
            .find(|h| h.window_id() == target_window_id)
        else {
            return;
        };

        let Ok((target_identifier, target_hwnd)) =
            target_handle.update(cx, |_, target_window, _| {
                (target_window.tabbing_identifier(), target_window.raw_handle())
            })
        else {
            return;
        };

        let Some(target_identifier) = target_identifier else {
            return;
        };

        // Perform platform merge by updating the dragged window (no nested window updates).
        if dragged_tab.id != target_window_id {
            if dragged_tab
                .handle
                .update(cx, |_, dragged_window, _| {
                    dragged_window.merge_into_tabbing_group(target_identifier.clone(), target_hwnd);
                })
                .is_err()
            {
                return;
            }
        } else {
            // Dragging the active tab "into itself" is a no-op.
            return;
        }

        // Snapshot the target group after the platform coordinator updates.
        let Ok(tabs) = target_handle.update(cx, |_, target_window, _| target_window.tabbed_windows())
        else {
            return;
        };
        let Some(tabs) = tabs else {
            return;
        };

        // Update controller to match the platform grouping.
        SystemWindowTabController::remove_tab(cx, dragged_tab.id);
        SystemWindowTabController::add_tab(cx, target_window_id, tabs);
        let controller = cx.global::<SystemWindowTabController>();
        let clamped_ix = controller
            .tabs(target_window_id)
            .map(|tabs| ix.min(tabs.len().saturating_sub(1)))
            .unwrap_or(ix);
        SystemWindowTabController::update_tab_position(cx, dragged_tab.id, clamped_ix);
        SystemWindowTabController::refresh_all_windows(cx);
    }

    #[cfg(target_os = "windows")]
    fn clear_drag_preview(cx: &mut App) {
        let previous = cx.global::<SystemWindowTabController>().drag_preview();
        if SystemWindowTabController::set_drag_preview(cx, None) {
            if let Some(previous) = previous {
                Self::refresh_window_ids(cx, [previous.target_window_id]);
            } else {
                SystemWindowTabController::refresh_all_windows(cx);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn refresh_window_ids(cx: &mut App, window_ids: impl IntoIterator<Item = WindowId>) {
        let ids = window_ids.into_iter().collect::<std::collections::HashSet<_>>();
        for handle in cx.windows() {
            if ids.contains(&handle.window_id()) {
                handle.update(cx, |_, window, _| window.refresh()).ok();
            }
        }
    }

    fn handle_right_click_action<F, P>(
        cx: &mut App,
        window: &mut Window,
        tabs: &Vec<SystemWindowTab>,
        predicate: P,
        mut action: F,
    ) where
        P: Fn(&SystemWindowTab) -> bool,
        F: FnMut(&mut Window, &mut App),
    {
        for tab in tabs {
            if predicate(tab) {
                if tab.id == window.window_handle().window_id() {
                    action(window, cx);
                } else {
                    tab.handle
                        .update(cx, |_view, window, cx| {
                            action(window, cx);
                        })
                        .ok();
                }
            }
        }
    }
}

impl Render for SystemWindowTabs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let use_system_window_tabs = WorkspaceSettings::get_global(cx).use_system_window_tabs;
        let active_background_color = cx.theme().colors().title_bar_background;
        let inactive_background_color = cx.theme().colors().tab_bar_background;
        let entity = cx.entity();

        let window_id = window.window_handle().window_id();
        let visible = cx.global::<SystemWindowTabController>().is_visible();
        let current_window_tab = vec![SystemWindowTab::new(
            SharedString::from(window.window_title()),
            window.window_handle(),
        )];
        let tabs = cx
            .global::<SystemWindowTabController>()
            .tabs(window_id)
            .unwrap_or(&current_window_tab)
            .clone();

        let tab_width = self.measured_tab_width.max(window.rem_size() * 10.);

        #[cfg(target_os = "windows")]
        {
            SystemWindowTabController::set_tab_bar_metrics(
                cx,
                window_id,
                SystemWindowTabBarMetrics {
                    tab_width,
                    scroll_offset_x: self.tab_bar_scroll_handle.offset().x,
                    tab_count: tabs.len().max(1),
                },
            );
        }

        #[cfg(target_os = "windows")]
        let drag_preview = cx.global::<SystemWindowTabController>().drag_preview();

        let mut tab_items = tabs
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                self.render_tab(
                    ix,
                    item.clone(),
                    tabs.clone(),
                    active_background_color,
                    inactive_background_color,
                    window,
                    cx,
                )
                .into_any_element()
            })
            .collect::<Vec<AnyElement>>();

        #[cfg(target_os = "windows")]
        if let Some(preview) = drag_preview
            && preview.target_window_id == window_id
            && cx.has_active_drag()
        {
            let placeholder = h_flex()
                .h(Tab::container_height(cx))
                .w(tab_width)
                .bg(cx.theme().colors().drop_target_background)
                .border_1()
                .border_color(cx.theme().colors().drop_target_border)
                .opacity(0.55)
                .into_any_element();

            let insert_ix = preview.insert_ix.min(tab_items.len());
            tab_items.insert(insert_ix, placeholder);
        }

        let number_of_tabs = tabs.len().max(1);
        if (!window.tab_bar_visible() && !visible)
            || (!use_system_window_tabs && number_of_tabs == 1)
        {
            return h_flex().into_any_element();
        }

        let tab_bar = h_flex()
            .w_full()
            .h(Tab::container_height(cx))
            .bg(inactive_background_color);

        #[cfg(target_os = "windows")]
        let tab_bar = tab_bar.on_drag_move::<DraggedWindowTab>(cx.listener(
            |this, event: &DragMoveEvent<DraggedWindowTab>, window, cx| {
                    let Some(dragged_tab) = event.dragged_item().downcast_ref::<DraggedWindowTab>()
                    else {
                        return;
                    };

                    let global_pos = window.bounds().origin + event.event.position;
                    let mut next_preview: Option<SystemWindowTabDragPreview> = None;

                    // Prefer top-most windows when multiple overlap.
                    let windows = cx.window_stack().unwrap_or_else(|| cx.windows());
                    for handle in windows {
                        let Ok((target_window_id, bounds)) =
                            handle.update(cx, |_, target_window, _| {
                                (target_window.window_handle().window_id(), target_window.bounds())
                            })
                        else {
                            continue;
                        };

                        // Only preview within the window tab bar area.
                        // On Windows, `PlatformTitleBar::height` is currently fixed at 32px.
                        let tab_bar_bounds = Bounds::new(
                            point(bounds.origin.x, bounds.origin.y + px(32.)),
                            size(bounds.size.width, Tab::container_height(cx)),
                        );

                        if !tab_bar_bounds.contains(&global_pos) {
                            continue;
                        }

                        let (tab_width, scroll_offset_x, tab_count) = {
                            let controller = cx.global::<SystemWindowTabController>();
                            let tab_count = controller
                                .tabs(target_window_id)
                                .map(|tabs| tabs.len())
                                .unwrap_or(1);
                            let metrics = controller.tab_bar_metrics(target_window_id).copied();
                            let tab_width = metrics.map(|m| m.tab_width).unwrap_or(px(0.));
                            let scroll_offset_x =
                                metrics.map(|m| m.scroll_offset_x).unwrap_or(px(0.));
                            (tab_width, scroll_offset_x, tab_count)
                        };

                        let insert_ix = if tab_width > px(0.) {
                            let local_x = (global_pos.x - bounds.origin.x) + scroll_offset_x;
                            ((local_x / tab_width).floor() as usize).min(tab_count)
                        } else {
                            tab_count
                        };

                        next_preview = Some(SystemWindowTabDragPreview {
                            target_window_id,
                            insert_ix,
                        });
                        break;
                    }

                    let previous = cx.global::<SystemWindowTabController>().drag_preview();
                    if SystemWindowTabController::set_drag_preview(cx, next_preview) {
                        // Only refresh windows that could visually change (previous target, new target,
                        // and the source window rendering the drag).
                        let mut to_refresh = Vec::new();
                        if let Some(prev) = previous {
                            to_refresh.push(prev.target_window_id);
                        }
                        if let Some(next) = next_preview {
                            to_refresh.push(next.target_window_id);
                        }
                        to_refresh.push(window.window_handle().window_id());
                        Self::refresh_window_ids(cx, to_refresh);
                    }

                    // `on_mouse_up_out` uses this to decide whether to detach.
                    if this.last_dragged_tab.is_none() {
                        this.last_dragged_tab = Some(dragged_tab.clone());
                    }
            },
        ));

        tab_bar
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    if let Some(tab) = this.last_dragged_tab.take() {
                        #[cfg(target_os = "windows")]
                        {
                            let preview = cx.global::<SystemWindowTabController>().drag_preview();
                            Self::clear_drag_preview(cx);

                            if let Some(preview) = preview
                                && cx
                                    .windows()
                                    .into_iter()
                                    .any(|h| h.window_id() == preview.target_window_id)
                            {
                                let tab_for_merge = tab;
                                cx.defer(move |cx| {
                                    Self::merge_tab_into_target_group(
                                        cx,
                                        tab_for_merge,
                                        preview.target_window_id,
                                        preview.insert_ix,
                                    );
                                });
                                return;
                            }

                            // No valid merge target: detach.
                            SystemWindowTabController::move_tab_to_new_window(cx, tab.id);
                            SystemWindowTabController::refresh_all_windows(cx);
                        }

                        // Perform the platform operation to actually detach into a new group.
                        if tab.id == window.window_handle().window_id() {
                            window.move_tab_to_new_window();
                        } else {
                            tab.handle
                                .update(cx, |_, window, _cx| window.move_tab_to_new_window())
                                .ok();
                        }
                    }
                }),
            )
            .child(
                h_flex()
                    .id("window tabs")
                    .w_full()
                    .h(Tab::container_height(cx))
                    .bg(inactive_background_color)
                    .drag_over::<DraggedWindowTab>(move |element, _dragged_tab, _, cx| {
                        element
                            .bg(cx.theme().colors().drop_target_background)
                            .border_color(cx.theme().colors().drop_target_border)
                            .border_0()
                    })
                    .on_drop({
                        let target_window_id = window.window_handle().window_id();
                        cx.listener(move |this, dragged_tab: &DraggedWindowTab, window, cx| {
                            this.last_dragged_tab = None;
                            #[cfg(target_os = "windows")]
                            Self::clear_drag_preview(cx);
                            #[cfg(target_os = "windows")]
                            {
                                let same_group = cx
                                    .global::<SystemWindowTabController>()
                                    .tabs(target_window_id)
                                    .is_some_and(|tabs| tabs.iter().any(|tab| tab.id == dragged_tab.id));

                                if same_group {
                                    Self::handle_tab_drop(
                                        dragged_tab,
                                        usize::MAX,
                                        target_window_id,
                                        window,
                                        cx,
                                    );
                                    return;
                                }

                                let dragged_tab = dragged_tab.clone();
                                cx.defer(move |cx| {
                                    Self::merge_tab_into_target_group(
                                        cx,
                                        dragged_tab,
                                        target_window_id,
                                        usize::MAX,
                                    );
                                });
                                return;
                            }

                            #[cfg(not(target_os = "windows"))]
                            Self::handle_tab_drop(dragged_tab, usize::MAX, target_window_id, window, cx);
                        })
                    })
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_bar_scroll_handle)
                    .children(tab_items)
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, _, cx| {
                                let entity = entity.clone();
                                entity.update(cx, |this, cx| {
                                    let width = bounds.size.width / number_of_tabs as f32;
                                    if width != this.measured_tab_width {
                                        this.measured_tab_width = width;
                                        cx.notify();
                                    }
                                });
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .child(
                h_flex()
                    .h_full()
                    .px(DynamicSpacing::Base06.rems(cx))
                    .border_t_1()
                    .border_l_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        IconButton::new("plus", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(|_event, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenRecent {
                                        create_new_window: true,
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for DraggedWindowTab {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let ui_font = ThemeSettings::get_global(cx).ui_font.clone();
        let label = Label::new(self.title.clone())
            .size(LabelSize::Small)
            .truncate()
            .color(if self.is_active {
                Color::Default
            } else {
                Color::Muted
            });

        h_flex()
            .h(Tab::container_height(cx))
            .w(self.width)
            .px(DynamicSpacing::Base16.px(cx))
            .justify_center()
            .bg(if self.is_active {
                self.active_background_color
            } else {
                self.inactive_background_color
            })
            .border_1()
            .border_color(cx.theme().colors().border)
            .font(ui_font)
            .child(label)
    }
}
