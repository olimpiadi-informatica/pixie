pub mod ps2;
pub mod serial;
pub mod xhci;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Up,
    Down,
    Left,
    Right,
    Delete,
    Enter,
    Char(char),
    CtrlAltDel,
}
