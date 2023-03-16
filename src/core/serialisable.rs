use super::error::BluefinError;

pub trait Serialisable {
    fn serialise(&self) -> Vec<u8>;
    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized;
}
