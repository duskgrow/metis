//! Probe native menu, popover and disclosure boundaries with real components.

#![cfg(feature = "workbench")]

#[path = "../src/workbench/disclosure.rs"]
mod disclosure;

use gpui_kit::{
    Anchor, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Pixels, Point, Role,
    ScrollDelta, ScrollHandle, Size, TestAppContext, VisualTestContext, Window, WindowBounds,
    WindowHandle, WindowOptions, actions,
    base::Root,
    component::{accordion::Accordion, button::Button, menu::DropdownMenu as _, popover::Popover},
    div, point,
    prelude::*,
    px, size,
    test::{TestSupportExt as _, TestWindowExt as _},
};

actions!(
    workbench_popups,
    [
        SelectModelA,
        SelectModelB,
        SelectEffortLow,
        SelectEffortHigh
    ]
);

fn open_view<V: Render>(
    cx: &mut TestAppContext,
    bounds: Size<Pixels>,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> (WindowHandle<Root>, Entity<V>) {
    cx.update(|cx| {
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: bounds,
                })),
                ..Default::default()
            },
            cx,
            build,
        )
        .expect("open popup probe window");
        (window.downcast::<Root>().expect("Base Root"), view)
    })
}

fn visual(handle: WindowHandle<Root>, cx: &TestAppContext) -> VisualTestContext {
    VisualTestContext::from_window(handle.into(), cx)
}

fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
    });
    cx.run_until_parked();
}

struct ModelEffort {
    model: &'static str,
    effort: &'static str,
    focus: FocusHandle,
}

