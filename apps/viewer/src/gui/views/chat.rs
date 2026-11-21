use std::collections::VecDeque;

use network::{AgentSession, Delivery, SessionHandle};
use prism::{
    Context, Entity, IntoElement, Render, Role, StyleRefinement, Window,
    component::{
        Theme,
        button::{Button, ButtonVariants as _},
        input::{Input, InputState},
        message_scroller::{MessageScroller, MessageScrollerState},
    },
    div,
    prelude::*,
    px,
};
use protocol::{
    ChatType,
    messages::{
        ChatFromViewer,
        chat::chat_from_viewer::{AgentData, ChatData},
    },
};

use crate::session::{ChatMessage, format_avatar_name};

const MAX_CHAT_HISTORY_MESSAGES: usize = 500;
const MAX_CHAT_HISTORY_BYTES: usize = 1024 * 1024;

pub struct ChatScreen {
    session: Option<AgentSession>,
    region_name: Option<String>,
    session_handle: Option<SessionHandle>,
    messages: VecDeque<ChatMessage>,
    message_bytes: usize,
    scroller: Entity<MessageScrollerState>,
    message_input: Entity<InputState>,
    send_error: Option<&'static str>,
}

impl ChatScreen {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message_input = cx.new(|cx| InputState::new(window, cx).placeholder("To nearby chat"));
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));

        Self {
            session: None,
            region_name: None,
            session_handle: None,
            messages: VecDeque::new(),
            message_bytes: 0,
            scroller,
            message_input,
            send_error: None,
        }
    }

    pub fn set_session(&mut self, session: AgentSession, cx: &mut Context<Self>) {
        self.session = Some(session);
        self.region_name = None;
        self.send_error = None;
        cx.notify();
    }

    pub fn clear_session(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.region_name = None;
        self.session_handle = None;
        self.send_error = None;
        self.messages.clear();
        self.message_bytes = 0;
        self.scroller.update(cx, |state, cx| state.reset(0, cx));
        cx.notify();
    }

    pub fn set_region_name(&mut self, region_name: String, cx: &mut Context<Self>) {
        self.region_name = Some(region_name);
        cx.notify();
    }

    pub fn set_session_handle(&mut self, session_handle: SessionHandle, cx: &mut Context<Self>) {
        self.session_handle = Some(session_handle);
        self.send_error = None;
        cx.notify();
    }

    pub fn add_chat_messages(&mut self, messages: impl IntoIterator<Item = ChatMessage>, cx: &mut Context<Self>) {
        let mut added_message = false;
        for message in messages {
            added_message |= self.push_message(message, cx);
        }
        if added_message {
            cx.notify();
        }
    }

    fn send_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.message_input.read(cx).value().trim().to_owned();
        if message.is_empty() {
            return;
        }

        let result = match (self.session_handle.as_ref(), self.session.as_ref()) {
            (Some(session_handle), Some(session)) => session_handle
                .send(
                    ChatFromViewer {
                        agent_data: AgentData { agent_id: session.agent_id, session_id: session.session_id },
                        chat_data: ChatData { message: message.clone(), chat_type: ChatType::Normal, channel: 0 },
                    },
                    Delivery::Reliable,
                )
                .map_err(|error| {
                    tracing::warn!(%error, "nearby chat send failed");
                    send_error_copy(&error)
                }),
            _ => Err("Connect to a simulator before sending nearby chat."),
        };

        match result {
            Ok(()) => {
                self.send_error = None;
                let from_name = self
                    .session
                    .as_ref()
                    .map(|session| format_avatar_name(&format!("{} {}", session.first_name, session.last_name)))
                    .unwrap_or_else(|| "You".into());
                self.push_message(ChatMessage { from_name, message }, cx);
                self.scroller.update(cx, |state, cx| state.scroll_to_end(cx));
                self.message_input.update(cx, |input, cx| input.set_value("", window, cx));
                cx.notify();
            }
            Err(error) => {
                self.send_error = Some(error);
                cx.notify();
            }
        }
    }

    fn push_message(&mut self, message: ChatMessage, cx: &mut Context<Self>) -> bool {
        let before = self.messages.len();
        if !retain_message(&mut self.messages, &mut self.message_bytes, message) {
            return false;
        }
        let after = self.messages.len();
        let evicted = before + 1 - after;
        self.scroller.update(cx, |state, cx| {
            if evicted > 0 {
                state.splice(0..evicted, 0, cx);
            }
            state.append(1, cx);
        });
        true
    }
}

