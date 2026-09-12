//! Linux 客户端侧装饰（CSD）支持。
//!
//! `Settings::window_decoration` 控制策略：
//! - `Auto`：请求 SSD（KWin 等会绘制系统标题栏）；合成器/WM 回退到 CSD 时自动
//!   补画应用标题栏。
//! - `Always`：无视合成器声称的装饰模式，无条件绘制应用标题栏 —— 用于 WM 报告
//!   SSD 但实际不绘制标题栏（GNOME/Mutter、部分 KWin 配置等）导致无法关闭窗口的环境。
//!
//! 阴影环、1px 边框与边缘调整大小命中区不需要在这里处理：gpui-component 的 `Root`
//! 默认 `bordered(true)`，已在 CSD 模式下用 `window_border()` 包裹整个视图，并在
//! SSD 模式下自动退化为无操作，因此这里只补标题栏本身，避免双层 padding。

use gpui::prelude::*;
use gpui::*;
use gpui_base::InteractiveElementExt as _;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::settings::WindowDecoration;

const TITLEBAR_HEIGHT: Pixels = px(38.);
const CONTROL_WIDTH: Pixels = px(46.);
const CONTROL_ICON_SIZE: Pixels = px(16.);

/// 按装饰策略决定是否（以及为何）绘制应用标题栏。
fn should_draw_titlebar(decoration: WindowDecoration, decorations: Decorations) -> bool {
    match decoration {
        WindowDecoration::Always => true,
        WindowDecoration::Auto => matches!(decorations, Decorations::Client { .. }),
    }
}

struct DragState {
    should_move: bool,
}

impl Render for DragState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 单个窗口控制按钮。图标颜色由调用方给出（与主题深浅反相以保证可见）。
fn control_button(
    id: &'static str,
    icon: IconName,
    foreground: Hsla,
    hover_background: Hsla,
    hover_foreground: Hsla,
    active_background: Hsla,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .w(CONTROL_WIDTH)
        .h_full()
        .flex_shrink_0()
        .justify_center()
        .content_center()
        .items_center()
        .text_color(foreground)
        .hover(|style| style.bg(hover_background).text_color(hover_foreground))
        .active(|style| style.bg(active_background).text_color(hover_foreground))
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_click(window, cx);
        })
        .child(Icon::new(icon).with_size(CONTROL_ICON_SIZE))
        .into_any_element()
}

/// 若需要 CSD，则在内容顶部加上自绘标题栏；否则原样返回。
pub fn decorate(
    inner: AnyElement,
    decoration: WindowDecoration,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if !should_draw_titlebar(decoration, window.window_decorations()) {
        return inner;
    }

    let theme = cx.theme().clone();
    // 按钮与深浅模式反相：深色主题用亮图标，浅色主题用暗图标，
    // 避免图标颜色与标题栏底色相同而看不见。
    let is_dark = theme.is_dark();
    let icon_color = if is_dark { white() } else { black() };
    let hover_background = if is_dark {
        hsla(0., 0., 1., 0.12)
    } else {
        hsla(0., 0., 0., 0.08)
    };
    let active_background = if is_dark {
        hsla(0., 0., 1., 0.2)
    } else {
        hsla(0., 0., 0., 0.16)
    };
    let close_hover_foreground = white();

    let drag_state = window.use_state(cx, |_, _| DragState {
        should_move: false,
    });

    let maximize_icon = if window.is_maximized() {
        IconName::WindowRestore
    } else {
        IconName::WindowMaximize
    };

    v_flex()
        .id("csd-frame")
        .size_full()
        .bg(theme.background)
        .child(
            h_flex()
                .id("csd-titlebar")
                .w_full()
                .h(TITLEBAR_HEIGHT)
                .flex_shrink_0()
                .items_center()
                .bg(theme.title_bar)
                .border_b_1()
                .border_color(theme.title_bar_border)
                .on_double_click(|_, window, _| window.zoom_window())
                .on_mouse_down(
                    MouseButton::Right,
                    |event: &MouseDownEvent, window, _| window.show_window_menu(event.position),
                )
                .on_mouse_down_out(window.listener_for(&drag_state, |state, _, _, _| {
                    state.should_move = false;
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    window.listener_for(&drag_state, |state, _, _, _| {
                        state.should_move = true;
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    window.listener_for(&drag_state, |state, _, _, _| {
                        state.should_move = false;
                    }),
                )
                .on_mouse_move(window.listener_for(&drag_state, |state, _, window, _| {
                    if state.should_move {
                        state.should_move = false;
                        window.start_window_move();
                    }
                }))
                // 左侧留空作为拖拽区，不显示标题文字。
                .child(div().flex_1().h_full())
                .child(control_button(
                    "csd-minimize",
                    IconName::WindowMinimize,
                    icon_color,
                    hover_background,
                    icon_color,
                    active_background,
                    |window, _| window.minimize_window(),
                ))
                .child(control_button(
                    "csd-maximize",
                    maximize_icon,
                    icon_color,
                    hover_background,
                    icon_color,
                    active_background,
                    |window, _| {
                        window.zoom_window();
                        // 最大化状态变化后立刻重画，切换 □/restore 图标。
                        window.refresh();
                    },
                ))
                .child(control_button(
                    "csd-close",
                    IconName::WindowClose,
                    icon_color,
                    theme.danger,
                    close_hover_foreground,
                    theme.danger_active,
                    |window, _| window.remove_window(),
                )),
        )
        .child(div().flex_1().min_h_0().overflow_hidden().child(inner))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::should_draw_titlebar;
    use crate::settings::WindowDecoration;
    use gpui::Decorations;

    #[test]
    fn always_mode_ignores_reported_server_decorations() {
        // 回归：Mutter / 部分 KWin 配置报告 SSD 但不绘制标题栏，此时仍必须给出
        // 可点击的关闭窗口按钮。
        assert!(should_draw_titlebar(
            WindowDecoration::Always,
            Decorations::Server
        ));
        assert!(should_draw_titlebar(
            WindowDecoration::Always,
            Decorations::Client {
                tiling: Default::default(),
            }
        ));
    }

    #[test]
    fn auto_mode_only_draws_when_compositor_gives_us_csd() {
        assert!(!should_draw_titlebar(
            WindowDecoration::Auto,
            Decorations::Server
        ));
        assert!(should_draw_titlebar(
            WindowDecoration::Auto,
            Decorations::Client {
                tiling: Default::default(),
            }
        ));
    }
}
