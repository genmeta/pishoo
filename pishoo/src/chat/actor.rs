use access_control::{SubjectId, Visitor};
use http::StatusCode;

pub struct Owner {
    name: String,
    subject_id: SubjectId,
}

impl Owner {
    pub fn new(name: String, subject_id: SubjectId) -> Self {
        Self { name, subject_id }
    }

    pub fn ensure(&self, visitor: Option<&Visitor>) -> Result<(), StatusCode> {
        match visitor {
            Some(visitor)
                if visitor.name() == self.name.as_str()
                    && visitor.subject_id() == &self.subject_id =>
            {
                Ok(())
            }
            _ => Err(StatusCode::FORBIDDEN),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}
