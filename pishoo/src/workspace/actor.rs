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

    pub fn subject_id(&self) -> &SubjectId {
        &self.subject_id
    }
}

#[cfg(test)]
mod tests {
    use access_control::{SubjectId, Visitor};

    use super::Owner;

    #[test]
    fn owner_requires_both_authenticated_name_and_subject() {
        let subject = SubjectId::new(b"actual-key".to_vec()).expect("valid owner subject");
        let owner = Owner::new("owner.example".to_owned(), subject.clone());
        assert!(owner.ensure(None).is_err());
        assert!(
            owner
                .ensure(Some(&Visitor::new("owner.example", subject.clone())))
                .is_ok()
        );
        assert!(
            owner
                .ensure(Some(&Visitor::new("other.example", subject)))
                .is_err()
        );
        assert!(
            owner
                .ensure(Some(&Visitor::new(
                    "owner.example",
                    SubjectId::new(b"old-key".to_vec()).expect("valid old subject"),
                )))
                .is_err()
        );
    }
}
