use serde_derive::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub shape: Option<(u8, u8)>,
}

#[derive(Serialize, Deserialize)]
pub struct RegistrationInfo {
    pub groups: Vec<Group>,
    pub candidate_group: String,
    pub candidate_position: Vec<u8>,
}
