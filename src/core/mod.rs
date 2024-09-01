use error::BluefinError;

pub mod context;
pub mod error;
pub mod header;
pub mod packet;

pub trait Serialisable {
    fn serialise(&self) -> Vec<u8>;
    fn deserialise(bytes: &[u8]) -> Result<Self, BluefinError>
    where
        Self: Sized;
}
