use eliteforge_agent_core::ToolOperationKind;
use eliteforge_agent_official_plugin_tools_local::local_tool_catalog;

#[test]
fn local_catalog_separates_queries_from_mutations_and_arbitrary_commands() {
    let catalog = local_tool_catalog();
    assert!(
        catalog
            .iter()
            .all(|tool| tool.operation_kind != ToolOperationKind::Unknown)
    );
    for (name, kind) in [
        ("fs_read_text", ToolOperationKind::Query),
        ("fs_write_text", ToolOperationKind::Command),
        ("fs_apply_patch", ToolOperationKind::Command),
        ("command_exec", ToolOperationKind::Command),
        ("terminal_read", ToolOperationKind::Query),
        ("terminal_start", ToolOperationKind::Command),
    ] {
        assert_eq!(
            catalog
                .iter()
                .find(|tool| tool.call_name == name)
                .unwrap()
                .operation_kind,
            kind,
            "{name}"
        );
    }
}

#[test]
fn every_local_tool_ships_its_display_resources_without_host_overlays() {
    let catalog = local_tool_catalog();
    assert_eq!(catalog.len(), 18);
    for descriptor in catalog {
        let resource = descriptor
            .display
            .as_ref()
            .unwrap_or_else(|| panic!("missing resource: {}", descriptor.id));
        assert_eq!(resource.default_locale, "en-US");
        let canonical = (
            descriptor.description.clone(),
            descriptor.input_schema.clone(),
        );
        let zh = descriptor.resolve_display("zh-CN");
        let en = descriptor.resolve_display("en-US");
        assert!(zh.name.chars().any(|c| !c.is_ascii()), "{}", descriptor.id);
        assert!(
            zh.description.chars().any(|c| !c.is_ascii()),
            "{}",
            descriptor.id
        );
        assert!(!en.name.is_empty());
        if descriptor.id == "filesystem.read_text" {
            assert_eq!(
                zh.input_schema["properties"]["path"]["description"],
                "文件路径"
            );
            assert_eq!(
                en.input_schema["properties"]["path"]["description"],
                "File path"
            );
        }
        assert_eq!((descriptor.description, descriptor.input_schema), canonical);
    }
}
