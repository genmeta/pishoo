use super::*;
use crate::sandbox::WasmRuntime;

pub(crate) fn check_lib(bytes: &[u8], runtime: &WasmRuntime) -> Result<oas3::OpenApiV3Spec> {
    let package = read_package(bytes)?;
    let openapi = validate_openapi(package_openapi(&package)?)?;
    if let Some(backend) = package.get("backend.wasm") {
        runtime.compile(backend)?;
    }
    Ok(openapi)
}
