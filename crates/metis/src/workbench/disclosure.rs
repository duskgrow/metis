//! Controlled disclosure with a stable, keyboard-focusable native summary.

use gpui_kit::{
    AnyElement, App, BoxShadow, FocusHandle, Point, Window,
    base::Button,
    component::{ActiveTheme as _, collapsible::Collapsible},
    div,
    prelude::*,
    px,
};

type ToggleHandler = Box<dyn Fn(bool, &mut Window, &mut App)>;

#[derive(gpui_kit::IntoElement)]
pub(super) struct Disclosure {
    id: String,
    open: bool,
    focus: FocusHandle,
    summary: Option<AnyElement>,
    body: Option<AnyElement>,
    on_toggle: Option<ToggleHandler>,
}

impl Disclosure {
    pub(super) fn new(id: impl Into<String>, open: bool, focus: FocusHandle) -> Self {
        Self {
            id: id.into(),
            open,
            focus,
            summary: None,
            body: None,
            on_toggle: None,
        }
    }

    pub(super) fn summary(mut self, summary: impl IntoElement) -> Self {
        self.summary = Some(summary.into_any_element());
        self
    }

    pub(super) fn body(mut self, body: impl IntoElement) -> Self {
        self.body = Some(body.into_any_element());
        self
    }

    pub(super) fn on_toggle(
        mut self,
        on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_toggle = Some(Box::new(on_toggle));
        self
    }
}

impl RenderOnce for Disclosure {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let open = self.open;
        let on_toggle = self.on_toggle;
        let selection = cx.theme().selection;
        let ring = cx.theme().ring;
        Collapsible::new()
            .open(open)
            .child(
                Button::new(format!("{}-summary", self.id))
                    .track_focus(&self.focus)
                    .aria_expanded(open)
                    .justify_start()
                    .rounded_md()
                    .p_1()
                    .hover(move |style| style.bg(selection))
                    .focus_visible(move |style| {
                        let mut style = style.bg(selection);
                        style.box_shadow.get_or_insert_default().push(BoxShadow {
                            color: ring,
                            offset: Point::default(),
                            blur_radius: px(0.),
                            spread_radius: px(2.),
                            inset: false,
                        });
                        style
                    })
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(if open { "▾" } else { "▸" })
                            .children(self.summary),
                    )
                    .on_click(move |_, window, cx| {
                        if let Some(on_toggle) = &on_toggle {
                            on_toggle(!open, window, cx);
                        }
                    }),
            )
            .content(div().id(format!("{}-body", self.id)).children(self.body))
    }
}
