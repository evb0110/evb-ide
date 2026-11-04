use std::{collections::HashMap, path::Path, sync::Arc};

use settings::{Settings, SettingsStore};

use gpui::{
    AnyWindowHandle, Context, InteractiveElement, MouseButton, ParentElement, ScrollHandle, Styled,
    SystemWindowTab, SystemWindowTabController, Window, WindowId, actions, div,
};

use theme_settings::ThemeSettings;
use ui::{
    Color, ContextMenu, DynamicSpacing, IconButton, IconButtonShape, IconName, IconSize, Label,
    LabelSize, Tab, Tooltip, h_flex, prelude::*, right_click_menu,
};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    CloseWindow, ItemSettings, MultiWorkspace, Workspace, WorkspaceSettings,
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

#[derive(Clone)]
pub struct DraggedWindowTab {
    pub id: WindowId,
    pub ix: usize,
    pub handle: AnyWindowHandle,
    pub title: SharedString,
}

/// A window tab labelled by the project open in it rather than by the window title, which also
/// names the active file.
struct ProjectTab {
    tab: SystemWindowTab,
    name: SharedString,
    /// The name of the folder containing the project, shown when another tab has the same name.
    location: Option<SharedString>,
    root_paths: Vec<Arc<Path>>,
}

impl ProjectTab {
    fn new(tab: SystemWindowTab, window: &Window, cx: &App) -> Self {
        // The window being rendered is not in `App::windows` while it draws, so its root view
        // comes from `window` instead of the handle.
        let multi_workspace = if tab.id == window.window_handle().window_id() {
            window.root::<MultiWorkspace>().flatten()
        } else {
            tab.handle
                .downcast::<MultiWorkspace>()
                .and_then(|handle| handle.entity(cx).ok())
        };
        let root_names_and_paths: Vec<(String, Arc<Path>)> = multi_workspace
            .map(|multi_workspace| {
                let project = multi_workspace
                    .read(cx)
                    .workspace()
                    .read(cx)
                    .project()
                    .read(cx);
                project
                    .visible_worktrees(cx)
                    .map(|worktree| {
                        let worktree = worktree.read(cx);
                        (worktree.root_name_str().to_string(), worktree.abs_path())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let name = if root_names_and_paths.is_empty() {
            tab.title.clone()
        } else {
            root_names_and_paths
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
                .into()
        };
        Self {
            tab,
            name,
            location: None,
            root_paths: root_names_and_paths
                .into_iter()
                .map(|(_, path)| path)
                .collect(),
        }
    }

    fn for_tabs(tabs: &[SystemWindowTab], window: &Window, cx: &App) -> Vec<Self> {
        let mut project_tabs: Vec<Self> = tabs
            .iter()
            .map(|tab| Self::new(tab.clone(), window, cx))
            .collect();
        let mut tabs_per_name = HashMap::<SharedString, usize>::new();
        for project_tab in &project_tabs {
            *tabs_per_name.entry(project_tab.name.clone()).or_default() += 1;
        }
        for project_tab in &mut project_tabs {
            if tabs_per_name
                .get(&project_tab.name)
                .is_some_and(|count| *count > 1)
            {
                project_tab.location = project_tab
                    .root_paths
                    .first()
                    .and_then(|path| path.parent())
                    .map(|parent| {
                        parent
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| parent.compact().to_string_lossy().into_owned())
                            .into()
                    });
            }
        }
        project_tabs
    }

    fn tooltip(&self) -> Option<SharedString> {
        if self.root_paths.is_empty() {
            return None;
        }
        Some(
            self.root_paths
                .iter()
                .map(|path| path.compact().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        )
    }
}

pub struct SystemWindowTabs {
    tab_bar_scroll_handle: ScrollHandle,
    last_dragged_tab: Option<DraggedWindowTab>,
    should_move_window: bool,
}

impl SystemWindowTabs {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.observe_global::<SystemWindowTabController>(|_, cx| cx.notify())
            .detach();

        Self {
            tab_bar_scroll_handle: ScrollHandle::new(),
            last_dragged_tab: None,
            should_move_window: false,
        }
    }

    pub fn init(cx: &mut App) {
        let mut was_use_system_window_tabs =
            WorkspaceSettings::get_global(cx).use_system_window_tabs;

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
            }

            // Only project windows are tabs of one another; Settings and other windows stay apart.
            cx.windows()
                .iter()
                .filter(|handle| handle.downcast::<MultiWorkspace>().is_some())
                .for_each(|handle| {
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
                        .log_err();
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
                        SystemWindowTabController::move_tab_to_new_window(
                            cx,
                            window.window_handle().window_id(),
                        );
                        window.move_tab_to_new_window();
                    })
                })
                .when(tab_groups.len() > 1, |div| {
                    div.on_action(move |_: &MergeAllWindows, window, cx| {
                        SystemWindowTabController::merge_all_windows(
                            cx,
                            window.window_handle().window_id(),
                        );
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
        project_tab: ProjectTab,
        next_tab_is_active: bool,
        tabs: Vec<SystemWindowTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let entity = cx.entity();
        let settings = ItemSettings::get_global(cx);
        let close_side = settings.close_position;
        let show_close_button = settings.show_close_button;
        let colors = cx.theme().colors();
        let border_color = colors.border;
        let hover_background = colors.ghost_element_hover;

        let tooltip = project_tab.tooltip();
        let ProjectTab {
            tab: item,
            name,
            location,
            ..
        } = project_tab;
        let is_active = window.window_handle().window_id() == item.id;

        let close_button = match show_close_button {
            ShowCloseButton::Hidden => None,
            _ => Some(
                IconButton::new(("close-window-tab", ix), IconName::Close)
                    .shape(IconButtonShape::Square)
                    .icon_color(Color::Muted)
                    .icon_size(IconSize::XSmall)
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close_window(item.handle, window, cx);
                    })
                    .map(|this| match show_close_button {
                        ShowCloseButton::Hover => this.visible_on_hover("tab"),
                        _ => this,
                    }),
            ),
        };
        let has_close_button = close_button.is_some();

        let label = h_flex()
            .min_w_0()
            .gap_1()
            .child(
                Label::new(name.clone())
                    .size(LabelSize::Small)
                    .truncate()
                    .color(if is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .when_some(location, |label, location| {
                label.child(
                    Label::new(location)
                        .size(LabelSize::XSmall)
                        .truncate()
                        .color(Color::Placeholder),
                )
            });

        let tab = h_flex()
            .id(ix)
            .group("tab")
            .relative()
            .flex_none()
            .h(Tab::container_height(cx))
            .max_w(rems(18.))
            .gap_1()
            .pl(DynamicSpacing::Base12.px(cx))
            .pr(if has_close_button {
                DynamicSpacing::Base04.px(cx)
            } else {
                DynamicSpacing::Base12.px(cx)
            })
            .when(
                has_close_button && close_side == ClosePosition::Left,
                |tab| {
                    tab.pl(DynamicSpacing::Base04.px(cx))
                        .pr(DynamicSpacing::Base12.px(cx))
                },
            )
            .cursor_pointer()
            .map(|tab| {
                if is_active {
                    tab.bg(colors.tab_bar_background)
                        .border_x_1()
                        .border_color(border_color)
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(px(2.))
                                .bg(colors.text_accent),
                        )
                } else {
                    tab.hover(|tab| tab.bg(hover_background))
                        .when(!next_tab_is_active, |tab| {
                            tab.child(
                                div()
                                    .absolute()
                                    .right_0()
                                    .top_2()
                                    .bottom_2()
                                    .w_px()
                                    .bg(border_color),
                            )
                        })
                }
            })
            .when_some(tooltip, |tab, tooltip| tab.tooltip(Tooltip::text(tooltip)))
            .on_drag(
                DraggedWindowTab {
                    id: item.id,
                    ix,
                    handle: item.handle,
                    title: name,
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
                cx.listener(move |this, dragged_tab: &DraggedWindowTab, _window, cx| {
                    this.last_dragged_tab = None;
                    Self::handle_tab_drop(dragged_tab, tab_ix, cx);
                })
            })
            .on_click(move |_, window, cx| {
                if item.handle.window_id() != window.window_handle().window_id() {
                    item.handle
                        .update(cx, |_, window, _| window.activate_window())
                        .log_err();
                }
            })
            .on_mouse_up(MouseButton::Middle, move |_, window, cx| {
                close_window(item.handle, window, cx);
            })
            .map(|tab| match (close_button, close_side) {
                (Some(close_button), ClosePosition::Left) => tab.child(close_button).child(label),
                (Some(close_button), ClosePosition::Right) => tab.child(label).child(close_button),
                (None, _) => tab.child(label),
            })
            .into_any();

        right_click_menu(ix)
            .trigger(|_, _, _| tab)
            .menu(move |window, cx| {
                let focus_handle = cx.focus_handle();
                let tabs = tabs.clone();
                let other_tabs = tabs.clone();
                let move_tabs = tabs.clone();
                let merge_tabs = tabs.clone();

                ContextMenu::build(window, cx, move |mut menu, _window_, _cx| {
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
                                SystemWindowTabController::move_tab_to_new_window(
                                    cx,
                                    window.window_handle().window_id(),
                                );
                                window.move_tab_to_new_window();
                            },
                        );
                    });

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
            })
    }

    fn handle_tab_drop(dragged_tab: &DraggedWindowTab, ix: usize, cx: &mut Context<Self>) {
        SystemWindowTabController::update_tab_position(cx, dragged_tab.id, ix);
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
                        .log_err();
                }
            }
        }
    }
}

