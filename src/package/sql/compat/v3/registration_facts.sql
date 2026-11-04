-- Package registration schema v3: snapshot used by installation/recovery journals.
SELECT json_object(
 'package',json((SELECT json_array(name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app) FROM package WHERE name=?1)),
 'dependencies',json((SELECT json_group_array(json_array(dependency_name,version)) FROM (SELECT dependency_name,version FROM package_dependency WHERE package_name=?1 ORDER BY dependency_name))),
 'capabilities',json((SELECT json_group_array(json_array(capability_id,title)) FROM (SELECT capability_id,title FROM lib_capability WHERE lib_name=?1 ORDER BY capability_id))),
 'operations',json((SELECT json_group_array(json_array(capability_id,method,path)) FROM (SELECT capability_id,method,path FROM lib_capability_operation WHERE lib_name=?1 ORDER BY capability_id,method,path)))
)
