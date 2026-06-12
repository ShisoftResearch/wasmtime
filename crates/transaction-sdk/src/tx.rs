#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tx;

impl Tx {
    pub unsafe fn from_marker() -> Self {
        Self
    }
}
