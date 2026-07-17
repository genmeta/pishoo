const DEB_POSTINST: &str = include_str!("../deb/pishoo-common.postinst");
const RPM_PACKAGE_SCRIPT: &str = include_str!("../release/rpm/package.sh");

#[test]
fn linux_packages_create_the_dhttp_group() {
    assert!(DEB_POSTINST.contains("addgroup --system --quiet dhttp"));
    assert!(!DEB_POSTINST.contains("addgroup --system --quiet pishoo"));
    assert!(RPM_PACKAGE_SCRIPT.contains("groupadd --system dhttp"));
    assert!(!RPM_PACKAGE_SCRIPT.contains("groupadd --system pishoo"));
}
