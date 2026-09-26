// Component compilation, per-version filesystem grants, and Store limits.

impl Default for LibPolicy {
    fn default() -> Self {
        Self {
            data_write: true,
            outgoing: Vec::new(),
            sign: false,
            verify: false,
        }
    }
}

impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .wasm_threads(false);
        let engine = Engine::new(&config).map_err(Error::Guest)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi_http::p2::add_to_linker_async(&mut linker).map_err(Error::Guest)?;
        identity::IdentityHost::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |state| state,
        )
        .map_err(Error::Guest)?;
        Ok(Self { engine, linker })
    }

    pub(crate) fn compile(&self, bytes: &[u8]) -> Result<Component> {
        let component = Component::from_binary(&self.engine, bytes)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        ProxyPre::new(pre).map_err(|error| Error::InvalidComponent(error.to_string()))?;
        Ok(component)
    }
}

impl Lib {
    pub(crate) fn load(
        runtime: Arc<Runtime>,
        id: String,
        bytes: &[u8],
        data_dir: &Path,
        policy: LibPolicy,
        cancel: CancellationToken,
    ) -> Result<Self> {
        if id.len() > 63
            || !id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(Error::InvalidComponent("invalid Lib id".into()));
        }
        let openapi = validate_lib(bytes)?;
        let digest = Sha256::digest(bytes).into();
        let component = runtime.compile(bytes)?;
        match std::fs::symlink_metadata(data_dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Error::InvalidComponent(
                    "Lib data must be a real directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(data_dir).map_err(Error::Io)?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
        let parent = data_dir
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = std::fs::canonicalize(parent).map_err(Error::Io)?;
        let data = std::fs::canonicalize(data_dir).map_err(Error::Io)?;
        if data.parent() != Some(parent.as_path())
            || std::fs::symlink_metadata(data_dir)
                .map_err(Error::Io)?
                .file_type()
                .is_symlink()
        {
            return Err(Error::InvalidComponent(
                "Lib data resolves outside its directory".into(),
            ));
        }
        let mut builder = WasiCtx::builder();
        let (directories, files) = if policy.data_write {
            (
                wasmtime_wasi::DirPerms::all(),
                wasmtime_wasi::FilePerms::all(),
            )
        } else {
            (
                wasmtime_wasi::DirPerms::READ,
                wasmtime_wasi::FilePerms::READ,
            )
        };
        builder
            .preopened_dir(data, "/data", directories, files)
            .map_err(Error::Guest)?;
        let filesystem = builder.build().filesystem().clone();
        Ok(Self {
            id,
            digest,
            openapi,
            component,
            runtime,
            filesystem,
            policy,
            cancel,
        })
    }
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for StoreData {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.outgoing,
        }
    }
}

impl ResourceLimiter for MemoryLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.pending = 0;
        let delta = desired.saturating_sub(current);
        if delta > (64usize << 20).saturating_sub(self.used)
            || !self.base.memory_growing(current, desired, maximum)?
        {
            return Ok(false);
        }
        self.used += delta;
        self.pending = delta;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.used -= self.pending;
        self.pending = 0;
        self.base.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.base.table_growing(current, desired, maximum)
    }
    fn instances(&self) -> usize {
        self.base.instances()
    }
    fn memories(&self) -> usize {
        self.base.memories()
    }
    fn tables(&self) -> usize {
        self.base.tables()
    }
}

fn copy_policy(policy: &LibPolicy) -> LibPolicy {
    LibPolicy {
        data_write: policy.data_write,
        outgoing: policy
            .outgoing
            .iter()
            .map(|rule| OutgoingRule {
                methods: rule.methods.clone(),
                origin: rule.origin.clone(),
                path_prefix: rule.path_prefix.clone(),
            })
            .collect(),
        sign: policy.sign,
        verify: policy.verify,
    }
}
