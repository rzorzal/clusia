//! The agent's chat inside a review (mockup `AgentChat.png`): the per-review model and the
//! column that shows it.

pub mod model;
pub mod panel;
pub mod permission;
pub mod suggestion;

use bevy::prelude::*;

use crate::bridge::Model;
use crate::review_state::ReviewTabs;

/// Points each chat at the agent its review is pinned to. It runs before the tells of a frame
/// are applied, so an error or a replay that creates a line already words it for that agent.
pub fn name_chats(tabs: Res<ReviewTabs>, model: Res<Model>, mut chats: ResMut<Chats>) {
    for (pr, chat) in chats.0.iter_mut() {
        let kind = tabs
            .0
            .get(pr)
            .map_or(model.snapshot.config.harness.kind, |t| {
                crate::agent_name::kind_of_tab(&model.snapshot, t)
            });
        if chat.kind != kind {
            chat.kind = kind;
        }
    }
}

pub use model::{
    ChatLine, ChatModel, Chats, DEFAULT_WIDTH, MAX_WIDTH, MIN_WIDTH, PanelTab, SuggestionState,
    display_text, place_of,
};
pub use panel::{
    AgentPanel, AgentRegion, CHAT_INPUT, ChatInput, PanelColumn, PanelTabs, PanelToggle,
    harness_ready, on_panel_toggle, resize_edge, send_message,
};
pub use permission::{PermissionModal, PermissionPlugin, PermissionQueue};