fn close_window(handle: AnyWindowHandle, window: &mut Window, cx: &mut App) {
    if handle.window_id() == window.window_handle().window_id() {
        window.dispatch_action(Box::new(CloseWindow), cx);
    } else {
        handle
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(CloseWindow), cx);
            })
            .log_err();
    }
}

impl Render for SystemWindowTabs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let use_system_window_tabs = WorkspaceSettings::get_global(cx).use_system_window_tabs;

        let controller = cx.global::<SystemWindowTabController>();
        let visible = controller.is_visible();
        let current_window_tab = vec![SystemWindowTab::new(
            SharedString::from(window.window_title()),
            window.window_handle(),
        )];
        let tabs = controller
            .tabs(window.window_handle().window_id())
            .unwrap_or(&current_window_tab)
            .clone();

        if (!window.tab_bar_visible() && !visible) || (!use_system_window_tabs && tabs.len() <= 1) {
            return h_flex().into_any_element();
        }

        let active_window_id = window.window_handle().window_id();
        let project_tabs = ProjectTab::for_tabs(&tabs, window, cx);
        let next_tab_is_active: Vec<bool> = (0..project_tabs.len())
            .map(|ix| {
                project_tabs
                    .get(ix + 1)
                    .is_some_and(|next| next.tab.id == active_window_id)
            })
            .collect();
        let tab_items = project_tabs
            .into_iter()
            .zip(next_tab_is_active)
            .enumerate()
            .map(|(ix, (project_tab, next_tab_is_active))| {
                self.render_tab(
                    ix,
                    project_tab,
                    next_tab_is_active,
                    tabs.clone(),
                    window,
                    cx,
                )
            })
            .collect::<Vec<_>>();

        let colors = cx.theme().colors();
        let is_mac = cfg!(target_os = "macos");

        // The strip has the title bar's color, so the title bar and the project tabs read as one
        // piece of window chrome above the workspace's top border.
        h_flex()
            .w_full()
            .h(Tab::container_height(cx))
            .bg(colors.title_bar_background)
            // A tab dropped anywhere in the strip but on another tab stays where it was; only a
            // release outside the strip tears it off.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.last_dragged_tab = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    // A drag cancelled with Escape is no longer active by the time it is let go.
                    if let Some(tab) = this.last_dragged_tab.take()
                        && cx.has_active_drag()
                    {
                        SystemWindowTabController::move_tab_to_new_window(cx, tab.id);
                        if tab.id == window.window_handle().window_id() {
                            window.move_tab_to_new_window();
                        } else {
                            tab.handle
                                .update(cx, |_, window, _cx| {
                                    window.move_tab_to_new_window();
                                })
                                .log_err();
                        }
                    }
                }),
            )
            .child(
                h_flex()
                    .id("window tabs")
                    .h_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_bar_scroll_handle)
                    .children(tab_items),
            )
            .child(
                h_flex()
                    .flex_none()
                    .h_full()
                    .px(DynamicSpacing::Base04.px(cx))
                    .child(
                        IconButton::new("open-project-tab", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Open Recent Project in a New Tab"))
                            .on_click(|_event, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenRecent {
                                        create_new_window: Some(true),
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .child(
                div()
                    .id("window tabs filler")
                    .flex_1()
                    .h_full()
                    // The empty end of the strip behaves like the title bar above it.
                    .when(is_mac, |filler| {
                        filler
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, _| this.should_move_window = true),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, _| this.should_move_window = false),
                            )
                            .on_mouse_down_out(
                                cx.listener(|this, _, _, _| this.should_move_window = false),
                            )
                            .on_mouse_move(cx.listener(|this, _, window, _| {
                                if this.should_move_window {
                                    this.should_move_window = false;
                                    window.start_window_move();
                                }
                            }))
                            .on_click(|event, window, _| {
                                if event.click_count() == 2 {
                                    window.titlebar_double_click();
                                }
                            })
                    }),
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
        let colors = cx.theme().colors();

        h_flex()
            .h(Tab::container_height(cx))
            .max_w(rems(18.))
            .px(DynamicSpacing::Base12.px(cx))
            .bg(colors.tab_bar_background)
            .border_1()
            .border_color(colors.border)
            .rounded_sm()
            .shadow_sm()
            .font(ui_font)
            .child(
                Label::new(self.title.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
    }
}
