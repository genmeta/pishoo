use std::time::SystemTime;

const MAX_SUBJECT_ID_LEN: usize = 64;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SubjectId(Vec<u8>);

impl SubjectId {
    pub fn new(value: impl Into<Vec<u8>>) -> Result<Self, SubjectIdError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_SUBJECT_ID_LEN {
            return Err(SubjectIdError);
        }
        Ok(Self(value))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Visitor {
    name: String,
    subject_id: SubjectId,
}

impl Visitor {
    pub fn new(name: impl Into<String>, subject_id: SubjectId) -> Self {
        Self {
            name: name.into(),
            subject_id,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn subject_id(&self) -> &SubjectId {
        &self.subject_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubjectIdError;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContactStatus {
    #[default]
    Pending,
    Syncing,
    Active,
    Changed,
    Retired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contact {
    name: String,
    alias: Option<String>,
    class: String,
    subject_id: SubjectId,
    description: String,
    status: ContactStatus,
    updated_at: SystemTime,
    created_at: SystemTime,
}

impl Contact {
    pub fn set_alias(&mut self, alias: Option<String>) {
        self.alias = alias;
        self.touch();
    }

    pub fn set_class(&mut self, class: String) {
        self.class = class;
        self.touch();
    }

    pub fn check_subject_id(&mut self, subject_id: &SubjectId) -> ContactStatus {
        if self.subject_id != *subject_id && self.status == ContactStatus::Active {
            self.status = ContactStatus::Changed;
            self.touch();
        }
        self.status
    }

    pub fn set_description(&mut self, description: String) {
        self.description = description;
        self.touch();
    }

    pub fn retire(&mut self) {
        self.status = ContactStatus::Retired;
        self.touch();
    }

    pub fn status(&self) -> ContactStatus {
        self.status
    }

    fn touch(&mut self) {
        self.updated_at = SystemTime::now();
    }
}

#[cfg(test)]
mod tests {
    use super::SubjectId;

    #[test]
    fn subject_id_accepts_a_dhttp_owner_hash() {
        let owner_hash = "0".repeat(64);

        assert_eq!(
            SubjectId::new(owner_hash.as_bytes()).unwrap().as_bytes(),
            owner_hash.as_bytes()
        );
    }

    #[test]
    fn subject_id_rejects_values_longer_than_a_dhttp_owner_hash() {
        assert!(SubjectId::new(vec![0; 65]).is_err());
    }
}
