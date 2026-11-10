//! The sizes and small building blocks the window's chrome shares, so the
//! dialogs, bars and panes line up and look alike.

use gpui_kit::{Div, IntoElement, ParentElement as _, Styled as _, div};

/// `element` at its natural width in a dialog footer. The footer's close
/// and confirm wrappers fill their parent, which in a row stretches them.
pub fn natural(element: impl IntoElement) -> Div {
    div().flex_none().child(element)
}