impl Render for ChatScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx).colors;

        let chat = cx.entity();
        let transcript = div()
            .flex_1()
            .min_h_0()
            .debug_selector(|| "chat-transcript-viewport".to_owned())
            .child(
                MessageScroller::new("chat-transcript", self.scroller.clone(), move |index, _window, cx| {
                    let theme = Theme::global(cx).colors;
                    let Some(message) = chat.read(cx).messages.get(index) else {
                        return div().into_any_element();
                    };
                    div()
                        .flex()
                        .items_start()
                        .gap_1()
                        .py_1()
                        .text_sm()
                        .child(div().flex_none().text_color(theme.muted_foreground).child(format!("{}:", message.from_name)))
                        .child(div().min_w_0().flex_1().text_color(theme.foreground).child(message.message.clone()))
                        .into_any_element()
                })
                .with_row_style(StyleRefinement::default().px_2().pb_0())
                .with_bottom_fade(theme.background),
            )
            .into_any_element();

        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .debug_selector(|| "chat-screen".to_owned())
            .child(transcript)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .bg(theme.sidebar)
                    .border_t_1()
                    .border_color(theme.border)
                    .debug_selector(|| "chat-composer".to_owned())
                    .when_some(self.send_error, |composer, error| {
                        composer.child(
                            div()
                                .id("nearby-chat-send-error")
                                .role(Role::Alert)
                                .w_full()
                                .min_w_0()
                                .text_sm()
                                .text_color(theme.danger)
                                .debug_selector(|| "chat-send-error".to_owned())
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w_0().debug_selector(|| "chat-input".to_owned()).child(
                                Input::new(&self.message_input).focus_bordered(false).aria_label("To nearby chat").h(px(38.)).w_full(),
                            ))
                            .child(
                                Button::new("send-nearby-chat")
                                    .label("Send")
                                    .primary()
                                    .h(px(32.))
                                    .debug_selector(|| "chat-send-button".to_owned())
                                    .on_click(cx.listener(|this, _event, window, cx| this.send_message(window, cx))),
                            ),
                    ),
            )
    }
}

fn send_error_copy(error: &network::Error) -> &'static str {
    match error {
        network::Error::CommandQueueFull => "Message wasn't sent. Try again in a moment.",
        network::Error::OutboundMessageTooLarge => "This message is too long to send. Shorten it and try again.",
        network::Error::SessionClosed | network::Error::SimulatorConnectionClosed => {
            "Message wasn't sent. Reconnect and try sending again."
        }
        _ => "Couldn't send nearby chat. Check your connection and try again.",
    }
}

