pub trait Serialisable {
    fn serialise(&self) -> Vec<u8>;
    fn deserialise(bytes: &[u8]) -> Self;
}
