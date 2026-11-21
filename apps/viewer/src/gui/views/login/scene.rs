use prism::{Context, Window};

use super::LoginScreen;
use crate::gui::{views::chat::ChatScreen, window_geometry::observe};

impl LoginScreen {
    pub(crate) fn attach_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._window_geometry_subscription = observe("login", window, cx);
    }

    pub(crate) fn chat_view(&self) -> prism::Entity<ChatScreen> {
        self.chat_screen.clone()
    }
}