impl Render for ModelEffort {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model;
        let effort = self.effort;
        let action_context = self.focus.clone();
        div()
            .size_full()
            .p_4()
            .on_action(cx.listener(|this, _: &SelectModelA, _, cx| {
                this.model = "Model A";
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectModelB, _, cx| {
                this.model = "Model B";
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectEffortLow, _, cx| {
                this.effort = "Low";
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectEffortHigh, _, cx| {
                this.effort = "High";
                cx.notify();
            }))
            .child(
                Button::new("model-effort")
                    .label(format!("{model} + {effort}"))
                    .track_focus(&self.focus)
                    .dropdown_menu(move |menu, window, cx| {
                        menu.action_context(action_context.clone())
                            .submenu("Model", window, cx, move |menu, _, _| {
                                menu.menu_with_check(
                                    "Model A",
                                    model == "Model A",
                                    Box::new(SelectModelA),
                                )
                                .menu_with_check(
                                    "Model B",
                                    model == "Model B",
                                    Box::new(SelectModelB),
                                )
                            })
                            .submenu("Effort", window, cx, move |menu, _, _| {
                                menu.menu_with_check(
                                    "Low",
                                    effort == "Low",
                                    Box::new(SelectEffortLow),
                                )
                                .menu_with_check(
                                    "High",
                                    effort == "High",
                                    Box::new(SelectEffortHigh),
                                )
                            })
                    }),
            )
    }
}

#[gpui_kit::test]
fn workbench_popups_model_effort_menu_maps_checked_submenus_keyboard_and_focus(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view(cx, size(px(640.), px(480.)), |_, cx| {
        cx.new(|cx| ModelEffort {
            model: "Model B",
            effort: "High",
            focus: cx.focus_handle(),
        })
    });
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    cx.update(|window, cx| {
        window.focus_next(cx);
        window.render_frame(cx);
        assert_eq!(window.find("model-effort").focused(), Some(true));
        assert_eq!(window.find("model-effort").label(), Some("Model B + High"));
        window.press("enter", cx);
        assert_eq!(window.find("popup-menu").focused(), Some(true));
        let mut menu = window.within("popup-menu");
        menu.press("down", cx);
        menu.press("right", cx);
        let mut submenu = window.within("submenu");
        assert_eq!(submenu.find("popup-menu").focused(), Some(true));
        assert_eq!(submenu.find(0usize).checked(), Some(false));
        assert_eq!(submenu.find(1usize).checked(), Some(true));
        submenu.press("down", cx);
        assert_eq!(submenu.find(1usize).selected(), Some(true));
        submenu.press("up", cx);
        assert_eq!(submenu.find(0usize).selected(), Some(true));
        submenu.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert!(window.try_find("popup-menu").is_none());
        assert_eq!(view.read(cx).model, "Model A");
        assert_eq!(window.find("model-effort").focused(), Some(true));
        assert_eq!(window.find("model-effort").label(), Some("Model A + High"));
    });

    cx.update(|window, cx| {
        window.press("enter", cx);
        let mut menu = window.within("popup-menu");
        menu.press("down", cx);
        menu.press("down", cx);
        menu.press("right", cx);
        let mut submenu = window.within("submenu");
        assert_eq!(submenu.find(0usize).checked(), Some(false));
        assert_eq!(submenu.find(1usize).checked(), Some(true));
        submenu.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert_eq!(view.read(cx).effort, "Low");
        assert_eq!(window.find("model-effort").label(), Some("Model A + Low"));
        assert_eq!(window.find("model-effort").focused(), Some(true));
    });

    cx.update(|window, cx| {
        window.press("enter", cx);
        let mut menu = window.within("popup-menu");
        menu.press("down", cx);
        menu.press("down", cx);
        menu.press("right", cx);
        let submenu = window.within("submenu");
        assert_eq!(submenu.find(0usize).checked(), Some(true));
        assert_eq!(submenu.find(1usize).checked(), Some(false));
        window.press("escape", cx);
    });
    settle(&mut cx);
    cx.update(|window, _| {
        assert!(window.try_find("popup-menu").is_none());
        assert_eq!(window.find("model-effort").focused(), Some(true));
    });

    cx.update(|window, cx| {
        window.press("enter", cx);
        let mut menu = window.within("popup-menu");
        menu.press("down", cx);
        menu.press("right", cx);
        assert_eq!(
            window.within("submenu").find("popup-menu").focused(),
            Some(true)
        );
        window.press("left", cx);
        // Native Left is an in-place focus return, not a page-stack Back:
        // the selected parent's submenu remains visible.
        assert!(window.try_find("submenu").is_some());
        assert_eq!(
            window.within("submenu").find("popup-menu").focused(),
            Some(false)
        );
        let menus = window.find_all("popup-menu");
        assert!(menus.iter().any(|menu| menu.focused() == Some(true)));
        window.press("escape", cx);
    });
    settle(&mut cx);
    cx.update(|window, _| {
        assert!(window.try_find("popup-menu").is_none());
        assert_eq!(window.find("model-effort").focused(), Some(true));
    });
}

struct ControlledPopover {
    open: bool,
    content_focus: FocusHandle,
    changes: Vec<bool>,
}

impl Render for ControlledPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content_focus = self.content_focus.clone();
        div()
            .size_full()
            .p_4()
            .child(
                Popover::new("controlled-popover")
                    .anchor(Anchor::TopLeft)
                    .open(self.open)
                    .track_focus(&self.content_focus)
                    .trigger(Button::new("controlled-trigger").label("Open"))
                    .content(move |_, _, _| {
                        div()
                            .id("controlled-content")
                            .test_support()
                            .track_focus(&content_focus)
                            .size(px(120.))
                    })
                    .on_open_change(cx.listener(|this, open, _, cx| {
                        this.open = *open;
                        this.changes.push(*open);
                        cx.notify();
                    })),
            )
            .child(Button::new("outside").label("Outside"))
    }
}

