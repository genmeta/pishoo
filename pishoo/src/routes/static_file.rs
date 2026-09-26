async fn static_file(root: &std::path::Path, request: Request<AxumBody>) -> Result<Response> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Err(Error::MethodNotAllowed);
    }
    let decoded = percent_encoding::percent_decode_str(request.uri().path())
        .decode_utf8()
        .map_err(|_| Error::BadRequest("invalid path encoding".into()))?;
    if decoded.contains(['\\', '\0']) || decoded.split('/').any(|p| p == ".." || p == ".") {
        return Err(Error::RouteNotFound);
    }
    let mut path = std::path::PathBuf::from(decoded.trim_start_matches('/'));
    let root = root.to_path_buf();
    let (file, metadata, path) = tokio::task::spawn_blocking(move || -> Result<_> {
        let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(|_| Error::RouteNotFound)?;
        if path.as_os_str().is_empty() || dir.metadata(&path).is_ok_and(|m| m.is_dir()) {
            path.push("index.html");
        }
        let file = dir.open(&path).map_err(|_| Error::RouteNotFound)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::RouteNotFound);
        }
        Ok((file.into_std(), metadata, path))
    })
    .await??;
    let body = if request.method() == Method::HEAD {
        AxumBody::empty()
    } else {
        AxumBody::from_stream(ReaderStream::with_capacity(
            tokio::fs::File::from_std(file),
            16 * 1024,
        ))
    };
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, metadata.len().into());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .as_ref()
            .parse()
            .unwrap(),
    );
    Ok(response)
}
