#[derive(Debug)]
pub struct DeserialiseError {
    message: String
}

impl DeserialiseError {
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for DeserialiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DeserialiseError {}
pub trait Serialisable {
    fn serialise(&self) -> Vec<u8>;
    fn deserialise(bytes: &[u8]) -> Result<Self, DeserialiseError> where Self: Sized;
}
