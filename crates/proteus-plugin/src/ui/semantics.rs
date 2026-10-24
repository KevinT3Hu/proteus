//! Accessibility metadata for declarative elements. IDs are local to a surface.
use super::Rect;
use crate::types::AccessibleNode;
pub use crate::types::{AccessibilityAction as Action, AccessibilityRole as Role};

#[derive(Debug, Clone)]
pub struct Semantics {
    pub(crate) id: Option<String>,
    pub(crate) role: Role,
    pub(crate) label: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) selected: Option<bool>,
    pub(crate) expanded: Option<bool>,
    pub(crate) disabled: bool,
    pub(crate) focused: bool,
    pub(crate) actions: Vec<Action>,
}
impl Semantics {
    pub fn new(role: Role) -> Self {
        Self {
            id: None,
            role,
            label: None,
            description: None,
            value: None,
            selected: None,
            expanded: None,
            disabled: false,
            focused: false,
            actions: Vec::new(),
        }
    }
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }
    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = Some(expanded);
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
    /// Handle these through UiEvent::Accessibility. Only advertised actions are dispatched.
    pub fn actions(mut self, actions: impl IntoIterator<Item = Action>) -> Self {
        self.actions = actions.into_iter().collect();
        self
    }
    pub(crate) fn node(self, id: String, parent: Option<String>, area: Rect) -> AccessibleNode {
        AccessibleNode {
            id: self.id.unwrap_or(id),
            parent,
            role: self.role,
            label: self.label.unwrap_or_default(),
            description: self.description,
            value: self.value,
            col: area.x as u32,
            row: area.y as u32,
            cols: area.width as u32,
            rows: area.height as u32,
            selected: self.selected,
            expanded: self.expanded,
            disabled: self.disabled,
            focused: self.focused && !self.disabled,
            actions: self.actions,
            position: None,
            set_size: None,
        }
    }
}
