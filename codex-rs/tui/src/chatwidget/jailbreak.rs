//! Jailbreak level popup for `ChatWidget`.

use super::*;
use codex_protocol::jailbreak::JailbreakLevel;
use codex_protocol::jailbreak::get_jailbreak_level;
use codex_protocol::jailbreak::set_jailbreak_level;

impl ChatWidget {
    /// Open a popup to choose the jailbreak level.
    pub(crate) fn open_jailbreak_popup(&mut self) {
        let current = get_jailbreak_level();
        let items: Vec<SelectionItem> = JailbreakLevel::ALL
            .iter()
            .map(|&level| {
                let is_current = level == current;
                SelectionItem {
                    name: level.label().to_string(),
                    description: Some(level.description().to_string()),
                    is_current,
                    dismiss_on_select: true,
                    actions: vec![Box::new(move |tx| {
                        set_jailbreak_level(level);
                        // Refresh the footer status line so the jailbreak
                        // indicator reflects the new level immediately.
                        tx.send(AppEvent::JailbreakLevelChanged);
                    })],
                    ..Default::default()
                }
            })
            .collect();

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select jailbreak level".to_string()),
            subtitle: Some(
                "Controls the content-handling system prompt section and synthetic history priming. Applies to this session."
                    .to_string(),
            ),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    /// Apply a jailbreak level from inline args (e.g. `/jailbreak full`).
    pub(crate) fn apply_jailbreak_inline(&mut self, arg: &str) {
        if let Some(level) = JailbreakLevel::from_str_opt(arg) {
            set_jailbreak_level(level);
            // Refresh the footer so the jailbreak indicator updates immediately.
            self.refresh_status_line();
            self.add_info_message(
                format!("Jailbreak level set to {level}"),
                /*hint*/ None,
            );
        } else {
            self.add_error_message(format!(
                "Invalid jailbreak level: '{arg}'. Valid options: disabled, lowered, full"
            ));
        }
    }
}
