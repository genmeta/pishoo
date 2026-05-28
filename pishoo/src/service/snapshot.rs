pub struct ServerService {
    _private: (),
}

#[cfg(test)]
impl ServerService {
    pub(crate) fn fake() -> Self {
        Self { _private: () }
    }
}