#[gpui_kit::test]
fn workbench_popups_controlled_popover_reports_changes_and_restores_focus(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view(cx, size(px(600.), px(500.)), |_, cx| {
        cx.new(|cx| ControlledPopover {
            open: false,
            content_focus: cx.focus_handle(),
            changes: Vec::new(),
        })
    });
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    cx.update(|window, cx| {
        window.focus_next(cx);
        window.render_frame(cx);
        assert_eq!(window.find("controlled-trigger").focused(), Some(true));
        window.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert!(view.read(cx).open);
        assert_eq!(view.read(cx).changes, [true]);
        assert_eq!(window.find("controlled-content").focused(), Some(true));
        window.press("escape", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert!(!view.read(cx).open);
        assert_eq!(view.read(cx).changes, [true, false]);
        assert!(window.try_find("controlled-content").is_none());
        assert_eq!(window.find("controlled-trigger").focused(), Some(true));
        window.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| window.click("outside", cx));
    settle(&mut cx);
    cx.update(|window, cx| {
        assert!(!view.read(cx).open);
        assert_eq!(view.read(cx).changes, [true, false, true, false]);
        assert!(window.try_find("controlled-content").is_none());
        assert_eq!(window.find("controlled-trigger").focused(), Some(true));
        assert_eq!(window.find("outside").focused(), Some(false));
    });
}

struct PositionedPopover {
    anchor: Anchor,
    top: Pixels,
    scroll: ScrollHandle,
}

impl Render for PositionedPopover {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            div()
                .id("position-scroll")
                .test_support()
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(
                    div().relative().w_full().h(px(900.)).child(
                        div().absolute().right(px(40.)).top(self.top).child(
                            Popover::new("positioned-popover")
                                .default_open(true)
                                .appearance(false)
                                .anchor(self.anchor)
                                .offset(px(0.))
                                .trigger(Button::new("positioned-trigger").size(px(40.)))
                                .child(div().id("positioned-content").test_support().size(px(60.))),
                        ),
                    ),
                ),
        )
    }
}

#[gpui_kit::test]
fn workbench_popups_legacy_popover_clamps_without_flip_and_tracks_resize_and_scroll(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view(cx, size(px(600.), px(500.)), |_, cx| {
        cx.new(|_| PositionedPopover {
            anchor: Anchor::BottomCenter,
            top: px(8.),
            scroll: ScrollHandle::new(),
        })
    });
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    cx.update(|window, _| {
        let trigger = window.find("positioned-trigger").bounds();
        let content = window.find("positioned-content").bounds();
        assert_eq!(content.top(), px(8.));
        assert!(content.top() < trigger.bottom());
        assert!(content.bottom() > trigger.top());
    });

    view.update(&mut cx, |view, cx| {
        view.anchor = Anchor::TopLeft;
        view.top = px(140.);
        cx.notify();
    });
    settle(&mut cx);
    let before_resize = cx.update(|window, _| {
        let trigger = window.find("positioned-trigger").bounds();
        let content = window.find("positioned-content").bounds();
        assert_eq!(content.left(), trigger.left());
        assert_eq!(content.top(), trigger.bottom());
        trigger
    });
    cx.simulate_resize(size(px(500.), px(420.)));
    settle(&mut cx);
    cx.update(|window, _| {
        let trigger = window.find("positioned-trigger").bounds();
        let content = window.find("positioned-content").bounds();
        assert!(trigger.left() < before_resize.left());
        assert_eq!(content.left(), trigger.left());
        assert_eq!(content.top(), trigger.bottom());
    });

    let before_scroll = cx.update(|window, cx| {
        let before = (
            window.find("positioned-trigger").bounds(),
            window.find("positioned-content").bounds(),
        );
        window.scroll(
            "position-scroll",
            ScrollDelta::Pixels(point(px(0.), px(-35.))),
            cx,
        );
        before
    });
    settle(&mut cx);
    cx.update(|window, _| {
        let trigger = window.find("positioned-trigger").bounds();
        let content = window.find("positioned-content").bounds();
        assert_ne!(trigger.origin, before_scroll.0.origin);
        assert_eq!(
            content.origin - before_scroll.1.origin,
            trigger.origin - before_scroll.0.origin
        );
        assert_eq!(content.left(), trigger.left());
        assert_eq!(content.top(), trigger.bottom());
    });
}

struct CollapsibleTool {
    open: bool,
    focus: FocusHandle,
}

