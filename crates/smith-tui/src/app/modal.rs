//! The four modals and the enum that says which one is up.

use smith_core::{PermissionRequest, UserQuestion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Done,
    Error,
}

/// Format a duration as `959ms` or `1.1s` for thought rows.
pub fn format_thought(secs: f32) -> String {
    if secs < 1.0 {
        format!("{}ms", (secs * 1000.0) as u32)
    } else {
        format!("{:.1}s", secs)
    }
}

#[derive(Debug, Clone)]
pub struct PermissionModal {
    pub request: PermissionRequest,
    /// Vertical scroll into the permission preview body.
    pub scroll: u16,
}

/// Shown after a `/plan` turn finishes — review the plan, then build or reject.
#[derive(Debug, Clone)]
pub struct PlanModal {
    pub text: String,
    pub scroll: u16,
}

/// Clarifying question from `ask_user`: three suggestions + free-text (index 3).
#[derive(Debug, Clone)]
pub struct QuestionModal {
    pub question: UserQuestion,
    /// 0..=2 = suggestions, 3 = custom text.
    pub selected: usize,
    pub custom: String,
}

/// The one overlay that can be on screen at a time. Only one interactive
/// wait is ever in flight per turn (the agent loop blocks on a single
/// `oneshot` at once), so this is a true sum type rather than three
/// independent `Option`s that happened to always be mutually exclusive in
/// practice — a state combination like "plan modal AND question modal both
/// open" is no longer representable at all, instead of just avoided by
/// convention.
/// The `/model` picker.
///
/// A filter rather than a plain list, because a gateway can list dozens and
/// scrolling to `openrouter/nvidia/nemotron-3-nano-30b-a3b:free` with arrow
/// keys is worse than typing four characters of it.
#[derive(Debug, Clone, Default)]
pub struct ModelPicker {
    pub provider: String,
    /// Everything the provider offered, in the order it offered it.
    pub all: Vec<smith_core::ModelChoice>,
    /// What has been typed to narrow the list.
    pub filter: String,
    /// Index into `matches()`, not into `all`.
    pub selected: usize,
    /// Top row of the visible window, so a long list scrolls.
    pub scroll: usize,
}

impl ModelPicker {
    /// Case-insensitive substring, which is what someone types when they
    /// half remember a name. Sub-sequence matching was considered and rejected:
    /// it turns `gpt` into a match for `google/gemma-4-31b-it`, and a picker
    /// that surprises you is worse than one that finds less.
    pub fn matches(&self) -> Vec<&smith_core::ModelChoice> {
        let needle = self.filter.trim().to_ascii_lowercase();
        self.all
            .iter()
            .filter(|m| needle.is_empty() || m.id.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    pub fn selected_id(&self) -> Option<String> {
        self.matches().get(self.selected).map(|m| m.id.clone())
    }

    /// Keeps the cursor inside the filtered list and the window around the
    /// cursor. Called after anything that can change either.
    pub fn clamp(&mut self, visible: usize) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(len - 1);
        let visible = visible.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.scroll = self.scroll.min(len.saturating_sub(1));
    }
}

#[derive(Debug, Clone, Default)]
pub enum Modal {
    #[default]
    None,
    Permission(PermissionModal),
    Plan(PlanModal),
    Question(QuestionModal),
    Model(ModelPicker),
}

impl Modal {
    pub fn is_none(&self) -> bool {
        matches!(self, Modal::None)
    }

    pub fn is_some(&self) -> bool {
        !self.is_none()
    }

    pub fn is_plan(&self) -> bool {
        matches!(self, Modal::Plan(_))
    }

    pub fn is_question(&self) -> bool {
        matches!(self, Modal::Question(_))
    }

    pub fn is_model(&self) -> bool {
        matches!(self, Modal::Model(_))
    }

    pub fn model(&self) -> Option<&ModelPicker> {
        match self {
            Modal::Model(m) => Some(m),
            _ => None,
        }
    }

    pub fn model_mut(&mut self) -> Option<&mut ModelPicker> {
        match self {
            Modal::Model(m) => Some(m),
            _ => None,
        }
    }

    pub fn permission(&self) -> Option<&PermissionModal> {
        match self {
            Modal::Permission(m) => Some(m),
            _ => None,
        }
    }

    pub fn permission_mut(&mut self) -> Option<&mut PermissionModal> {
        match self {
            Modal::Permission(m) => Some(m),
            _ => None,
        }
    }

    pub fn plan(&self) -> Option<&PlanModal> {
        match self {
            Modal::Plan(m) => Some(m),
            _ => None,
        }
    }

    pub fn plan_mut(&mut self) -> Option<&mut PlanModal> {
        match self {
            Modal::Plan(m) => Some(m),
            _ => None,
        }
    }

    pub fn question(&self) -> Option<&QuestionModal> {
        match self {
            Modal::Question(m) => Some(m),
            _ => None,
        }
    }

    pub fn question_mut(&mut self) -> Option<&mut QuestionModal> {
        match self {
            Modal::Question(m) => Some(m),
            _ => None,
        }
    }
}
