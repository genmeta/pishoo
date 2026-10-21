SELECT json_object(
 'package',json((SELECT json_array(name,author,version,title,description,license,repository,installed_at,upgraded_at) FROM lib WHERE ?1='lib' AND name=?2 UNION ALL SELECT json_array(name,author,version,title,description,license,repository,installed_at,upgraded_at) FROM app WHERE ?1='app' AND name=?2)),
 'dependencies',json((SELECT json_group_array(json_array(dependency,version)) FROM (SELECT lib_name AS dependency,version FROM app_dependency WHERE ?1='app' AND app_name=?2 UNION ALL SELECT dependency_name,version FROM lib_dependency WHERE ?1='lib' AND lib_name=?2 ORDER BY 1))),
 'capabilities',json((SELECT json_group_array(json_array(capability_id,title)) FROM (SELECT capability_id,title FROM lib_capability WHERE ?1='lib' AND lib_name=?2 ORDER BY capability_id))),
 'operations',json((SELECT json_group_array(json_array(capability_id,method,path)) FROM (SELECT capability_id,method,path FROM lib_capability_operation WHERE ?1='lib' AND lib_name=?2 ORDER BY capability_id,method,path)))
)