impl Render for CollapsibleTool {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        div().size_full().p_4().child(
            disclosure::Disclosure::new("collapsible-tool", self.open, self.focus.clone())
                .summary("read_file · completed")
                .body(
                    div()
                        .id("collapsible-tool-output")
                        .test_support()
                        .child("file contents"),
                )
                .on_toggle(move |open, _, cx| {
                    owner
                        .update(cx, |this, cx| {
                            this.open = open;
                            cx.notify();
                        })
                        .expect("disclosure owner is alive");
                }),
        )
    }
}

#[gpui_kit::test]
fn workbench_popups_collapsible_carries_tool_body_with_a_thin_focusable_summary(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let (handle, _) = open_view(cx, size(px(600.), px(400.)), |_, cx| {
        cx.new(|cx| CollapsibleTool {
            open: false,
            focus: cx.focus_handle(),
        })
    });
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    cx.update(|window, cx| {
        assert!(window.try_find("collapsible-tool-output").is_none());
        assert_eq!(
            window.find("collapsible-tool-summary").expanded(),
            Some(false)
        );
        window.focus_next(cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("collapsible-tool-summary").focused(),
            Some(true)
        );
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.find("collapsible-tool-output").visible());
        assert_eq!(
            window.find("collapsible-tool-summary").expanded(),
            Some(true)
        );
        window.press("space", cx);
        window.render_frame(cx);
        assert!(window.try_find("collapsible-tool-output").is_none());
        assert_eq!(
            window.find("collapsible-tool-summary").expanded(),
            Some(false)
        );
        assert_eq!(
            window.find("collapsible-tool-summary").focused(),
            Some(true)
        );
    });
}

struct AccordionTool {
    open: bool,
    detail_activations: usize,
}

impl Render for AccordionTool {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p_4().child(
            Accordion::new("tool-accordion")
                .item(|item| {
                    item.open(self.open)
                        .title(
                            div()
                                .flex()
                                .gap_2()
                                .child(div().id("tool-status").test_support().child("Completed"))
                                .child(
                                    div()
                                        .id("tool-command")
                                        .test_support()
                                        .child("read_file /workspace/a/long/path.rs"),
                                )
                                .child(
                                    Button::new("tool-summary-action")
                                        .label("Details")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.detail_activations += 1;
                                            cx.stop_propagation();
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("accordion-tool-output")
                                .test_support()
                                .child("tool output"),
                        )
                })
                .on_toggle_click(cx.listener(|this, open: &[usize], _, cx| {
                    this.open = open.contains(&0);
                    cx.notify();
                })),
        )
    }
}

#[gpui_kit::test]
fn workbench_popups_accordion_title_is_complex_but_its_trigger_has_no_focus_anchor(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (handle, view) = open_view(cx, size(px(700.), px(400.)), |_, cx| {
        cx.new(|_| AccordionTool {
            open: false,
            detail_activations: 0,
        })
    });
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    cx.update(|window, cx| {
        let trigger = window.within("tool-accordion").find(("trigger", 0usize));
        assert_eq!(trigger.role(), Some(Role::Button));
        assert_eq!(trigger.expanded(), Some(false));
        // This is the capability gap, not a keyboard success assertion:
        // Component Accordion exposes no trigger FocusHandle/track_focus.
        assert_eq!(trigger.focused(), None);
        assert!(window.find("tool-status").visible());
        assert!(window.find("tool-command").visible());

        window.focus_next(cx);
        window.render_frame(cx);
        assert_eq!(window.find("tool-summary-action").focused(), Some(true));
        window.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert_eq!(view.read(cx).detail_activations, 1);
        assert!(!view.read(cx).open);
        window
            .within("tool-accordion")
            .click(("trigger", 0usize), cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert!(view.read(cx).open);
        assert!(window.find("accordion-tool-output").visible());
        assert_eq!(
            window
                .within("tool-accordion")
                .find(("trigger", 0usize))
                .focused(),
            None
        );
    });
}
