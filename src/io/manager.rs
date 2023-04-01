use std::collections::HashMap;

use crate::{core::error::BluefinError, network::connection::Connection};

const MAX_NUMBER_OF_CONNECTIONS: usize = 100;
pub struct ConnectionManager {
    pub connection_map: HashMap<String, Connection>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            connection_map: HashMap::new(),
        }
    }

    pub fn get_connection(&mut self, id: &str) -> Result<&mut Connection, BluefinError> {
        if !self.connection_map.contains_key(id) {
            return Err(BluefinError::NoSuchConnectionError);
        }

        let conn = self.connection_map.get_mut(id).unwrap();
        Ok(conn)
    }

    pub fn add_connection(&mut self, conn: Connection) -> Result<(), BluefinError> {
        if self.connection_map.len() >= MAX_NUMBER_OF_CONNECTIONS {
            return Err(BluefinError::TooManyOpenConnectionsError);
        }

        self.connection_map.insert(conn.id.clone(), conn);

        Ok(())
    }
}
