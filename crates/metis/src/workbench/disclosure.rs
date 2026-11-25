//! Controlled disclosure with a stable, keyboard-focusable native summary.

use gpui_kit::{
    AnyElement, App, FocusHandle, Window,
    component::{button::Button, collapsible::Collapsible},
    div,
    prelude::*,
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
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let open = self.open;
        let on_toggle = self.on_toggle;
        Collapsible::new()
            .open(open)
            .child(
                Button::new(format!("{}-summary", self.id))
                    .track_focus(&self.focus)
                    .children(self.summary)
                    .on_click(move |_, window, cx| {
                        if let Some(on_toggle) = &on_toggle {
                            on_toggle(!open, window, cx);
                        }
                    }),
            )
            .content(div().id(format!("{}-body", self.id)).children(self.body))
    }
}
