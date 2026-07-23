use pedelec_lib::pedelec_core::ToolRegistry;

#[test]
fn empty_tools_manifest_produces_an_empty_public_registry() {
    let temp = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::load_from_skills_dir(temp.path()).unwrap();
    assert_eq!(registry.tools().count(), 0);
}
