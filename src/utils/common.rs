use crate::core::error::BluefinError;

pub type BluefinResult<T> = Result<T, BluefinError>;

#[macro_export]
macro_rules! set_some_builder_field {
    ($field_name:ident,$field_type:ident) => {
        #[inline]
        pub fn $field_name(mut self, $field_name: $field_type) -> Self {
            self.$field_name = Some($field_name);
            self
        }
    };
}