fn retain_message(messages: &mut VecDeque<ChatMessage>, message_bytes: &mut usize, message: ChatMessage) -> bool {
    let size = message.from_name.len().saturating_add(message.message.len());
    if size > MAX_CHAT_HISTORY_BYTES {
        return false;
    }

    while messages.len() >= MAX_CHAT_HISTORY_MESSAGES || message_bytes.saturating_add(size) > MAX_CHAT_HISTORY_BYTES {
        let Some(removed) = messages.pop_front() else {
            break;
        };
        *message_bytes = message_bytes.saturating_sub(removed.from_name.len().saturating_add(removed.message.len()));
    }

    *message_bytes += size;
    messages.push_back(message);
    true
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use prism::{Modifiers, TestAppContext, VisualTestContext, point, px, size};

    use super::{ChatMessage, ChatScreen, MAX_CHAT_HISTORY_BYTES, MAX_CHAT_HISTORY_MESSAGES, retain_message, send_error_copy};

    #[test]
    fn history_keeps_recent_messages() {
        let mut messages = VecDeque::new();
        let mut bytes = 0;
        for index in 0..MAX_CHAT_HISTORY_MESSAGES + 5 {
            assert!(retain_message(
                &mut messages,
                &mut bytes,
                ChatMessage { from_name: format!("Agent {index}"), message: "hello".into() },
            ));
        }

        assert_eq!(messages.len(), MAX_CHAT_HISTORY_MESSAGES);
        assert_eq!(messages.front().map(|message| message.from_name.as_str()), Some("Agent 5"));
        assert_eq!(messages.back().map(|message| message.from_name.as_str()), Some("Agent 504"));
        assert!(bytes <= MAX_CHAT_HISTORY_BYTES);
    }

    #[test]
    fn history_bounds_bytes() {
        let mut messages = VecDeque::new();
        let mut bytes = 0;
        for index in 0..2 {
            assert!(retain_message(
                &mut messages,
                &mut bytes,
                ChatMessage { from_name: format!("Agent {index}"), message: "x".repeat(600 * 1024) },
            ));
        }

        assert_eq!(messages.len(), 1);
        assert_eq!(messages.front().map(|message| message.from_name.as_str()), Some("Agent 1"));
        assert_eq!(bytes, messages.front().unwrap().from_name.len() + 600 * 1024);

        assert!(!retain_message(
            &mut messages,
            &mut bytes,
            ChatMessage { from_name: "Agent".into(), message: "x".repeat(MAX_CHAT_HISTORY_BYTES) },
        ));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages.front().map(|message| message.from_name.as_str()), Some("Agent 1"));
    }

    #[test]
    fn send_errors_preserve_safe_feedback() {
        assert_eq!(send_error_copy(&network::Error::CommandQueueFull), "Message wasn't sent. Try again in a moment.");
        assert_eq!(
            send_error_copy(&network::Error::OutboundMessageTooLarge),
            "This message is too long to send. Shorten it and try again."
        );
        assert_eq!(send_error_copy(&network::Error::SessionClosed), "Message wasn't sent. Reconnect and try sending again.");
        assert_eq!(
            send_error_copy(&network::Error::SimulatorSend("session_id=private".into())),
            "Couldn't send nearby chat. Check your connection and try again."
        );
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
    }

    #[prism::test]
    fn rejected_send_preserves_draft(cx: &mut TestAppContext) {
        cx.update(prism::init);
        let (chat, cx) = cx.add_window_view(ChatScreen::new);
        draw(cx);

        chat.update_in(cx, |chat, window, cx| chat.message_input.update(cx, |input, cx| input.set_value("hello nearby", window, cx)));
        draw(cx);

        let send_bounds = cx.debug_bounds("chat-send-button").unwrap();
        cx.simulate_click(
            point(send_bounds.left() + send_bounds.size.width / 2., send_bounds.top() + send_bounds.size.height / 2.),
            Modifiers::default(),
        );
        draw(cx);

        let (draft, message_count, send_error) = cx.update(|_, app| {
            let chat = chat.read(app);
            (chat.message_input.read(app).value().to_string(), chat.messages.len(), chat.send_error)
        });
        assert_eq!(draft, "hello nearby");
        assert_eq!(message_count, 0);
        assert_eq!(send_error, Some("Connect to a simulator before sending nearby chat."));
        assert!(cx.debug_bounds("chat-send-error").is_some(), "the send error should be visible next to the composer");

        cx.simulate_resize(size(px(360.), px(240.)));
        draw(cx);
        let error = cx.debug_bounds("chat-send-error").expect("visible error after resize");
        let input = cx.debug_bounds("chat-input").expect("visible input after resize");
        assert!(error.bottom() <= input.top(), "error feedback must not overlap the retained draft");
        assert_eq!(cx.debug_bounds("chat-composer").unwrap().bottom(), cx.debug_bounds("chat-screen").unwrap().bottom());

        cx.update(|_, app| chat.update(app, |chat, cx| chat.clear_session(cx)));
        draw(cx);
        assert!(cx.debug_bounds("chat-send-error").is_none(), "clearing the session should clear its stale send error");
        assert_eq!(cx.update(|_, app| chat.read(app).message_input.read(app).value().to_string()), "hello nearby");
    }
}
