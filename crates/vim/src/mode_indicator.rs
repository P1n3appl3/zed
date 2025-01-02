use gpui::{div, Element, Model, ModelContext, Render, Subscription, WeakModel, Window};
use itertools::Itertools;
use workspace::{item::ItemHandle, ui::prelude::*, StatusItemView};

use crate::{Vim, VimEvent, VimGlobals};

/// The ModeIndicator displays the current mode in the status bar.
pub struct ModeIndicator {
    vim: Option<WeakModel<Vim>>,
    pending_keys: Option<String>,
    vim_subscription: Option<Subscription>,
}

impl ModeIndicator {
    /// Construct a new mode indicator in this window.
    pub fn new(window: &mut Window, cx: &mut ModelContext<Self>) -> Self {
        cx.observe_pending_input(window, |this, window, cx| {
            this.update_pending_keys(window, cx);
            cx.notify();
        })
        .detach();

        let handle = cx.view().clone();
        let window = window.window_handle();
        cx.observe_new_views::<Vim>(move |_, cx| {
            if window.window_handle() != window {
                return;
            }
            let vim = cx.view().clone();
            handle.update(cx, |_, cx| {
                cx.subscribe_in(
                    &vim,
                    window,
                    |mode_indicator, vim, event, window, cx| match event {
                        VimEvent::Focused => {
                            mode_indicator.vim_subscription =
                                Some(cx.observe_in(&vim, window, |_, _, window, cx| cx.notify()));
                            mode_indicator.vim = Some(vim.downgrade());
                        }
                    },
                )
                .detach()
            })
        })
        .detach();

        Self {
            vim: None,
            pending_keys: None,
            vim_subscription: None,
        }
    }

    fn update_pending_keys(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        self.pending_keys = window.pending_input_keystrokes().map(|keystrokes| {
            keystrokes
                .iter()
                .map(|keystroke| format!("{}", keystroke))
                .join(" ")
        });
    }

    fn vim(&self) -> Option<Model<Vim>> {
        self.vim.as_ref().and_then(|vim| vim.upgrade())
    }

    fn current_operators_description(
        &self,
        vim: Model<Vim>,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> String {
        let recording = Vim::globals(cx)
            .recording_register
            .map(|reg| format!("recording @{reg} "))
            .into_iter();

        let vim = vim.read(cx);
        recording
            .chain(
                cx.global::<VimGlobals>()
                    .pre_count
                    .map(|count| format!("{}", count)),
            )
            .chain(vim.selected_register.map(|reg| format!("\"{reg}")))
            .chain(
                vim.operator_stack
                    .iter()
                    .map(|item| item.status().to_string()),
            )
            .chain(
                cx.global::<VimGlobals>()
                    .post_count
                    .map(|count| format!("{}", count)),
            )
            .collect::<Vec<_>>()
            .join("")
    }
}

impl Render for ModeIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) -> impl IntoElement {
        let vim = self.vim();
        let Some(vim) = vim else {
            return div().into_any();
        };

        let vim_readable = vim.read(cx);
        let mode = if vim_readable.temp_mode {
            format!("(insert) {}", vim_readable.mode)
        } else {
            vim_readable.mode.to_string()
        };

        let current_operators_description =
            self.current_operators_description(vim.clone(), window, cx);
        let pending = self
            .pending_keys
            .as_ref()
            .unwrap_or(&current_operators_description);
        Label::new(format!("{} -- {} --", pending, mode))
            .size(LabelSize::Small)
            .line_height_style(LineHeightStyle::UiLabel)
            .into_any_element()
    }
}

impl StatusItemView for ModeIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut ModelContext<Self>,
    ) {
    }
}
