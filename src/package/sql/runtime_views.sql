CREATE TEMP VIEW lib AS SELECT * FROM pkg_registry.package WHERE has_lib=1;
CREATE TEMP VIEW app AS SELECT * FROM pkg_registry.package WHERE has_app=1;
