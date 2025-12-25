use bluefin_proto::error::BluefinError;

pub mod header;
pub mod packet;

pub trait Extract: Default {
    /// Replace self with default and returns the initial value.
    fn extract(&mut self) -> Self;
}

impl<T: Default> Extract for T {
    fn extract(&mut self) -> Self {
        std::mem::replace(self, T::default())
    }
}

pub trait Serialisable {
    fn serialise(&self) -> Vec<u8>;
    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized;
}
